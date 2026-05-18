//! `move-window` and `move-window-here` subcommands: move the focused
//! window into a workspace within a target activity.
//!
//! Two verbs share the entire helper surface, because
//! `move-window-here` is the single-stage degenerate of `move-window`'s
//! two-stage flow with the activity pinned to the currently-active one.
//!
//! ## Why these verbs do not dispatch `MoveWindowToActivity`
//!
//! No such action exists on the compositor fork: in the workspace-as-atom
//! model, activity membership is a workspace-level property. To move a
//! window to a different activity the external tool finds (or asks for)
//! a workspace already in the desired activity and dispatches
//! `Action::MoveWindowToWorkspace` against its id. The trailing-empty
//! workspace of each activity is the conventional landing slot when the
//! user doesn't pick an existing workspace explicitly.
//!
//! ## Why `window_id: None`, `focus: false`, `WorkspaceReferenceArg::Id`
//!
//! - **`window_id: None`** — both verbs operate on the focused window;
//!   there is intentionally no `--window-id <u64>` flag in v1.
//! - **`focus: false`** — the verb is "move," not "move + switch."
//!   `Action::MoveWindowToWorkspace.focus` defaults to `true` upstream,
//!   so the literal `false` here is load-bearing and pinned by unit
//!   tests.
//! - **`WorkspaceReferenceArg::Id(_)`** — focus-drift guard. Resolving
//!   by `Index(_)` or `Name(_)` would risk targeting a different
//!   workspace if focus or workspace ordering changed between the
//!   client snapshot read and the compositor processing the action.
//!
//! ## Two-stage picker UX
//!
//! 1. **Stage 1 (activity).** A single-select picker over the activity
//!    names, with a `« Current activity »` sentinel as the first row so
//!    the user has a single-keystroke shortcut to the focused activity.
//!    Compositor-supplied activity order is preserved otherwise (no
//!    `names_focused_first` reorder — the sentinel already covers that
//!    affordance).
//! 2. **Stage 2 (workspace).** A single-select picker over workspaces
//!    in the chosen activity that live on the focused output. When the
//!    chosen activity is the active one a `« New workspace »` sentinel
//!    is appended so the user can land on the activity's trailing-empty
//!    workspace; for non-active activities the sentinel is omitted (the
//!    compositor's trailing-empty invariant only applies to the active
//!    activity).
//!
//! `move-window-here` collapses the flow to stage 2 only, with the
//! activity pinned to the currently-active one and the new-workspace
//! sentinel always offered.
//!
//! ## Error model
//!
//! - `Action::MoveWindowToWorkspace` reply handling goes through
//!   [`send_expect_handled_or_no_op`], which additionally accepts
//!   [`Response::NoOp(NoOpReason::AlreadyOnTarget)`] as a non-error
//!   outcome — the compositor's durable signal that the focused window
//!   already lived on the target workspace. The dispatcher consumes the
//!   resulting [`HandledOutcome`] and routes
//!   [`HandledOutcome::Handled`] → post-move stderr confirmation,
//!   [`HandledOutcome::NoOp`] → un-annotated `workspace_label`
//!   breadcrumb identical to the eager
//!   [`Stage2ResolutionWithNew::AlreadyCurrent`] /
//!   [`Stage2ResolutionLiteralOnly::AlreadyCurrent`] paths.
//! - Client-side `ActivityNotFound` (named-arg only) is produced by
//!   walking the `Activities` snapshot, mirroring how
//!   `move-workspace` lets the compositor produce it.
//! - Synthetic `MalformedResponse(Server(_))` carriers fire when the
//!   compositor's snapshot violates an invariant we depend on. The four
//!   CLI-internal synthetic strings (not on the wire):
//!   - `"no focused workspace"` — [`focused_workspace`] (via
//!     [`focused_output_name`])
//!   - `"focused workspace has no output"` — [`focused_output_name`]
//!   - `"no active activity"` — [`current_activity`]
//!   - `"trailing-empty workspace expected for active activity"` —
//!     [`dispatch_stage2_with_new`] when the user picks `« New workspace »`
//!     but no trailing-empty workspace is present on the focused output.
//!     The compositor's trailing-empty invariant guarantees the active
//!     activity always has one such workspace; surfacing the violation as
//!     a typed error instead of a panic matches the rest of this
//!     module's synthetic-string discipline.
//!
//!   All use the same rustdoc-discipline pattern as
//!   [`crate::assign_workspace::focused_workspace`].

use std::collections::HashMap;

use anyhow::{Context, Result};
use niri_ipc::{Action, Activity, NoOpReason, Request, Response, Workspace, WorkspaceReferenceArg};

use crate::error::{CliError, MalformedResponseSource};
use crate::ipc::{IpcError, NiriClient};
use crate::ipc_helpers::{
    HandledOutcome, send_expect_activities, send_expect_handled_or_no_op, send_expect_workspaces,
    variant_name,
};
use crate::picker::{NameOutcome, PickerOutcome};

// ---- Stage sentinels -------------------------------------------------------

// Stage 1 carries two sentinel rows (`« Current activity »`,
// `« New activity »`); stage 2 carries one (`« New workspace »`). The
// unicode form is preferred for each, and the underscore-fallback form
// is substituted iff a collision against the user-visible row set is
// detected at composition time. All sentinels are CLI-internal (never
// emitted on the wire) and resolved by strict string equality.
//
// Stage 1's two sentinels are carried as a typed [`Stage1Sentinels`]
// struct so the matched pair travels atomically between composer and
// resolver — same pattern as [`crate::picker::multi_select::SentinelNames`].
// Stage 2 still carries a plain `&'static str` because it only has one
// sentinel.

/// Stage-1 sentinel (preferred unicode form): the `« Current activity »`
/// row that opens stage 2 against the focused activity.
const UNICODE_CURRENT_ACTIVITY: &str = "« Current activity »";

/// Stage-1 sentinel fallback used iff any activity name collides with
/// the unicode form. Selected by [`sentinel_names`] per picker invocation
/// against the live activity name set.
const FALLBACK_CURRENT_ACTIVITY: &str = "__niri_activities_current_activity__";

/// Stage-1 sentinel (preferred unicode form): the `« New activity »`
/// row that prompts the user for a name, creates the activity over IPC,
/// and proceeds to stage 2 against the new activity.
const UNICODE_NEW_ACTIVITY: &str = "« New activity »";

/// Stage-1 sentinel fallback used iff any activity name collides with
/// the unicode form for the new-activity row. Selected by [`sentinel_names`].
const FALLBACK_NEW_ACTIVITY: &str = "__niri_activities_new_activity__";

/// Stage-2 sentinel (preferred unicode form): the `« New workspace »`
/// row that resolves to the active activity's trailing-empty workspace.
const UNICODE_NEW_WORKSPACE: &str = "« New workspace »";

/// Stage-2 sentinel fallback used iff any workspace label collides with
/// the unicode form. Selected by [`workspace_sentinel_names`] per picker
/// invocation against the live workspace label set.
const FALLBACK_NEW_WORKSPACE: &str = "__niri_activities_new_workspace__";

/// Stage-1 sentinel pair produced by [`sentinel_names`] for a given
/// activity-name slice. Both fields are CLI-internal `&'static str`s —
/// the unicode form when no collision is detected, the underscore
/// fallback otherwise. They are chosen independently per row, so a
/// fixture with one colliding name but not the other still ends up
/// using unicode for the non-colliding side.
#[derive(Debug, Clone, Copy)]
struct Stage1Sentinels {
    current: &'static str,
    new_activity: &'static str,
}

// ---- Stage-resolution enums ------------------------------------------------

/// Post-picker resolution for stage 1.
#[derive(Debug)]
enum Stage1Resolution<'a> {
    /// User picked the `« Current activity »` sentinel — open stage 2
    /// against the currently-active activity.
    CurrentActivity,
    /// User picked the `« New activity »` sentinel — prompt for a name,
    /// dispatch `Action::CreateActivity`, refetch `Activities`, then
    /// open stage 2 against the freshly-created activity.
    NewActivity,
    /// User picked a literal activity name.
    Selected(&'a Activity),
    Cancelled,
    /// The stage-1 picker returned a row that was not in the items we
    /// passed — a picker-side contract violation. Propagated as
    /// `MalformedResponse(Server(...))` at the call site.
    Unknown(String),
}

/// Post-picker resolution for stage 2 when the `« New workspace »`
/// sentinel is part of the composed item list (active-activity path).
///
/// The presence of [`Self::NewWorkspace`] is encoded in the type: callers
/// of this variant arm are statically guaranteed to be on the with-new
/// path, eliminating the need for a panic branch on the literal-only
/// path.
#[derive(Debug)]
enum Stage2ResolutionWithNew<'a> {
    NewWorkspace,
    Selected(&'a Workspace),
    /// User picked the row annotated with the ` (current)` suffix — the
    /// focused window already lives in this workspace, so the move is a
    /// no-op. Surfaced as a typed resolution rather than dispatched: the
    /// caller writes a stderr diagnostic and exits 0 without an IPC
    /// round-trip. Carries the workspace id so the caller can render the
    /// un-annotated label in the diagnostic.
    ///
    /// **Why a distinct variant.** `AlreadyCurrent` represents user
    /// intent (clicked the `(current)`-annotated row); [`Self::Unknown`]
    /// represents a picker-contract violation (label not in items).
    /// Collapsing the two would route a contract violation to a
    /// user-gesture path, which is the silent-failure anti-pattern this
    /// module otherwise guards against.
    AlreadyCurrent(u64),
    Cancelled,
    /// The stage-2 picker returned a label that was not in the items we
    /// passed — a picker-side contract violation. Propagated as
    /// `MalformedResponse(Server(...))` at the call site.
    Unknown(String),
}

/// Post-picker resolution for stage 2 on the non-active activity path.
///
/// The `« New workspace »` sentinel is **not** injected into the
/// composed item list on this path (the compositor's trailing-empty
/// invariant only applies to the active activity), so a real picker
/// invocation cannot resolve to [`Self::NewWorkspace`] — fuzzel will
/// never return a row the caller didn't paste in. The variant exists
/// as type-state scaffolding so the resolver's signature is symmetric
/// with [`Stage2ResolutionWithNew`] and the dispatch arm is already
/// wired should a literal-only "new workspace" affordance be added
/// later. Lighting it up requires the compositor to expose an
/// `Action::CreateWorkspace` (or equivalent) so the non-active activity
/// path can mint a landing slot on demand; until that exists the
/// composer deliberately omits the sentinel on this path.
///
/// **Reachability:** in current production code the `NewWorkspace`
/// arm of [`dispatch_stage2_literal_only`] is statically unreachable.
/// It routes to `MalformedResponse(Server(_))` rather than
/// `unreachable!()` so a future regression — composer mistakenly
/// appending the sentinel, fuzzel echoing a synthesised row — fails
/// loudly with exit 65 and a diagnostic stderr line, not SIGABRT.
#[derive(Debug)]
enum Stage2ResolutionLiteralOnly<'a> {
    /// Resolver detected the `« New workspace »` sentinel label.
    /// Currently runtime-unreachable on this path (the composer does
    /// not inject the sentinel); see the enum docs for why the variant
    /// exists regardless.
    NewWorkspace,
    Selected(&'a Workspace),
    /// User picked the row annotated with the ` (current)` suffix — the
    /// focused window already lives in this workspace, so the move is a
    /// no-op. Same rationale as
    /// [`Stage2ResolutionWithNew::AlreadyCurrent`]: surfaced as a typed
    /// resolution to keep user intent and picker-contract violations on
    /// distinct branches.
    AlreadyCurrent(u64),
    Cancelled,
    /// The stage-2 picker returned a label that was not in the items we
    /// passed — a picker-side contract violation. Propagated as
    /// `MalformedResponse(Server(...))` at the call site.
    Unknown(String),
}

// ---- Public entry points ---------------------------------------------------

/// Moves the focused window into the trailing-empty workspace of the
/// activity named `activity_name`.
///
/// **Contract:** fully non-interactive named-arg form. Issues
/// `Activities` then `Workspaces`, resolves the activity by name (client
/// side), picks the trailing-empty workspace on the focused output, and
/// dispatches `MoveWindowToWorkspace`.
///
/// **Zero-case.** Two sub-cases both return `Ok(())` — exit 0:
/// - Named activity has no workspaces on the focused output → writes
///   `"activity '<name>' has no workspaces on the focused output; nothing
///   to move window to"` to stderr.
/// - Named activity has workspaces on the focused output but none are empty
///   (trailing-empty invariant only guarantees an empty slot for the active
///   activity) → writes `"activity '<name>' has no empty workspaces on the
///   focused output; create one or pick an existing workspace via
///   \`move-window\` (no arg)"` to stderr.
///
/// **Returns `Err` when:**
/// - The named activity is not in the `Activities` snapshot →
///   `CliError::ActivityNotFound(name)` (exit 66).
/// - `Activities` or `Workspaces` reply is the wrong variant →
///   `MalformedResponse(WrongVariant { ... })` (exit 65).
/// - No workspace has `is_focused: true` → synthetic
///   `MalformedResponse(Server("no focused workspace"))` (exit 65).
/// - The focused workspace has no output → synthetic
///   `MalformedResponse(Server("focused workspace has no output"))` (exit 65).
/// - Compositor reply variant / server-error handling matches
///   [`send_expect_handled_or_no_op`].
///
/// **`follow`.** When `true`, the focused-window id is captured from the
/// in-scope `Workspaces` snapshot and threaded into the dispatch — see
/// [`decide_window_id_for_dispatch`]. The default (`false`) preserves the
/// pre-`--follow` wire shape (`window_id: None`).
pub(crate) fn run(
    client: &mut dyn NiriClient,
    activity_name: &str,
    follow: bool,
    _overview: bool,
) -> Result<()> {
    let activities = send_expect_activities(client).context("requesting activities")?;
    let activity = activities
        .iter()
        .find(|a| a.name == activity_name)
        .ok_or_else(|| CliError::ActivityNotFound(activity_name.to_owned()))?;
    let activity_id = activity.id;

    let workspaces = send_expect_workspaces(client).context("requesting workspaces")?;
    let focused_output = focused_output_name(&workspaces)?;
    let filtered =
        workspaces_in_activity_on_focused_output(&workspaces, activity_id, focused_output);

    if filtered.is_empty() {
        eprintln!(
            "niri-activities: activity '{activity_name}' has no workspaces on the focused output; nothing to move window to"
        );
        return Ok(());
    }
    let Some(ws) = trailing_empty_workspace(&filtered) else {
        eprintln!(
            "niri-activities: activity '{activity_name}' has no empty workspaces on the focused output; create one or pick an existing workspace via `move-window` (no arg)"
        );
        return Ok(());
    };
    let ws_id = ws.id;
    let window_id_for_dispatch = decide_window_id_for_dispatch(follow, &workspaces);
    handle_move_outcome(
        dispatch_move(client, ws_id, window_id_for_dispatch)?,
        ws_id,
        activity_name,
        &workspaces,
    )?;
    Ok(())
}

/// Renders the post-`dispatch_move` stderr line for a given
/// [`HandledOutcome`]. Threaded through every `dispatch_move` call site so
/// each one routes the [`HandledOutcome::Handled`] path through
/// [`print_move_confirmation`] and the [`HandledOutcome::NoOp`] path
/// through the un-annotated `workspace_label` breadcrumb identical to the
/// one rendered by the eager [`Stage2ResolutionWithNew::AlreadyCurrent`]
/// arm.
///
/// For the [`NoOpReason::AlreadyOnTarget`] case, the lookup key is the
/// `workspace_id` carried in the [`NoOpReason`] payload, not the local
/// `ws_id` we dispatched against — the wire payload is the authoritative
/// signal of which workspace the focused window already lived on. In
/// well-behaved compositor replies the two are equal, but the wire value
/// is the contract.
///
/// **Error return.** `NoOpReason` is `#[non_exhaustive]`; the catch-all
/// arm routes to `MalformedResponse(Server(_))` rather than silently
/// treating an unknown future variant as a benign no-op. This forces a
/// CLI rev whenever the compositor adds a `NoOpReason` variant — the
/// correct friction for maintaining the contract. When a new variant
/// warrants distinct user-facing text, hoist it out of the catch-all
/// explicitly (matching its name above the `_` arm) and return `Ok(())`.
fn handle_move_outcome(
    outcome: HandledOutcome,
    ws_id: u64,
    activity_name: &str,
    workspaces: &[Workspace],
) -> Result<()> {
    match outcome {
        HandledOutcome::Handled => print_move_confirmation(ws_id, activity_name),
        HandledOutcome::NoOp(NoOpReason::AlreadyOnTarget {
            workspace_id: payload_id,
        }) => {
            if payload_id != ws_id {
                eprintln!(
                    "niri-activities: warning: compositor reported AlreadyOnTarget for ws {payload_id} but we dispatched against ws {ws_id}"
                );
            }
            print_already_current_breadcrumb(payload_id, workspaces);
        }
        // `NoOpReason` is `#[non_exhaustive]`. An unknown future variant
        // routes to `MalformedResponse(Server)` so contract drift surfaces
        // as exit 65 rather than a silent success. Hoist a known variant
        // explicitly above to give it a distinct user-facing path.
        HandledOutcome::NoOp(other) => {
            return Err(
                CliError::MalformedResponse(MalformedResponseSource::Server(format!(
                    "unexpected NoOpReason variant: {other:?}"
                )))
                .into(),
            );
        }
    }
    Ok(())
}

/// Returns the breadcrumb message shown when the move resolves to a no-op
/// (either via eager picker shortcut or via the compositor's
/// `Response::NoOp(AlreadyOnTarget)` reply). The label is looked up against
/// the workspace snapshot we already have in hand; if the id is absent, fall
/// back to a bare `id <n>` form so the user still gets a useful diagnostic.
///
/// Extracted as a pure formatter so tests can assert the rendered string
/// without stderr-capture machinery.
fn format_already_current_breadcrumb(ws_id: u64, workspaces: &[Workspace]) -> String {
    let label = workspaces
        .iter()
        .find(|w| w.id == ws_id)
        .map(workspace_label)
        .unwrap_or_else(|| format!("id {ws_id}"));
    format!("niri-activities: focused window is already in workspace {label}; nothing to move")
}

/// Prints the breadcrumb returned by [`format_already_current_breadcrumb`].
fn print_already_current_breadcrumb(ws_id: u64, workspaces: &[Workspace]) {
    eprintln!("{}", format_already_current_breadcrumb(ws_id, workspaces));
}

/// Two-stage picker form for `move-window`.
///
/// **Contract:**
/// 1. Issues `Request::Activities`. If the list is empty, writes a
///    single-line stderr diagnostic and returns `Ok(())` — the
///    stage-1 picker is never spawned.
/// 2. Opens stage 1 (activity picker) with a `« Current activity »`
///    sentinel as the first row and a `« New activity »` sentinel as
///    the last row. Cancellation returns `Ok(())`.
/// 3. If the user picked `« New activity »`, prompts for a name via
///    `prompt_name_fn`, dispatches `Action::CreateActivity { name }`,
///    refetches `Activities`, and proceeds to stage 2 against the new
///    activity. Empty-name input is rejected client-side as
///    [`CliError::Usage`] (exit 64) **before** any IPC is issued.
/// 4. Otherwise dispatches to [`dispatch_stage2_with_new`] (active
///    activity, or `« Current activity »` sentinel, or freshly-created
///    activity that lands as active) or [`dispatch_stage2_literal_only`]
///    (non-active activity).
///    The dispatch helpers issue `Request::Workspaces`, filter to
///    workspaces in the chosen activity on the focused output, and
///    manage zero-case diagnostics and sentinel composition.
/// 5. **Zero-case:** on the literal-only path, when the filtered
///    workspace list is empty, the helper writes a stderr diagnostic and
///    returns `Ok(())` — stage 2 is **not** spawned. On the with-new
///    path the `« New workspace »` sentinel covers the empty case so no
///    short-circuit fires there.
/// 6. Opens stage 2 (workspace picker). `« New workspace »` is only
///    offered on the with-new path. Cancellation returns `Ok(())`.
/// 7. Dispatches `Action::MoveWindowToWorkspace { window_id: None,
///    reference: Id(ws.id), focus: false }`.
///
/// The `pick` parameter is a closure (not `FnOnce`) because it is
/// called twice — once per stage. The `prompt_name_fn` parameter fires
/// at most once, only when the user picks the `« New activity »`
/// sentinel. Production wiring passes [`crate::picker::pick_one`] and
/// [`crate::picker::prompt_name`] respectively.
///
/// **`follow`.** When `true`, the focused-window id is captured from the
/// in-scope `Workspaces` snapshot (fetched in stage 2) and threaded into
/// the dispatch — see [`decide_window_id_for_dispatch`]. The default
/// (`false`) preserves the pre-`--follow` wire shape (`window_id: None`).
pub(crate) fn run_picker<F, P>(
    client: &mut dyn NiriClient,
    pick: F,
    prompt_name_fn: P,
    follow: bool,
    _overview: bool,
) -> Result<()>
where
    F: Fn(&str, &[String]) -> Result<PickerOutcome, CliError>,
    P: FnOnce(&str) -> Result<NameOutcome, CliError>,
{
    let activities = send_expect_activities(client).context("requesting activities")?;
    if activities.is_empty() {
        eprintln!("niri-activities: no activities configured; nothing to move window to");
        return Ok(());
    }

    let activity_name_refs: Vec<&str> = activities.iter().map(|a| a.name.as_str()).collect();
    let stage1_sentinels = sentinel_names(&activity_name_refs);
    let stage1_items = compose_stage1_items(&activities, &stage1_sentinels);
    let stage1_picked = pick("Move window to activity:", &stage1_items)?;

    let activity_names: HashMap<u64, String> =
        activities.iter().map(|a| (a.id, a.name.clone())).collect();

    match resolve_stage1(stage1_picked, &stage1_sentinels, &activities) {
        Stage1Resolution::Cancelled => Ok(()),
        Stage1Resolution::CurrentActivity => {
            let active = current_activity(&activities)?;
            let name = active.name.clone();
            dispatch_stage2_with_new(client, active.id, &name, &activity_names, &pick, follow)
        }
        // Active activity → with-new path (compositor trailing-empty
        // invariant guarantees a landing slot for « New workspace »).
        // Non-active → literal-only path (no auto-materialised
        // trailing-empty, so no sentinel offered).
        Stage1Resolution::Selected(activity) if activity.is_active => {
            let name = activity.name.clone();
            dispatch_stage2_with_new(client, activity.id, &name, &activity_names, &pick, follow)
        }
        Stage1Resolution::Selected(activity) => {
            let name = activity.name.clone();
            dispatch_stage2_literal_only(client, activity.id, &name, &activity_names, &pick, follow)
        }
        Stage1Resolution::NewActivity => {
            // Prompt for a name in the same fuzzel-shaped UI. Pre-reject
            // empty-Enter client-side as CliError::Usage so the user
            // sees an actionable diagnostic without an IPC round-trip.
            let outcome = prompt_name_fn("new activity name:")?;
            let new_name = match outcome {
                NameOutcome::Cancelled => return Ok(()),
                NameOutcome::Unnamed => {
                    return Err(
                        CliError::Usage("new activity name must not be empty".to_owned()).into(),
                    );
                }
                NameOutcome::Typed(s) => s,
            };
            create_activity_via_ipc(client, &new_name)?;
            // Refetch Activities so the new activity's id is in scope.
            // The compositor mints the id when CreateActivity is
            // handled; the post-create snapshot is the only authority
            // for that id from the CLI's perspective.
            let after =
                send_expect_activities(client).context("requesting activities after create")?;
            let new_activity = after.iter().find(|a| a.name == new_name).ok_or_else(|| {
                CliError::MalformedResponse(MalformedResponseSource::Server(
                    "newly-created activity not present in post-create Activities snapshot"
                        .to_owned(),
                ))
            })?;
            // Rebuild the activity-name map from the post-create
            // snapshot so the new activity's id is in scope for the
            // stage-2 label annotations.
            let activity_names_after: HashMap<u64, String> =
                after.iter().map(|a| (a.id, a.name.clone())).collect();
            if new_activity.is_active {
                dispatch_stage2_with_new(
                    client,
                    new_activity.id,
                    &new_name,
                    &activity_names_after,
                    &pick,
                    follow,
                )
                .context("dispatching stage 2 against newly-created activity")
            } else {
                dispatch_stage2_literal_only(
                    client,
                    new_activity.id,
                    &new_name,
                    &activity_names_after,
                    &pick,
                    follow,
                )
                .context("dispatching stage 2 against newly-created activity")
            }
        }
        Stage1Resolution::Unknown(row) => Err(CliError::MalformedResponse(
            MalformedResponseSource::Server(format!(
                "stage-1 picker returned row not in items: {row:?}"
            )),
        )
        .into()),
    }
}

/// Dispatches `Action::CreateActivity { name }` and maps the compositor's
/// wire-error matrix verbatim to the same `CliError` variants used by
/// [`crate::create::run`]:
///
/// - `CreateActivityError::DuplicateName` display → [`CliError::CantCreate`].
/// - `CreateActivityError::EmptyName` display → [`CliError::Usage`].
/// - other `IpcError::Server(_)` → [`MalformedResponseSource::Server`].
///
/// **Why not [`crate::create::run`]:** the stage-1 new-activity flow
/// is a multi-IPC pipeline (CreateActivity → re-fetch Activities →
/// dispatch_stage2_*), so this is a step of that pipeline rather than a
/// terminal verb invocation. Open-coding the dispatch here keeps the
/// pipeline's error-context layer (`"creating activity from move-window
/// picker"`) attachable without misroute-by-strings between
/// `creating activity` (the standalone verb's context) and the picker's
/// context.
///
/// **Synthetic-string discipline.** The `"creating activity from
/// move-window picker"` context label is a CLI-internal string — it is
/// not on the wire. Same audit-skip discipline as
/// [`focused_workspace`]'s `"no focused workspace"`.
fn create_activity_via_ipc(client: &mut dyn NiriClient, name: &str) -> Result<()> {
    let req = Request::Action(Action::CreateActivity {
        name: name.to_owned(),
    });
    let result: anyhow::Result<()> = match client.send(req) {
        Ok(Response::Handled) => Ok(()),
        Ok(other) => Err(
            CliError::MalformedResponse(MalformedResponseSource::WrongVariant {
                expected: "Response::Handled",
                got: variant_name(&other).into(),
            })
            .into(),
        ),
        Err(IpcError::Server(msg)) if msg == "activity name already exists" => {
            Err(CliError::CantCreate(format!("activity \"{name}\" already exists")).into())
        }
        Err(IpcError::Server(msg)) if msg == "activity name must not be empty" => {
            Err(CliError::Usage("activity name must not be empty".to_owned()).into())
        }
        Err(other) => Err(CliError::from(other).into()),
    };
    result.context("creating activity from move-window picker")
}

/// Single-stage picker form for `move-window-here`.
///
/// **Contract:** opens stage 2 against the currently-active activity
/// only. The `« New workspace »` sentinel is always offered (the active
/// activity's trailing-empty workspace is the canonical landing slot
/// guaranteed by the compositor invariant). Cancellation returns
/// `Ok(())`.
///
/// **Returns `Err` when:**
/// - No active activity is present → synthetic
///   `MalformedResponse(Server("no active activity"))` (exit 65).
/// - `Activities` or `Workspaces` reply is the wrong variant →
///   `MalformedResponse(WrongVariant { ... })` (exit 65).
/// - No focused workspace / focused workspace has no output — same
///   synthetics as [`run`].
/// - Reply / variant handling matches [`send_expect_handled_or_no_op`].
///
/// **`follow`.** When `true`, the focused-window id is captured from the
/// in-scope `Workspaces` snapshot and threaded into the dispatch — see
/// [`decide_window_id_for_dispatch`]. The default (`false`) preserves the
/// pre-`--follow` wire shape (`window_id: None`).
pub(crate) fn run_here_picker<F>(
    client: &mut dyn NiriClient,
    pick: F,
    follow: bool,
    _overview: bool,
) -> Result<()>
where
    F: FnOnce(&str, &[String]) -> Result<PickerOutcome, CliError>,
{
    let activities = send_expect_activities(client).context("requesting activities")?;
    let active = current_activity(&activities)?;
    let activity_id = active.id;
    let activity_name = active.name.clone();
    let activity_names: HashMap<u64, String> =
        activities.iter().map(|a| (a.id, a.name.clone())).collect();
    dispatch_stage2_with_new(
        client,
        activity_id,
        &activity_name,
        &activity_names,
        pick,
        follow,
    )
}

// ---- Stage-2 dispatch (with-new vs literal-only) ---------------------------

/// Runs stage 2 against a pinned `target_activity_id` on the with-new
/// path: the `« New workspace »` sentinel is always appended to the
/// composed item list. Selected by callers when the target activity is
/// the currently-active one (compositor trailing-empty invariant
/// guarantees a landing slot exists).
///
/// `target_activity_name` is the user-visible name of the target
/// activity, threaded through purely for the post-move stderr
/// confirmation. It is not used by the dispatch path itself; the
/// id-based `target_activity_id` is what selects workspaces.
///
/// Sentinel resolution always succeeds when the compositor's
/// trailing-empty invariant holds — the `MalformedResponse(Server(
/// "trailing-empty workspace expected for active activity"))` arm in
/// `NewWorkspace` is a defensive surface for invariant violation, not
/// a normal path.
///
/// **Invariant breach.** The literal `"trailing-empty workspace expected
/// for active activity"` is a **CLI-internal** value — it is **not**
/// emitted on the wire by the niri compositor. A future grep that audits
/// compositor wire-string matches must skip this site. It routes through
/// `MalformedResponse(Server)` → exit 65 via the same
/// `IpcError::Server → MalformedResponseSource::Server` `Display` path
/// as [`focused_workspace`].
///
/// The `pick` parameter accepts an `FnOnce` because stage 2 fires only
/// once per invocation; `run_picker` passes its `Fn` closure here by
/// reference.
fn dispatch_stage2_with_new<F>(
    client: &mut dyn NiriClient,
    target_activity_id: u64,
    target_activity_name: &str,
    activity_names: &HashMap<u64, String>,
    pick: F,
    follow: bool,
) -> Result<()>
where
    F: FnOnce(&str, &[String]) -> Result<PickerOutcome, CliError>,
{
    debug_assert!(
        activity_names.contains_key(&target_activity_id),
        "target activity id not in name map — caller construction drift",
    );
    let workspaces = send_expect_workspaces(client).context("requesting workspaces")?;
    let focused_output = focused_output_name(&workspaces)?;
    // Ordering invariant: read the focused workspace id AFTER
    // `focused_output_name` so the `no focused workspace` / `focused
    // workspace has no output` synthetic errors fire first; this
    // computation is best-effort (None is a legitimate "no focus").
    let focused_workspace_id = focused_workspace(&workspaces).ok().map(|w| w.id);
    let mut filtered =
        workspaces_in_activity_on_focused_output(&workspaces, target_activity_id, focused_output);
    sort_for_picker(&mut filtered);

    let workspace_labels: Vec<String> = filtered.iter().map(|w| workspace_label(w)).collect();
    let workspace_label_refs: Vec<&str> = workspace_labels.iter().map(String::as_str).collect();
    let stage2_sentinel = workspace_sentinel_names(&workspace_label_refs);
    let stage2_items = compose_stage2_items_with_new(
        &filtered,
        focused_workspace_id,
        stage2_sentinel,
        target_activity_id,
        activity_names,
    );

    let stage2_picked = pick("Move window to workspace:", &stage2_items)?;
    match resolve_stage2_with_new(
        stage2_picked,
        stage2_sentinel,
        &filtered,
        focused_workspace_id,
        target_activity_id,
        activity_names,
    ) {
        Stage2ResolutionWithNew::Cancelled => Ok(()),
        Stage2ResolutionWithNew::Selected(ws) => {
            let ws_id = ws.id;
            let window_id_for_dispatch = decide_window_id_for_dispatch(follow, &workspaces);
            let outcome = dispatch_move(client, ws_id, window_id_for_dispatch)?;
            handle_move_outcome(outcome, ws_id, target_activity_name, &workspaces)?;
            Ok(())
        }
        Stage2ResolutionWithNew::AlreadyCurrent(ws_id) => {
            // Eager pre-dispatch breadcrumb when the user picked the
            // `(current)`-annotated row. Same un-annotated label as the
            // post-dispatch `Response::NoOp` arm: the message already
            // says "already in workspace", so a trailing `(current)`
            // would be redundant, and membership disclosure
            // (`[sticky]`, `[also in: …]`) is elided — the user just
            // clicked this row, they know where the window lives.
            print_already_current_breadcrumb(ws_id, &workspaces);
            Ok(())
        }
        Stage2ResolutionWithNew::Unknown(label) => Err(CliError::MalformedResponse(
            MalformedResponseSource::Server(format!(
                "stage-2 picker returned label not in items: {label:?}"
            )),
        )
        .into()),
        Stage2ResolutionWithNew::NewWorkspace => {
            let Some(ws) = trailing_empty_workspace(&filtered) else {
                return Err(CliError::MalformedResponse(MalformedResponseSource::Server(
                    "trailing-empty workspace expected for active activity".to_owned(),
                ))
                .into());
            };
            let ws_id = ws.id;
            let window_id_for_dispatch = decide_window_id_for_dispatch(follow, &workspaces);
            let outcome = dispatch_move(client, ws_id, window_id_for_dispatch)?;
            handle_move_outcome(outcome, ws_id, target_activity_name, &workspaces)?;
            Ok(())
        }
    }
}

/// Runs stage 2 against a pinned `target_activity_id` on the literal-only
/// path: the `« New workspace »` sentinel is structurally absent from the
/// composed item list. Selected by callers when the target activity is a
/// non-active one (trailing-empty invariant does not apply, so the
/// sentinel has no canonical landing slot to resolve against).
///
/// **Zero-case.** When the filtered workspace list is empty, the stage-2
/// picker is **not** spawned: a stderr diagnostic is written and `Ok(())`
/// is returned. This short-circuit is structurally pinned to the
/// literal-only path because the with-new path's sentinel covers the
/// zero-case affordance.
fn dispatch_stage2_literal_only<F>(
    client: &mut dyn NiriClient,
    target_activity_id: u64,
    target_activity_name: &str,
    activity_names: &HashMap<u64, String>,
    pick: F,
    follow: bool,
) -> Result<()>
where
    F: FnOnce(&str, &[String]) -> Result<PickerOutcome, CliError>,
{
    debug_assert!(
        activity_names.contains_key(&target_activity_id),
        "target activity id not in name map — caller construction drift",
    );
    let workspaces = send_expect_workspaces(client).context("requesting workspaces")?;
    let focused_output = focused_output_name(&workspaces)?;
    // Ordering invariant: read the focused workspace id AFTER
    // `focused_output_name` so the synthetic errors fire first. See
    // `dispatch_stage2_with_new` for the matching rationale.
    let focused_workspace_id = focused_workspace(&workspaces).ok().map(|w| w.id);
    let mut filtered =
        workspaces_in_activity_on_focused_output(&workspaces, target_activity_id, focused_output);
    sort_for_picker(&mut filtered);

    if filtered.is_empty() {
        eprintln!(
            "niri-activities: activity '{target_activity_name}' has no workspaces on the focused output; nothing to move window to"
        );
        return Ok(());
    }

    let stage2_items = compose_stage2_items_literal_only(
        &filtered,
        focused_workspace_id,
        target_activity_id,
        activity_names,
    );
    // Compute the sentinel against the live label set even though the
    // literal-only composer does not inject it. This keeps the resolver
    // call shape symmetric with `dispatch_stage2_with_new` and means
    // the (statically unreachable) `NewWorkspace` arm below is the
    // single place a regression — composer mistakenly appending the
    // sentinel, fuzzel synthesising a row — would surface.
    let labels: Vec<&str> = stage2_items.iter().map(String::as_str).collect();
    let stage2_sentinel = workspace_sentinel_names(&labels);

    let stage2_picked = pick("Move window to workspace:", &stage2_items)?;
    match resolve_stage2_literal_only(
        stage2_picked,
        stage2_sentinel,
        &filtered,
        focused_workspace_id,
        target_activity_id,
        activity_names,
    ) {
        Stage2ResolutionLiteralOnly::Cancelled => Ok(()),
        Stage2ResolutionLiteralOnly::Selected(ws) => {
            let ws_id = ws.id;
            let window_id_for_dispatch = decide_window_id_for_dispatch(follow, &workspaces);
            let outcome = dispatch_move(client, ws_id, window_id_for_dispatch)?;
            handle_move_outcome(outcome, ws_id, target_activity_name, &workspaces)?;
            Ok(())
        }
        Stage2ResolutionLiteralOnly::AlreadyCurrent(ws_id) => {
            // See dispatch_stage2_with_new for the rationale on rendering the un-annotated label here.
            print_already_current_breadcrumb(ws_id, &workspaces);
            Ok(())
        }
        Stage2ResolutionLiteralOnly::Unknown(label) => Err(CliError::MalformedResponse(
            MalformedResponseSource::Server(format!(
                "stage-2 picker returned label not in items: {label:?}"
            )),
        )
        .into()),
        // Statically unreachable on the literal-only path: the composer
        // never appends the sentinel. Routing through
        // `MalformedResponse(Server)` rather than `unreachable!()` means
        // a future regression (composer change, fuzzel synthesising a
        // row, picker contract drift) surfaces as exit 65 with a
        // diagnostic stderr line, not SIGABRT.
        //
        // **Synthetic-string discipline.** The literal
        // `"stage-2 literal-only path produced new-workspace sentinel"`
        // is a CLI-internal value — it is **not** emitted on the wire
        // by the niri compositor. A future grep that audits compositor
        // wire-string matches must skip this site. Same pattern as
        // [`focused_workspace`]'s `"no focused workspace"`.
        Stage2ResolutionLiteralOnly::NewWorkspace => {
            Err(CliError::MalformedResponse(MalformedResponseSource::Server(
                "stage-2 literal-only path produced new-workspace sentinel".to_owned(),
            ))
            .into())
        }
    }
}

/// Dispatches the `MoveWindowToWorkspace` action against `ws_id` and
/// returns the compositor's reply classification.
///
/// `focus: false` and `reference: Id(ws_id)` are load-bearing — see
/// module docs for the rationale. `window_id` is now caller-supplied:
/// the default `None` path (operate on the focused window) and the
/// `Some(captured_id)` path used under `--follow` (so the post-move
/// `FocusWindow` step has an authoritative id even if focus drifts
/// between the snapshot read and the compositor processing the move).
/// The IPC error is wrapped with `.context("moving window to
/// workspace")` so the operation surfaces in the stderr chain.
///
/// **Why [`HandledOutcome`] and not `()`.** The compositor's
/// `MoveWindowToWorkspace` handler may reply with
/// [`Response::NoOp(NoOpReason::AlreadyOnTarget)`] when the focused
/// window already lives on the target workspace — a durable signal that
/// the action's preconditions were met but no state change was needed.
/// Routing through [`send_expect_handled_or_no_op`] lets the caller
/// distinguish the state-changing [`HandledOutcome::Handled`] path
/// (which renders the post-move confirmation) from the
/// [`HandledOutcome::NoOp`] path (which renders the already-current
/// breadcrumb). Other reply variants surface as
/// [`MalformedResponseSource::WrongVariant`] via the helper's contract.
fn dispatch_move(
    client: &mut dyn NiriClient,
    ws_id: u64,
    window_id: Option<u64>,
) -> Result<HandledOutcome> {
    let req = Request::Action(Action::MoveWindowToWorkspace {
        window_id,
        reference: WorkspaceReferenceArg::Id(ws_id),
        focus: false,
    });
    send_expect_handled_or_no_op(client, req).context("moving window to workspace")
}

/// Returns the `active_window_id` of the focused workspace, or `None`
/// when no workspace is focused OR the focused workspace has no active
/// window.
///
/// **Why pure.** The two "cannot capture" cases collapse to a single
/// `None` because the caller treats them identically: emit a stderr
/// fallback diagnostic and dispatch with `window_id: None`. The
/// "no focused workspace at all" condition is already surfaced as
/// [`CliError::MalformedResponse(Server("no focused workspace"))`] by
/// [`focused_workspace`] / [`focused_output_name`], which fire before
/// this helper in every dispatcher; if execution reaches here, the
/// focused-workspace probe has already passed, so this helper's `None`
/// strictly means "focused workspace has no active window."
///
/// **No new IPC call.** Reads from the `Workspaces` snapshot already in
/// scope at the dispatch site.
fn capture_focused_window_id(workspaces: &[Workspace]) -> Option<u64> {
    focused_workspace(workspaces).ok()?.active_window_id
}

/// Decides what to pass as `Action::MoveWindowToWorkspace.window_id`
/// based on the `--follow` flag and the focused-window snapshot.
///
/// - `follow: false` → `None` (default behavior; compositor operates on
///   the focused window at dispatch time).
/// - `follow: true` and a focused window exists → `Some(captured_id)`.
/// - `follow: true` and no window can be captured → `None` plus a
///   stderr fallback diagnostic noting that the post-follow window
///   refocus will be skipped. The user's primary intent (move) is
///   preserved; the partial-fulfillment is surfaced via stderr.
///
/// **Why a helper.** The decision is duplicated across four
/// `dispatch_move` call sites, each reading against a different in-scope
/// `workspaces` binding. Pulling the eprintln + decision into one place
/// keeps the call sites three-liners and pins the fallback message in a
/// single location.
///
/// **Synthetic-string discipline.** The eprintln literal is a
/// CLI-internal diagnostic — it is **not** emitted on the wire by the
/// niri compositor. A future grep that audits compositor wire-string
/// matches must skip this site. Same audit-skip discipline as
/// [`focused_workspace`]'s `"no focused workspace"`.
fn decide_window_id_for_dispatch(follow: bool, workspaces: &[Workspace]) -> Option<u64> {
    if !follow {
        return None;
    }
    let captured = capture_focused_window_id(workspaces);
    if captured.is_none() {
        eprintln!(
            "niri-activities: --follow set but no focused window to capture; \
             dispatching move with compositor-resolved window (no window-id \
             refocus will fire after follow)"
        );
    }
    captured
}

/// Renders the post-move confirmation line shown on stderr after a
/// successful `MoveWindowToWorkspace` dispatch. Pure helper so the line
/// shape is unit-testable.
///
/// The window id is intentionally omitted: `dispatch_move` uses
/// `window_id: None` (operates on the focused window), so there is a
/// snapshot-vs-dispatch race between the `Workspaces` snapshot read and
/// the compositor processing the action. The user knows visually which
/// window moved; the workspace id and activity name are sufficient to
/// confirm the destination.
fn format_move_confirmation(ws_id: u64, activity_name: &str) -> String {
    format!(
        "niri-activities: moved focused window to workspace {ws_id} in activity '{activity_name}'"
    )
}

/// Writes [`format_move_confirmation`]'s output to stderr.
///
/// Called on every successful `dispatch_move`. Always prints; no
/// None-skip path.
fn print_move_confirmation(ws_id: u64, activity_name: &str) {
    eprintln!("{}", format_move_confirmation(ws_id, activity_name));
}

// ---- Pure helpers ----------------------------------------------------------

/// Returns the workspace whose `is_focused` flag is `true`, or
/// `MalformedResponse(Server("no focused workspace"))` when no such
/// workspace exists.
///
/// **Synthetic-string discipline.** The literal `"no focused workspace"`
/// is a **CLI-internal** value — it is **not** emitted on the wire by
/// the niri compositor. A future grep that audits compositor wire-string
/// matches must skip this site. Chosen for human-readable diagnostics
/// via the existing `IpcError::Server → MalformedResponseSource::Server`
/// `Display` path.
fn focused_workspace(workspaces: &[Workspace]) -> Result<&Workspace, CliError> {
    workspaces.iter().find(|w| w.is_focused).ok_or_else(|| {
        CliError::MalformedResponse(MalformedResponseSource::Server(
            "no focused workspace".to_owned(),
        ))
    })
}

/// Returns the `output` of the focused workspace, or
/// `MalformedResponse(Server("focused workspace has no output"))` when
/// the focused workspace's `output` field is `None`.
///
/// **Synthetic-string discipline.** Same as [`focused_workspace`] —
/// CLI-internal, not on the wire.
fn focused_output_name(workspaces: &[Workspace]) -> Result<&str, CliError> {
    let ws = focused_workspace(workspaces)?;
    ws.output.as_deref().ok_or_else(|| {
        CliError::MalformedResponse(MalformedResponseSource::Server(
            "focused workspace has no output".to_owned(),
        ))
    })
}

/// Returns a reference to the currently-active activity, or
/// `MalformedResponse(Server("no active activity"))` when none has
/// `is_active: true`.
///
/// **Synthetic-string discipline.** Same as [`focused_workspace`] —
/// CLI-internal, not on the wire. Defensive: the compositor invariant
/// is that exactly one activity is active at a time, but a
/// hand-constructed test snapshot or a future protocol drift could
/// violate that.
fn current_activity(activities: &[Activity]) -> Result<&Activity, CliError> {
    activities.iter().find(|a| a.is_active).ok_or_else(|| {
        CliError::MalformedResponse(MalformedResponseSource::Server(
            "no active activity".to_owned(),
        ))
    })
}

/// Filters `workspaces` to those that belong to `activity_id` and live
/// on `focused_output`. Compositor-supplied order is preserved (we walk
/// the slice; no sort). Callers that want a deterministic picker order
/// run [`sort_for_picker`] on the result before composing labels.
fn workspaces_in_activity_on_focused_output<'a>(
    workspaces: &'a [Workspace],
    activity_id: u64,
    focused_output: &str,
) -> Vec<&'a Workspace> {
    workspaces
        .iter()
        .filter(|w| {
            w.activities.contains(&activity_id) && w.output.as_deref() == Some(focused_output)
        })
        .collect()
}

/// Sorts a filtered workspace list into the deterministic order shown to
/// the user by the stage-2 picker:
///
/// 1. Active-activity workspaces first (`is_in_active_activity == true`),
///    then hidden-activity workspaces.
/// 2. Within each bucket, ascending `idx` (active rows) / `id` (hidden
///    rows), with `id` as the final tiebreaker so two hidden rows with
///    the same nominal `idx` keep a stable order.
///
/// `(!is_in_active_activity, idx, id)` is the resulting comparator — the
/// negated bool sorts `false` (active) before `true` (hidden). Compositor-
/// supplied order is intentionally NOT preserved: the compositor's snapshot
/// can interleave hidden and active workspaces in any order, which produces
/// jumpy picker rows from the user's perspective. A stable, predictable
/// order is the picker-UX contract.
fn sort_for_picker(workspaces: &mut Vec<&Workspace>) {
    workspaces.sort_by_key(|w| (!w.is_in_active_activity, w.idx, w.id));
}

/// Returns the trailing-empty workspace from `filtered`: the one with
/// the highest `idx` whose `active_window_id` is `None`. Returns `None`
/// when every workspace in the slice has at least one window.
///
/// The compositor's trailing-empty invariant guarantees this exists for
/// the active activity; for non-active activities the result is
/// best-effort and the caller treats `None` as a legitimate "nothing to
/// move to."
fn trailing_empty_workspace<'a>(filtered: &[&'a Workspace]) -> Option<&'a Workspace> {
    filtered
        .iter()
        .filter(|w| w.active_window_id.is_none())
        .max_by_key(|w| w.idx)
        .copied()
}

/// Returns the stage-1 sentinel pair guaranteed not to collide with any
/// element of `activity_names`. Each row prefers its unicode form and
/// falls back to its underscore form independently — a fixture with one
/// colliding name does not force both sentinels to use fallbacks.
fn sentinel_names(activity_names: &[&str]) -> Stage1Sentinels {
    let current = if activity_names.contains(&UNICODE_CURRENT_ACTIVITY) {
        FALLBACK_CURRENT_ACTIVITY
    } else {
        UNICODE_CURRENT_ACTIVITY
    };
    let new_activity = if activity_names.contains(&UNICODE_NEW_ACTIVITY) {
        FALLBACK_NEW_ACTIVITY
    } else {
        UNICODE_NEW_ACTIVITY
    };
    Stage1Sentinels {
        current,
        new_activity,
    }
}

/// Returns the stage-2 sentinel guaranteed not to collide with any element
/// of `workspace_labels`. Prefers the unicode form; substitutes the
/// underscore fallback iff a collision would occur.
fn workspace_sentinel_names(workspace_labels: &[&str]) -> &'static str {
    if workspace_labels.contains(&UNICODE_NEW_WORKSPACE) {
        FALLBACK_NEW_WORKSPACE
    } else {
        UNICODE_NEW_WORKSPACE
    }
}

/// Composes the stage-1 item list: `« Current activity »` first
/// (sentinels.current), then compositor-supplied activity names in
/// their original order, then `« New activity »` last
/// (sentinels.new_activity).
///
/// **Ordering invariant.** Sentinels bookend the list. The activity
/// names are **never** reshuffled by `names_focused_first` — the
/// `« Current activity »` row already covers the focused-activity
/// shortcut.
fn compose_stage1_items(activities: &[Activity], sentinels: &Stage1Sentinels) -> Vec<String> {
    let mut out = Vec::with_capacity(activities.len() + 2);
    out.push(sentinels.current.to_owned());
    for a in activities {
        out.push(a.name.clone());
    }
    out.push(sentinels.new_activity.to_owned());
    out
}

/// Composes the stage-2 item list for the with-new path: workspace
/// labels in compositor order, then `« New workspace »` (or its
/// fallback) appended unconditionally.
///
/// **Ordering invariant.** Workspace labels preserve the compositor-
/// supplied order of the `workspaces` slice (no sort). The sentinel is
/// **always** the last row. Both invariants are load-bearing for
/// `resolve_stage2_with_new`: it walks the same slice to match labels,
/// and the sentinel is identified by strict string equality (not
/// position).
fn compose_stage2_items_with_new(
    workspaces: &[&Workspace],
    focused_workspace_id: Option<u64>,
    sentinel: &str,
    target_activity_id: u64,
    activity_names: &HashMap<u64, String>,
) -> Vec<String> {
    let mut out: Vec<String> = workspaces
        .iter()
        .map(|w| {
            workspace_label_with_annotations(
                w,
                target_activity_id,
                focused_workspace_id == Some(w.id),
                activity_names,
            )
        })
        .collect();
    out.push(sentinel.to_owned());
    out
}

/// Composes the stage-2 item list for the literal-only path: workspace
/// labels in compositor order, with no sentinel appended.
///
/// **Ordering invariant.** Workspace labels preserve the compositor-
/// supplied order of the `workspaces` slice (no sort). The sentinel is
/// structurally absent — see [`Stage2ResolutionLiteralOnly`] for the
/// type-level encoding of that absence.
fn compose_stage2_items_literal_only(
    workspaces: &[&Workspace],
    focused_workspace_id: Option<u64>,
    target_activity_id: u64,
    activity_names: &HashMap<u64, String>,
) -> Vec<String> {
    workspaces
        .iter()
        .map(|w| {
            workspace_label_with_annotations(
                w,
                target_activity_id,
                focused_workspace_id == Some(w.id),
                activity_names,
            )
        })
        .collect()
}

/// Resolves the stage-1 picker outcome to one of five branches:
/// cancellation, the `« Current activity »` sentinel, the
/// `« New activity »` sentinel, a literal activity selection, or
/// `Unknown` when the stage-1 picker returns a row not in the items we
/// passed (a picker-side contract violation).
///
/// Sentinel matches are strict equality against the unicode or
/// underscore-fallback form passed as `sentinels`. Activity match walks
/// the snapshot by name. `Unknown(name)` is returned rather than
/// silently folding contract violations into `Cancelled` so callers
/// can surface the anomaly as `MalformedResponse`.
fn resolve_stage1<'a>(
    picked: PickerOutcome,
    sentinels: &Stage1Sentinels,
    activities: &'a [Activity],
) -> Stage1Resolution<'a> {
    match picked {
        PickerOutcome::Cancelled => Stage1Resolution::Cancelled,
        PickerOutcome::Selected(name) => {
            if name == sentinels.current {
                Stage1Resolution::CurrentActivity
            } else if name == sentinels.new_activity {
                Stage1Resolution::NewActivity
            } else if let Some(a) = activities.iter().find(|a| a.name == name) {
                Stage1Resolution::Selected(a)
            } else {
                Stage1Resolution::Unknown(name)
            }
        }
    }
}

/// Resolves the stage-2 picker outcome on the with-new path to one of
/// four branches: cancellation, the `« New workspace »` sentinel, a
/// literal workspace selection, or `Unknown` when the stage-2 picker
/// returns a label not in the items we passed (a picker-side contract
/// violation).
///
/// `Unknown(label)` is returned rather than silently folding contract
/// violations into `Cancelled` so callers can surface the anomaly as
/// `MalformedResponse`.
fn resolve_stage2_with_new<'a>(
    picked: PickerOutcome,
    sentinel: &str,
    candidates: &'a [&'a Workspace],
    focused_workspace_id: Option<u64>,
    target_activity_id: u64,
    activity_names: &HashMap<u64, String>,
) -> Stage2ResolutionWithNew<'a> {
    match picked {
        PickerOutcome::Cancelled => Stage2ResolutionWithNew::Cancelled,
        PickerOutcome::Selected(label) => {
            if label == sentinel {
                Stage2ResolutionWithNew::NewWorkspace
            } else if let Some(ws) = focused_workspace_id
                .and_then(|focused_id| candidates.iter().find(|w| w.id == focused_id).copied())
                .filter(|ws| {
                    // Match the full composed label including membership
                    // suffix — the helper's suffix-ordering invariant
                    // (`{base}{membership}{current}`) is what the
                    // composer emits, so the resolver matches against
                    // the same shape.
                    label
                        == workspace_label_with_annotations(
                            ws,
                            target_activity_id,
                            true,
                            activity_names,
                        )
                })
            {
                Stage2ResolutionWithNew::AlreadyCurrent(ws.id)
            } else if let Some(ws) = candidates
                .iter()
                .find(|w| {
                    workspace_label_with_annotations(w, target_activity_id, false, activity_names)
                        == label
                })
                .copied()
            {
                debug_assert!(
                    focused_workspace_id != Some(ws.id),
                    "Selected branch reached for the focused workspace; AlreadyCurrent branch should have matched first",
                );
                Stage2ResolutionWithNew::Selected(ws)
            } else {
                Stage2ResolutionWithNew::Unknown(label)
            }
        }
    }
}

/// Resolves the stage-2 picker outcome on the literal-only path to one
/// of four branches: cancellation, the `« New workspace »` sentinel,
/// a literal workspace selection, or `Unknown` when the stage-2 picker
/// returns a label not in the items we passed (a picker-side contract
/// violation).
///
/// **Signature symmetry.** Takes a `sentinel` argument so it is
/// shape-compatible with [`resolve_stage2_with_new`]. On the literal-
/// only path the dispatcher does **not** inject the sentinel into the
/// item list, so a `Stage2ResolutionLiteralOnly::NewWorkspace` outcome
/// is statically unreachable in production today — see the enum docs.
/// The resolver still performs the equality check so a future
/// regression that does inject the sentinel produces a typed
/// resolution rather than misrouting to `Unknown`.
///
/// `Unknown(label)` is returned rather than silently folding contract
/// violations into `Cancelled` so callers can surface the anomaly as
/// `MalformedResponse`.
fn resolve_stage2_literal_only<'a>(
    picked: PickerOutcome,
    sentinel: &str,
    candidates: &'a [&'a Workspace],
    focused_workspace_id: Option<u64>,
    target_activity_id: u64,
    activity_names: &HashMap<u64, String>,
) -> Stage2ResolutionLiteralOnly<'a> {
    match picked {
        PickerOutcome::Cancelled => Stage2ResolutionLiteralOnly::Cancelled,
        PickerOutcome::Selected(label) => {
            if label == sentinel {
                Stage2ResolutionLiteralOnly::NewWorkspace
            } else if let Some(ws) = focused_workspace_id
                .and_then(|focused_id| candidates.iter().find(|w| w.id == focused_id).copied())
                .filter(|ws| {
                    label
                        == workspace_label_with_annotations(
                            ws,
                            target_activity_id,
                            true,
                            activity_names,
                        )
                })
            {
                Stage2ResolutionLiteralOnly::AlreadyCurrent(ws.id)
            } else if let Some(ws) = candidates
                .iter()
                .find(|w| {
                    workspace_label_with_annotations(w, target_activity_id, false, activity_names)
                        == label
                })
                .copied()
            {
                debug_assert!(
                    focused_workspace_id != Some(ws.id),
                    "Selected branch reached for the focused workspace; AlreadyCurrent branch should have matched first",
                );
                Stage2ResolutionLiteralOnly::Selected(ws)
            } else {
                Stage2ResolutionLiteralOnly::Unknown(label)
            }
        }
    }
}

/// Renders a workspace as a single-line label for the stage-2 picker menu.
///
/// Format (chosen by `ws.is_in_active_activity`):
/// - Active-activity workspace, named → `<name> (idx N)`.
/// - Active-activity workspace, unnamed → `idx N`.
/// - Hidden-activity workspace, named → `<name> (id N)`.
/// - Hidden-activity workspace, unnamed → `id N`.
///
/// The asymmetry is load-bearing. The compositor's contract is that
/// `Workspace.idx` is only meaningful when `is_in_active_activity ==
/// true`; hidden-activity workspaces all carry `idx = 0`, so labelling
/// them by `idx` produced indistinguishable rows that fuzzel collapses
/// into a single unselectable entry. Labelling by the stable, globally-
/// unique `id` is the fix: two hidden workspaces produce two distinct
/// rows. The single-activity-per-invocation invariant (both stage-2
/// dispatchers filter to one activity at a time) means the user never
/// sees a popup mixing `idx N` and `id N` rows — every row in a given
/// popup uses the same scheme.
fn workspace_label(ws: &Workspace) -> String {
    let unit = if ws.is_in_active_activity {
        "idx"
    } else {
        "id"
    };
    let value = if ws.is_in_active_activity {
        u64::from(ws.idx)
    } else {
        ws.id
    };
    match &ws.name {
        Some(name) => format!("{name} ({unit} {value})"),
        None => format!("{unit} {value}"),
    }
}

/// Renders a workspace as a stage-2 picker label with optional
/// annotation suffixes layered on top of the base [`workspace_label`]
/// output.
///
/// Annotations (suffix order is **load-bearing** and pinned by tests):
///
/// 1. **Base** — [`workspace_label`] output.
/// 2. **Membership suffix** — discloses cross-activity membership of the
///    workspace:
///    - If `ws.is_sticky == true`: ` [sticky]`. Sticky takes
///      precedence over `[also in: …]` because a sticky workspace is
///      conceptually in every activity, so listing the other activity
///      names by name would be noisy and misleading.
///    - Else if `ws.activities.len() > 1`: ` [also in: <names>]`,
///      where `<names>` is the alphabetically-sorted list of the
///      activity names from `activity_names` whose ids appear in
///      `ws.activities` and are not `target_activity_id`. Ids absent
///      from `activity_names` are silently skipped (defensive: a
///      compositor-supplied id with no matching name is not a CLI
///      crash condition).
///    - Else: no membership suffix (the workspace is in exactly one
///      activity and not sticky).
/// 3. **Current suffix** — ` (current)` iff `is_current == true`.
///
/// **Suffix-ordering invariant.** The final shape is always
/// `{base}{membership}{current}` — membership annotations come BEFORE
/// the `(current)` marker. This ordering is the resolver's source of
/// truth for label matching: future extensions to this function must
/// preserve it. The invariant is pinned by
/// `workspace_label_sticky_plus_current_renders_in_correct_order` in
/// the test module.
///
/// This helper is the **single source of truth** for stage-2 label shape.
/// Both composers (`compose_stage2_items_with_new`, `compose_stage2_items_literal_only`)
/// call it forward; both resolvers (`resolve_stage2_with_new`, `resolve_stage2_literal_only`)
/// call it for backward string-matching. Changing the output here cascades through all
/// four sites — keep them in sync or split the helper into separate emit/parse halves.
fn workspace_label_with_annotations(
    ws: &Workspace,
    target_activity_id: u64,
    is_current: bool,
    activity_names: &HashMap<u64, String>,
) -> String {
    let base = workspace_label(ws);
    let membership = if ws.is_sticky {
        " [sticky]".to_string()
    } else if ws.activities.len() > 1 {
        let mut others: Vec<&str> = ws
            .activities
            .iter()
            .filter(|&&id| id != target_activity_id)
            // Race-window: `Activities` and `Workspaces` are fetched in separate IPC
            // round-trips; a new activity can land between them, appearing in
            // `ws.activities` before `activity_names` sees it. Silently skipping the
            // unknown id keeps the label clean (no stray `[also in: ]` artifact).
            .filter_map(|id| activity_names.get(id).map(String::as_str))
            .collect();
        if others.is_empty() {
            String::new()
        } else {
            others.sort_unstable();
            format!(" [also in: {}]", others.join(", "))
        }
    } else {
        String::new()
    };
    let current = if is_current { " (current)" } else { "" };
    format!("{base}{membership}{current}")
}

#[cfg(test)]
mod tests {
    use niri_ipc::{
        Action, Activity, NoOpReason, Reply, Request, Response, Workspace, WorkspaceReferenceArg,
    };

    use super::*;
    use crate::ipc::MockClient;

    // ---- Fixtures ----------------------------------------------------------

    fn act(id: u64, name: &str, is_active: bool) -> Activity {
        Activity {
            id,
            name: name.into(),
            is_active,
            is_config_declared: true,
            ..Default::default()
        }
    }

    fn ws(
        id: u64,
        idx: u8,
        focused: bool,
        output: Option<&str>,
        activities: Vec<u64>,
        active_window: Option<u64>,
    ) -> Workspace {
        // Default test fixture mirrors the common case: focused implies
        // the workspace is in the active activity. Tests that need a
        // hidden-activity workspace (`is_in_active_activity = false`)
        // build a workspace via this helper and clear the flag explicitly.
        Workspace {
            id,
            idx,
            name: None,
            output: output.map(str::to_owned),
            is_urgent: false,
            is_active: false,
            is_focused: focused,
            active_window_id: active_window,
            activities,
            is_sticky: false,
            is_in_active_activity: focused,
        }
    }

    /// Same as [`ws`] but the produced workspace is **not** in the active
    /// activity (`is_in_active_activity = false`). Used by the
    /// `workspace_label` tests that pin the hidden-activity → `id N`
    /// labelling branch.
    fn ws_hidden(
        id: u64,
        idx: u8,
        output: Option<&str>,
        activities: Vec<u64>,
        active_window: Option<u64>,
    ) -> Workspace {
        let mut w = ws(id, idx, false, output, activities, active_window);
        w.is_in_active_activity = false;
        w
    }

    fn move_req(ws_id: u64) -> Request {
        Request::Action(Action::MoveWindowToWorkspace {
            window_id: None,
            reference: WorkspaceReferenceArg::Id(ws_id),
            focus: false,
        })
    }

    /// Variant of [`move_req`] that pins `window_id: Some(_)` — used by
    /// the `--follow` thread-through tests to assert the captured id
    /// reaches the wire.
    fn move_req_with_window(ws_id: u64, window_id: u64) -> Request {
        Request::Action(Action::MoveWindowToWorkspace {
            window_id: Some(window_id),
            reference: WorkspaceReferenceArg::Id(ws_id),
            focus: false,
        })
    }

    /// Default `prompt_name` fake for `run_picker` tests that do **not**
    /// exercise the `« New activity »` sentinel branch. Panicking on
    /// invocation pins that the test under cuts the stage-1 dispatch
    /// before reaching the new-activity prompt — a regression that
    /// accidentally routed a non-new-activity row through the prompt
    /// would surface as the panic.
    fn no_new_activity_prompt(_prompt: &str) -> Result<NameOutcome, CliError> {
        panic!("prompt_name_fn must NOT be invoked for this test");
    }

    // ---- focused_workspace / focused_output_name ---------------------------

    #[test]
    fn focused_workspace_returns_focused_one() {
        let workspaces = vec![
            ws(1, 0, false, Some("DP-1"), vec![1], None),
            ws(2, 1, true, Some("DP-1"), vec![1], None),
        ];
        let w = focused_workspace(&workspaces).expect("focused exists");
        assert_eq!(w.id, 2);
    }

    #[test]
    fn focused_workspace_no_focused_routes_to_malformed_server() {
        let workspaces = vec![ws(1, 0, false, Some("DP-1"), vec![1], None)];
        let err = focused_workspace(&workspaces).expect_err("no focused must fail");
        match err {
            CliError::MalformedResponse(MalformedResponseSource::Server(msg)) => {
                assert_eq!(msg, "no focused workspace");
            }
            other => panic!("expected MalformedResponse(Server), got {other:?}"),
        }
    }

    #[test]
    fn focused_output_name_returns_output_field() {
        let workspaces = vec![ws(1, 0, true, Some("DP-1"), vec![1], None)];
        let out = focused_output_name(&workspaces).expect("output exists");
        assert_eq!(out, "DP-1");
    }

    #[test]
    fn focused_output_name_focused_workspace_no_output_routes_to_malformed_server() {
        let workspaces = vec![ws(1, 0, true, None, vec![1], None)];
        let err = focused_output_name(&workspaces).expect_err("no output must fail");
        match err {
            CliError::MalformedResponse(MalformedResponseSource::Server(msg)) => {
                assert_eq!(msg, "focused workspace has no output");
            }
            other => panic!("expected MalformedResponse(Server), got {other:?}"),
        }
    }

    // ---- current_activity --------------------------------------------------

    #[test]
    fn current_activity_returns_active_one() {
        let acts = vec![act(1, "Work", false), act(2, "Personal", true)];
        let a = current_activity(&acts).expect("active exists");
        assert_eq!(a.id, 2);
        assert_eq!(a.name, "Personal");
    }

    #[test]
    fn current_activity_no_active_routes_to_malformed_server() {
        let acts = vec![act(1, "Work", false)];
        let err = current_activity(&acts).expect_err("no active must fail");
        match err {
            CliError::MalformedResponse(MalformedResponseSource::Server(msg)) => {
                assert_eq!(msg, "no active activity");
            }
            other => panic!("expected MalformedResponse(Server), got {other:?}"),
        }
    }

    // ---- workspaces_in_activity_on_focused_output --------------------------

    #[test]
    fn workspaces_in_activity_on_focused_output_filters_by_membership_and_output() {
        let workspaces = vec![
            // matches: activity + output
            ws(1, 0, false, Some("DP-1"), vec![1], None),
            // wrong activity
            ws(2, 1, false, Some("DP-1"), vec![2], None),
            // wrong output
            ws(3, 2, false, Some("DP-2"), vec![1], None),
            // matches: activity + output
            ws(4, 3, false, Some("DP-1"), vec![1, 2], None),
        ];
        let filtered = workspaces_in_activity_on_focused_output(&workspaces, 1, "DP-1");
        assert_eq!(
            filtered.iter().map(|w| w.id).collect::<Vec<_>>(),
            vec![1, 4]
        );
    }

    #[test]
    fn workspaces_in_activity_on_focused_output_preserves_compositor_order() {
        let workspaces = vec![
            ws(10, 2, false, Some("DP-1"), vec![1], None),
            ws(11, 0, false, Some("DP-1"), vec![1], None),
            ws(12, 1, false, Some("DP-1"), vec![1], None),
        ];
        let filtered = workspaces_in_activity_on_focused_output(&workspaces, 1, "DP-1");
        // No reorder — input order is preserved (ids 10, 11, 12 as queued).
        assert_eq!(
            filtered.iter().map(|w| w.id).collect::<Vec<_>>(),
            vec![10, 11, 12]
        );
    }

    // ---- sort_for_picker ---------------------------------------------------

    #[test]
    fn sort_for_picker_orders_hidden_workspaces_by_id_ascending() {
        // Three hidden-activity workspaces with idx 0 (compositor contract).
        // The picker order must be stable and predictable: ascending by id,
        // because idx is degenerate for hidden rows.
        let a = ws_hidden(30, 0, Some("DP-1"), vec![2], None);
        let b = ws_hidden(10, 0, Some("DP-1"), vec![2], None);
        let c = ws_hidden(20, 0, Some("DP-1"), vec![2], None);
        let mut filtered = vec![&a, &b, &c];
        sort_for_picker(&mut filtered);
        assert_eq!(
            filtered.iter().map(|w| w.id).collect::<Vec<_>>(),
            vec![10, 20, 30],
        );
    }

    #[test]
    fn sort_for_picker_orders_active_workspaces_by_idx_ascending() {
        // Active-activity workspaces sort by idx, with id as the final
        // tiebreaker. Compositor-supplied order is NOT preserved.
        let mut a = ws(100, 2, false, Some("DP-1"), vec![1], None);
        a.is_in_active_activity = true;
        let mut b = ws(101, 0, false, Some("DP-1"), vec![1], None);
        b.is_in_active_activity = true;
        let mut c = ws(102, 1, false, Some("DP-1"), vec![1], None);
        c.is_in_active_activity = true;
        let mut filtered = vec![&a, &b, &c];
        sort_for_picker(&mut filtered);
        assert_eq!(
            filtered.iter().map(|w| w.idx).collect::<Vec<_>>(),
            vec![0, 1, 2],
        );
    }

    #[test]
    fn sort_for_picker_active_workspaces_sort_before_hidden_workspaces() {
        // Mixed fixture: one active-activity workspace with a high idx (99)
        // and one hidden-activity workspace with a low id (0). Active must
        // land first regardless of idx/id ordering — the bucket boundary is
        // `!is_in_active_activity`, which sorts `false` (active) before
        // `true` (hidden).
        let mut active = ws(50, 99, false, Some("DP-1"), vec![1], None);
        active.is_in_active_activity = true;
        let hidden = ws_hidden(0, 0, Some("DP-1"), vec![2], None);
        let mut filtered = vec![&hidden, &active];
        sort_for_picker(&mut filtered);
        assert_eq!(
            filtered.iter().map(|w| w.id).collect::<Vec<_>>(),
            vec![50, 0],
            "active bucket must precede hidden bucket regardless of idx/id values",
        );
    }

    // ---- format_move_confirmation ------------------------------------------

    #[test]
    fn format_move_confirmation_renders_workspace_activity() {
        // Pins the exact stderr format. The window id is intentionally
        // absent (snapshot-vs-dispatch race; dispatch uses window_id: None).
        // The activity name is single-quoted so whitespace-bearing names
        // stay legible.
        let line = format_move_confirmation(7, "Personal");
        assert_eq!(
            line,
            "niri-activities: moved focused window to workspace 7 in activity 'Personal'",
        );
    }

    // ---- workspace_label ---------------------------------------------------

    #[test]
    fn workspace_label_named_workspace_includes_name_and_idx() {
        // Active-activity workspace → labelled by idx.
        let mut w = ws(1, 3, false, Some("DP-1"), vec![1], None);
        w.is_in_active_activity = true;
        w.name = Some("Work".into());
        assert_eq!(workspace_label(&w), "Work (idx 3)");
    }

    #[test]
    fn workspace_label_unnamed_workspace_shows_idx_only() {
        // Active-activity workspace → labelled by idx.
        let mut w = ws(1, 7, false, Some("DP-1"), vec![1], None);
        w.is_in_active_activity = true;
        assert_eq!(workspace_label(&w), "idx 7");
    }

    #[test]
    fn workspace_label_named_hidden_workspace_uses_id() {
        // Hidden-activity workspace → labelled by id (idx is not meaningful
        // for workspaces outside the active activity).
        let mut w = ws_hidden(42, 0, Some("DP-1"), vec![2], None);
        w.name = Some("Reading".into());
        assert_eq!(workspace_label(&w), "Reading (id 42)");
    }

    #[test]
    fn workspace_label_unnamed_hidden_workspace_uses_id() {
        let w = ws_hidden(7, 0, Some("DP-1"), vec![2], None);
        assert_eq!(workspace_label(&w), "id 7");
    }

    #[test]
    fn workspace_label_two_hidden_workspaces_with_idx_zero_produce_distinct_labels() {
        // Regression pin for the daily-driver bug: two hidden-activity
        // workspaces both have idx 0 (compositor contract: idx is only
        // meaningful for the active activity). Labelling by idx produces
        // a single collapsed/unselectable row in fuzzel; labelling by id
        // distinguishes them.
        let a = ws_hidden(11, 0, Some("DP-1"), vec![3], None);
        let b = ws_hidden(12, 0, Some("DP-1"), vec![3], None);
        let la = workspace_label(&a);
        let lb = workspace_label(&b);
        assert_ne!(la, lb, "hidden workspaces must produce distinct labels");
        assert_eq!(la, "id 11");
        assert_eq!(lb, "id 12");
    }

    // ---- trailing_empty_workspace ------------------------------------------

    #[test]
    fn trailing_empty_workspace_picks_max_idx_with_no_active_window() {
        let a = ws(1, 0, false, Some("DP-1"), vec![1], None);
        let b = ws(2, 5, false, Some("DP-1"), vec![1], None);
        let c = ws(3, 3, false, Some("DP-1"), vec![1], Some(42));
        let filtered = vec![&a, &b, &c];
        let trailing = trailing_empty_workspace(&filtered).expect("trailing-empty exists");
        assert_eq!(trailing.id, 2, "max-idx empty workspace must win");
    }

    #[test]
    fn trailing_empty_workspace_returns_none_when_all_have_windows() {
        let a = ws(1, 0, false, Some("DP-1"), vec![1], Some(10));
        let b = ws(2, 1, false, Some("DP-1"), vec![1], Some(20));
        let filtered = vec![&a, &b];
        assert!(trailing_empty_workspace(&filtered).is_none());
    }

    // ---- sentinel_names / workspace_sentinel_names --------------------------

    #[test]
    fn sentinel_names_no_collision_uses_unicode() {
        let names = vec!["Work", "Personal"];
        let s = sentinel_names(&names);
        assert_eq!(s.current, "« Current activity »");
        assert_eq!(s.new_activity, "« New activity »");
    }

    #[test]
    fn sentinel_names_collision_with_current_activity_uses_underscore_fallback() {
        // Per-row fallback: only `current` is colliding, so `new_activity`
        // still uses its unicode form.
        let names = vec!["Work", "« Current activity »"];
        let s = sentinel_names(&names);
        assert_eq!(s.current, "__niri_activities_current_activity__");
        assert_eq!(s.new_activity, "« New activity »");
    }

    #[test]
    fn sentinel_names_falls_back_when_activity_named_new_activity() {
        // Per-row fallback: only `new_activity` is colliding, so
        // `current` still uses its unicode form. Pins that collision
        // detection is independent per sentinel.
        let names = vec!["Work", "« New activity »"];
        let s = sentinel_names(&names);
        assert_eq!(s.current, "« Current activity »");
        assert_eq!(s.new_activity, "__niri_activities_new_activity__");
    }

    #[test]
    fn workspace_sentinel_names_no_collision_uses_unicode() {
        let labels = vec!["ws-1", "ws-2"];
        let s = workspace_sentinel_names(&labels);
        assert_eq!(s, "« New workspace »");
    }

    #[test]
    fn workspace_sentinel_names_collision_with_new_workspace_uses_underscore_fallback() {
        let labels = vec!["ws-1", "« New workspace »"];
        let s = workspace_sentinel_names(&labels);
        assert_eq!(s, "__niri_activities_new_workspace__");
    }

    // ---- compose_stage1_items / compose_stage2_items_{with_new,literal_only} -

    #[test]
    fn compose_stage1_items_puts_current_activity_sentinel_first() {
        let acts = vec![act(1, "Work", false), act(2, "Personal", true)];
        let sentinels = sentinel_names(&["Work", "Personal"]);
        let items = compose_stage1_items(&acts, &sentinels);
        assert_eq!(items[0], "« Current activity »");
        assert_eq!(items[1], "Work");
        assert_eq!(items[2], "Personal");
    }

    #[test]
    fn compose_stage1_items_preserves_compositor_order_no_focused_first_reorder() {
        // 'Personal' is active here, but the « Current activity »
        // sentinel covers the focused-activity shortcut — the activity
        // slice must NOT be reordered to hoist 'Personal' above 'Work'.
        let acts = vec![act(1, "Work", false), act(2, "Personal", true)];
        let sentinels = sentinel_names(&["Work", "Personal"]);
        let items = compose_stage1_items(&acts, &sentinels);
        assert_eq!(
            items,
            vec![
                "« Current activity »",
                "Work",
                "Personal",
                "« New activity »",
            ],
        );
    }

    #[test]
    fn compose_stage1_includes_new_activity_sentinel_at_end() {
        // The « New activity » sentinel must be the last row so the
        // user has a single-keystroke (or End-key) affordance to reach
        // the new-activity prompt regardless of how many activities
        // exist.
        let acts = vec![act(1, "Work", false), act(2, "Personal", true)];
        let sentinels = sentinel_names(&["Work", "Personal"]);
        let items = compose_stage1_items(&acts, &sentinels);
        assert_eq!(items.last().map(String::as_str), Some("« New activity »"));
        assert_eq!(items.len(), 4);
    }

    #[test]
    fn compose_stage2_items_with_new_appends_new_workspace_sentinel() {
        let a = ws(1, 0, false, Some("DP-1"), vec![1], None);
        let b = ws(2, 1, false, Some("DP-1"), vec![1], None);
        let filtered = vec![&a, &b];
        let sentinel = workspace_sentinel_names(&["idx 0", "idx 1"]);
        let items = compose_stage2_items_with_new(&filtered, None, sentinel, 1, &HashMap::new());
        assert_eq!(items.last().map(String::as_str), Some("« New workspace »"));
        assert_eq!(items.len(), 3);
    }

    #[test]
    fn compose_stage2_items_literal_only_omits_new_workspace_sentinel() {
        let a = ws(1, 0, false, Some("DP-1"), vec![1], None);
        let filtered = vec![&a];
        let items = compose_stage2_items_literal_only(&filtered, None, 1, &HashMap::new());
        assert!(items.iter().all(|s| s != "« New workspace »"));
        assert!(
            items
                .iter()
                .all(|s| s != "__niri_activities_new_workspace__")
        );
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn dispatch_stage2_literal_only_still_does_not_offer_new_workspace_sentinel_to_picker() {
        // Structural pin: compose_stage2_items_literal_only's output
        // contains neither the unicode sentinel nor the underscore
        // fallback, even when the workspace labels would collide. This
        // remains true after Stage2ResolutionLiteralOnly grew the
        // `NewWorkspace` type-state variant — the resolver enum's shape
        // changed, the composer's contract did not.
        let a = ws(1, 0, false, Some("DP-1"), vec![1], None);
        let b = ws(2, 1, false, Some("DP-1"), vec![1], None);
        let filtered = vec![&a, &b];
        let items = compose_stage2_items_literal_only(&filtered, None, 1, &HashMap::new());
        assert!(items.iter().all(|s| s != "« New workspace »"));
        assert!(
            items
                .iter()
                .all(|s| s != "__niri_activities_new_workspace__")
        );
    }

    // ---- workspace_label_with_annotations / current-workspace annotation ---

    /// Test-only helper that builds a `HashMap<u64, String>` from a
    /// slice of `(id, name)` tuples. Keeps the annotation tests below
    /// readable without hand-rolling `into_iter().collect()` at every
    /// call site.
    fn activity_lookup(entries: &[(u64, &str)]) -> HashMap<u64, String> {
        entries
            .iter()
            .map(|(id, name)| (*id, (*name).to_owned()))
            .collect()
    }

    #[test]
    fn workspace_label_with_annotations_appends_current_suffix() {
        // is_current=true must append ` (current)`; is_current=false
        // returns the base label unchanged.
        let mut w = ws(7, 2, true, Some("DP-1"), vec![1], None);
        w.is_in_active_activity = true;
        let names = activity_lookup(&[(1, "Work")]);
        assert_eq!(
            workspace_label_with_annotations(&w, 1, true, &names),
            "idx 2 (current)",
        );
        assert_eq!(
            workspace_label_with_annotations(&w, 1, false, &names),
            "idx 2",
        );
    }

    #[test]
    fn compose_stage2_annotates_only_focused_window_workspace() {
        // Three candidates in the same activity; the focused window is
        // in the middle one (id 20). Only that row gets the (current)
        // annotation.
        let mut a = ws(10, 0, false, Some("DP-1"), vec![1], None);
        a.is_in_active_activity = true;
        let mut b = ws(20, 1, true, Some("DP-1"), vec![1], None);
        b.is_in_active_activity = true;
        let mut c = ws(30, 2, false, Some("DP-1"), vec![1], None);
        c.is_in_active_activity = true;
        let filtered = vec![&a, &b, &c];
        let sentinel = workspace_sentinel_names(&[]);
        let names = activity_lookup(&[(1, "Work")]);
        let items = compose_stage2_items_with_new(&filtered, Some(20), sentinel, 1, &names);
        // Sentinel is the last row; only middle row carries (current).
        assert_eq!(items[0], "idx 0");
        assert_eq!(items[1], "idx 1 (current)");
        assert_eq!(items[2], "idx 2");
        assert_eq!(items[3], "« New workspace »");
        assert_eq!(
            items.iter().filter(|s| s.contains("(current)")).count(),
            1,
            "exactly one row must carry (current); got: {items:?}",
        );
    }

    #[test]
    fn resolve_stage2_with_new_current_row_returns_already_current() {
        // Picker outcome is the (current)-suffixed label for id 7;
        // resolver returns AlreadyCurrent(7).
        let mut w = ws(7, 3, true, Some("DP-1"), vec![1], None);
        w.is_in_active_activity = true;
        let other = ws(8, 4, false, Some("DP-1"), vec![1], None);
        let filtered = vec![&w, &other];
        let sentinel = workspace_sentinel_names(&[]);
        let picked = PickerOutcome::Selected("idx 3 (current)".into());
        let names = activity_lookup(&[(1, "Work")]);
        match resolve_stage2_with_new(picked, sentinel, &filtered, Some(7), 1, &names) {
            Stage2ResolutionWithNew::AlreadyCurrent(id) => assert_eq!(id, 7),
            other => panic!("expected AlreadyCurrent(7), got {other:?}"),
        }
    }

    #[test]
    fn resolve_stage2_literal_only_current_row_returns_already_current() {
        // Symmetric pin on the literal-only path.
        let mut w = ws(7, 3, true, Some("DP-1"), vec![1], None);
        w.is_in_active_activity = true;
        let filtered = vec![&w];
        let sentinel = workspace_sentinel_names(&[]);
        let picked = PickerOutcome::Selected("idx 3 (current)".into());
        let names = activity_lookup(&[(1, "Work")]);
        match resolve_stage2_literal_only(picked, sentinel, &filtered, Some(7), 1, &names) {
            Stage2ResolutionLiteralOnly::AlreadyCurrent(id) => assert_eq!(id, 7),
            other => panic!("expected AlreadyCurrent(7), got {other:?}"),
        }
    }

    #[test]
    fn resolve_stage2_with_new_picked_multi_activity_label_returns_selected() {
        // Workspace id 7 belongs to activities [1, 2]. The picker returns the
        // fully-annotated label (including the [also in: Personal] suffix).
        // The resolver must map this to Selected(ws) — not Unknown.
        let mut w = ws(7, 3, false, Some("DP-1"), vec![1, 2], None);
        w.is_in_active_activity = true;
        let filtered = vec![&w];
        let sentinel = workspace_sentinel_names(&[]);
        let names = activity_lookup(&[(1, "Work"), (2, "Personal")]);
        // Reconstruct the expected label the same way the composer would.
        let expected_label = workspace_label_with_annotations(&w, 1, false, &names);
        let picked = PickerOutcome::Selected(expected_label);
        match resolve_stage2_with_new(picked, sentinel, &filtered, None, 1, &names) {
            Stage2ResolutionWithNew::Selected(ws) => assert_eq!(ws.id, 7),
            other => panic!("expected Selected(7), got {other:?}"),
        }
    }

    #[test]
    fn resolve_stage2_literal_only_picked_multi_activity_label_returns_selected() {
        // Same pattern against the literal-only resolver.
        let w = ws_hidden(7, 0, Some("DP-1"), vec![1, 2], None);
        let filtered = vec![&w];
        let sentinel = workspace_sentinel_names(&[]);
        let names = activity_lookup(&[(1, "Work"), (2, "Personal")]);
        let expected_label = workspace_label_with_annotations(&w, 2, false, &names);
        let picked = PickerOutcome::Selected(expected_label);
        match resolve_stage2_literal_only(picked, sentinel, &filtered, None, 2, &names) {
            Stage2ResolutionLiteralOnly::Selected(ws) => assert_eq!(ws.id, 7),
            other => panic!("expected Selected(7), got {other:?}"),
        }
    }

    #[test]
    fn dispatch_stage2_with_new_already_current_emits_breadcrumb_and_no_move() {
        // Drive dispatch_stage2_with_new via run_picker with a picker that
        // returns the (current)-annotated row. The dispatcher must return
        // Ok(()) and must NOT issue a MoveWindowToWorkspace request.
        // MockClient screams if an unexpected request arrives, so the
        // absence of a `client.expect(move_req(...))` call is the assertion.
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![act(1, "Work", true)])),
        );
        client.expect(
            Request::Workspaces,
            Reply::Ok(Response::Workspaces(vec![
                ws(10, 0, true, Some("DP-1"), vec![1], Some(99)),
                ws(20, 1, false, Some("DP-1"), vec![1], None),
            ])),
        );
        // No move_req expectation — the dispatcher must short-circuit.
        let pick = |prompt: &str, items: &[String]| -> Result<PickerOutcome, CliError> {
            if prompt == "Move window to activity:" {
                Ok(PickerOutcome::Selected("Work".into()))
            } else {
                // Pick the (current)-annotated row for the focused workspace.
                let current_row = items
                    .iter()
                    .find(|s| s.contains("(current)"))
                    .expect("(current) row must be present")
                    .clone();
                Ok(PickerOutcome::Selected(current_row))
            }
        };
        run_picker(&mut client, pick, no_new_activity_prompt, false, false)
            .expect("AlreadyCurrent must exit Ok");
        client.assert_consumed_in_order();
    }

    #[test]
    fn dispatch_stage2_literal_only_already_current_emits_breadcrumb_and_no_move() {
        // Drive dispatch_stage2_literal_only via run_picker with a picker that
        // returns the (current)-annotated row. The dispatcher must return
        // Ok(()) and must NOT issue a MoveWindowToWorkspace request.
        // Uses a non-active activity (literal-only path). Single-activity
        // membership keeps the label free of annotation suffixes so the
        // resolver match is clean.
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![
                act(1, "Work", true),
                act(2, "Personal", false),
            ])),
        );
        // The focused workspace (id 10) is in activity 2 (Personal) only.
        let mut focused_ws = ws(10, 0, true, Some("DP-1"), vec![2], Some(99));
        focused_ws.is_in_active_activity = false;
        client.expect(
            Request::Workspaces,
            Reply::Ok(Response::Workspaces(vec![focused_ws])),
        );
        // No move_req expectation — the dispatcher must short-circuit.
        let pick = |prompt: &str, items: &[String]| -> Result<PickerOutcome, CliError> {
            if prompt == "Move window to activity:" {
                Ok(PickerOutcome::Selected("Personal".into()))
            } else {
                // Pick the (current)-annotated row.
                let current_row = items
                    .iter()
                    .find(|s| s.contains("(current)"))
                    .expect("(current) row must be present")
                    .clone();
                Ok(PickerOutcome::Selected(current_row))
            }
        };
        run_picker(&mut client, pick, no_new_activity_prompt, false, false)
            .expect("AlreadyCurrent must exit Ok");
        client.assert_consumed_in_order();
    }

    #[test]
    fn dispatch_stage2_with_new_handles_response_no_op_already_on_target() {
        // Durable no-op signaling: user picks a non-(current) row (id 20),
        // but the compositor's reply is Response::NoOp(AlreadyOnTarget) —
        // a snapshot race (the window's workspace already matched the
        // target by the time the action was processed) or any other
        // compositor-side reason for the move to resolve to no-op.
        //
        // The dispatcher must return Ok(()) and consume the queued
        // move_req. assert_consumed_in_order pins that the dispatcher
        // does NOT issue any extra IPC after the no-op.
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![act(1, "Work", true)])),
        );
        client.expect(
            Request::Workspaces,
            Reply::Ok(Response::Workspaces(vec![
                ws(10, 0, true, Some("DP-1"), vec![1], Some(99)),
                ws(20, 1, false, Some("DP-1"), vec![1], None),
            ])),
        );
        client.expect(
            move_req(20),
            Reply::Ok(Response::NoOp(NoOpReason::AlreadyOnTarget {
                workspace_id: 20,
            })),
        );

        let pick = |prompt: &str, items: &[String]| -> Result<PickerOutcome, CliError> {
            if prompt == "Move window to activity:" {
                Ok(PickerOutcome::Selected("Work".into()))
            } else {
                // Pick the non-(current) row to drive dispatch through
                // to the compositor; items[0] is the focused row with
                // `(current)`, so pick items[1].
                assert_eq!(items[0], "idx 0 (current)");
                Ok(PickerOutcome::Selected(items[1].clone()))
            }
        };
        run_picker(&mut client, pick, no_new_activity_prompt, false, false)
            .expect("NoOp reply must exit Ok");
        client.assert_consumed_in_order();
    }

    #[test]
    fn dispatch_stage2_literal_only_handles_response_no_op_already_on_target() {
        // Symmetric to dispatch_stage2_with_new_handles_response_no_op_already_on_target
        // on the literal-only (non-active activity) path.
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![
                act(1, "Work", true),
                act(2, "Personal", false),
            ])),
        );
        // Two workspaces in Personal (id 2, non-active) on DP-1; the
        // focused workspace (id 10) lives in Personal so the (current)
        // row sits at items[0] and items[1] is the dispatch target.
        let mut focused_ws = ws(10, 0, true, Some("DP-1"), vec![2], Some(99));
        focused_ws.is_in_active_activity = false;
        let other_ws = ws_hidden(20, 1, Some("DP-1"), vec![2], None);
        client.expect(
            Request::Workspaces,
            Reply::Ok(Response::Workspaces(vec![focused_ws, other_ws])),
        );
        client.expect(
            move_req(20),
            Reply::Ok(Response::NoOp(NoOpReason::AlreadyOnTarget {
                workspace_id: 20,
            })),
        );

        let pick = |prompt: &str, items: &[String]| -> Result<PickerOutcome, CliError> {
            if prompt == "Move window to activity:" {
                Ok(PickerOutcome::Selected("Personal".into()))
            } else {
                // Pick the non-(current) row; items[0] is the focused
                // row with `(current)`. Non-active-activity labels use
                // `id <n>` (not `idx <n>`) per `workspace_label`'s
                // `is_in_active_activity = false` branch.
                assert_eq!(items[0], "id 10 (current)");
                Ok(PickerOutcome::Selected(items[1].clone()))
            }
        };
        run_picker(&mut client, pick, no_new_activity_prompt, false, false)
            .expect("NoOp reply must exit Ok");
        client.assert_consumed_in_order();
    }

    #[test]
    fn handle_move_outcome_no_op_payload_id_diverges_uses_payload_id_label() {
        // Regression pin for the payload_id != ws_id divergence case.
        // The compositor reports AlreadyOnTarget for workspace 99, but we
        // dispatched against workspace 20. Workspace 99 IS in the snapshot
        // so the breadcrumb resolves to its label (not workspace-20's).
        // This exercises the wire-payload-is-authoritative discipline: the
        // formatter must key the label lookup on payload_id (99 / "idx 5"),
        // not ws_id (20 / "idx 1").
        let ws_20 = ws(20, 1, false, Some("DP-1"), vec![1], None);
        let mut ws_99 = ws(99, 5, false, Some("DP-1"), vec![1], None);
        ws_99.is_in_active_activity = true;
        let workspaces = vec![ws_20, ws_99];

        // Assert the formatter resolves using payload_id (99), whose idx is 5.
        let msg = format_already_current_breadcrumb(99, &workspaces);
        assert!(
            msg.contains("idx 5"),
            "breadcrumb must contain workspace-99's label 'idx 5', got: {msg:?}",
        );
        assert!(
            !msg.contains("idx 1"),
            "breadcrumb must NOT contain workspace-20's label 'idx 1', got: {msg:?}",
        );

        // Also verify handle_move_outcome (which calls the formatter with
        // payload_id) returns Ok for this divergence case.
        let outcome = HandledOutcome::NoOp(NoOpReason::AlreadyOnTarget { workspace_id: 99 });
        handle_move_outcome(outcome, 20, "Work", &workspaces)
            .expect("AlreadyOnTarget with diverged ids must return Ok");
    }

    #[test]
    fn handle_move_outcome_known_no_op_variant_returns_ok() {
        // Smoke check: the known `AlreadyOnTarget` variant routes to `Ok(())`.
        //
        // `NoOpReason` is `#[non_exhaustive]`. We cannot construct a
        // truly-unknown variant from this crate, so the catch-all
        // `HandledOutcome::NoOp(other) => Err(MalformedResponse(...))` arm
        // cannot be reached in tests today. The compiler's exhaustiveness
        // check is the structural pin for that arm: once a new `NoOpReason`
        // variant lands in `niri-ipc`, this file must add an explicit match
        // arm for it (or map it here), keeping the catch-all honest.
        //
        // TODO(when NoOpReason gains a variant): add a dedicated catch-all
        // behavior test once a second unknown variant can be constructed.
        let workspaces = vec![ws(10, 0, true, Some("DP-1"), vec![1], None)];
        let outcome = HandledOutcome::NoOp(NoOpReason::AlreadyOnTarget { workspace_id: 10 });
        let result = handle_move_outcome(outcome, 10, "Work", &workspaces);
        assert!(result.is_ok(), "known AlreadyOnTarget must return Ok");
    }

    #[test]
    fn compose_stage2_annotates_no_row_when_focused_window_workspace_not_in_filtered_set() {
        // Cross-activity case: the focused workspace lives in activity
        // A (id 1) but the filter walks activity B (id 2) candidates.
        // The focused id is still threaded through, but no candidate
        // matches it — so NO row gets (current).
        let b1 = ws_hidden(100, 0, Some("DP-1"), vec![2], None);
        let b2 = ws_hidden(101, 0, Some("DP-1"), vec![2], None);
        let filtered = vec![&b1, &b2];
        let sentinel = workspace_sentinel_names(&[]);
        let names = activity_lookup(&[(1, "Work"), (2, "Personal")]);
        // Focused id is 5, none of the candidates have that id.
        let items = compose_stage2_items_with_new(&filtered, Some(5), sentinel, 2, &names);
        assert!(
            items.iter().all(|s| !s.contains("(current)")),
            "no row may carry (current) when focused workspace is outside the filter set; got: {items:?}",
        );
    }

    // ---- workspace_label_with_annotations / membership disclosure ---------

    #[test]
    fn workspace_label_sticky_appends_sticky_suffix() {
        // is_sticky = true → label ends with " [sticky]". The sticky
        // suffix does NOT include any " [also in: …]" disclosure (the
        // sticky branch is taken in preference; a sticky workspace is
        // conceptually in every activity so an enumerated also-in list
        // would be misleading).
        let mut w = ws(7, 2, false, Some("DP-1"), vec![1, 2], None);
        w.is_in_active_activity = true;
        w.is_sticky = true;
        let names = activity_lookup(&[(1, "Work"), (2, "Personal")]);
        let label = workspace_label_with_annotations(&w, 1, false, &names);
        assert!(
            label.ends_with(" [sticky]"),
            "sticky workspaces must carry [sticky] suffix; got: {label}",
        );
        assert!(
            !label.contains("[also in:"),
            "sticky branch must NOT emit [also in: …]; got: {label}",
        );
    }

    #[test]
    fn workspace_label_multi_activity_lists_others_alphabetized() {
        // activities = [1, 2, 3], target = 1; the other names must be
        // sorted alphabetically and the target excluded.
        let mut w = ws(7, 2, false, Some("DP-1"), vec![1, 2, 3], None);
        w.is_in_active_activity = true;
        // Names chosen so the unsorted compositor order (2, 3) is the
        // OPPOSITE of the sorted output (Archive, Research) — pins the
        // sort_unstable call.
        let names = activity_lookup(&[(1, "Work"), (2, "Research"), (3, "Archive")]);
        let label = workspace_label_with_annotations(&w, 1, false, &names);
        assert!(
            label.ends_with(" [also in: Archive, Research]"),
            "multi-activity label must list other names alphabetized; got: {label}",
        );
    }

    #[test]
    fn workspace_label_single_membership_has_no_also_in_annotation() {
        // activities = [1]: workspace is in exactly one activity, so
        // neither [also in:] nor [sticky] is emitted.
        let mut w = ws(7, 2, false, Some("DP-1"), vec![1], None);
        w.is_in_active_activity = true;
        let names = activity_lookup(&[(1, "Work")]);
        let label = workspace_label_with_annotations(&w, 1, false, &names);
        assert!(
            !label.contains("[also in:"),
            "single-membership workspace must NOT emit [also in: …]; got: {label}",
        );
        assert!(
            !label.contains("[sticky]"),
            "non-sticky workspace must NOT emit [sticky]; got: {label}",
        );
    }

    #[test]
    fn workspace_label_sticky_takes_precedence_over_also_in() {
        // is_sticky = true AND activities.len() > 1: the sticky branch
        // wins; no also-in is emitted.
        let mut w = ws(7, 2, false, Some("DP-1"), vec![1, 2, 3], None);
        w.is_in_active_activity = true;
        w.is_sticky = true;
        let names = activity_lookup(&[(1, "Work"), (2, "Personal"), (3, "Archive")]);
        let label = workspace_label_with_annotations(&w, 1, false, &names);
        assert!(
            label.contains("[sticky]"),
            "sticky must be emitted; got: {label}",
        );
        assert!(
            !label.contains("[also in:"),
            "sticky must take precedence over [also in: …]; got: {label}",
        );
    }

    #[test]
    fn workspace_label_unknown_activity_id_in_activities_list_is_skipped() {
        // activities = [1, 2, 99]; only ids 1 and 2 are in the name
        // map. Id 99 is silently skipped (defensive: a compositor-
        // supplied id without a corresponding name is not a CLI crash
        // condition).
        let mut w = ws(7, 2, false, Some("DP-1"), vec![1, 2, 99], None);
        w.is_in_active_activity = true;
        let names = activity_lookup(&[(1, "Work"), (2, "Personal")]);
        let label = workspace_label_with_annotations(&w, 1, false, &names);
        assert!(
            !label.contains("99"),
            "unknown id must not leak into the rendered label; got: {label}",
        );
        assert!(
            label.contains("Personal"),
            "known names must still be listed; got: {label}",
        );
        assert_eq!(
            label.matches("[also in:").count(),
            1,
            "exactly one [also in: …] substring expected; got: {label}",
        );
    }

    #[test]
    fn workspace_label_sticky_plus_current_renders_in_correct_order() {
        // Suffix-ordering invariant: when BOTH membership and current
        // suffixes fire, the order is {base}{membership}{current}.
        // Pinned here with is_sticky + is_current → ends with
        // "[sticky] (current)".
        let mut w = ws(7, 2, false, Some("DP-1"), vec![1, 2], None);
        w.is_in_active_activity = true;
        w.is_sticky = true;
        let names = activity_lookup(&[(1, "Work"), (2, "Personal")]);
        let label = workspace_label_with_annotations(&w, 1, true, &names);
        assert!(
            label.ends_with("[sticky] (current)"),
            "suffix order must be {{base}}{{membership}}{{current}}; got: {label}",
        );
    }

    #[test]
    fn workspace_label_multi_activity_plus_current_renders_in_correct_order() {
        // Suffix-ordering invariant: [also in: …] precedes (current).
        // activities = [1, 2], is_current = true → ends with "[also in: Personal] (current)".
        let mut w = ws(7, 2, false, Some("DP-1"), vec![1, 2], None);
        w.is_in_active_activity = true;
        let names = activity_lookup(&[(1, "Work"), (2, "Personal")]);
        let label = workspace_label_with_annotations(&w, 1, true, &names);
        assert!(
            label.ends_with("[also in: Personal] (current)"),
            "suffix order must be {{base}}{{membership}}{{current}}; got: {label}",
        );
    }

    // ---- resolve_stage1 / resolve_stage2_{with_new,literal_only} ----------

    #[test]
    fn resolve_stage1_recognises_current_activity_sentinel_with_underscore_fallback() {
        let acts = vec![act(1, "Work", true)];
        // Force the underscore fallback for `current` by passing a
        // colliding name.
        let sentinels = sentinel_names(&["« Current activity »"]);
        assert_eq!(sentinels.current, "__niri_activities_current_activity__");
        let picked = PickerOutcome::Selected("__niri_activities_current_activity__".into());
        match resolve_stage1(picked, &sentinels, &acts) {
            Stage1Resolution::CurrentActivity => {}
            other => panic!("expected CurrentActivity, got {other:?}"),
        }
    }

    #[test]
    fn resolve_stage1_recognises_new_activity_sentinel() {
        let acts = vec![act(1, "Work", true)];
        let sentinels = sentinel_names(&["Work"]);
        assert_eq!(sentinels.new_activity, "« New activity »");
        let picked = PickerOutcome::Selected("« New activity »".into());
        match resolve_stage1(picked, &sentinels, &acts) {
            Stage1Resolution::NewActivity => {}
            other => panic!("expected NewActivity, got {other:?}"),
        }
    }

    #[test]
    fn resolve_stage2_with_new_recognises_new_workspace_sentinel_with_underscore_fallback() {
        let a = ws(1, 0, false, Some("DP-1"), vec![1], None);
        let filtered = vec![&a];
        // Force the underscore fallback by passing a colliding label.
        let sentinel = workspace_sentinel_names(&["« New workspace »"]);
        assert_eq!(sentinel, "__niri_activities_new_workspace__");
        let picked = PickerOutcome::Selected("__niri_activities_new_workspace__".into());
        match resolve_stage2_with_new(picked, sentinel, &filtered, None, 1, &HashMap::new()) {
            Stage2ResolutionWithNew::NewWorkspace => {}
            other => panic!("expected NewWorkspace, got {other:?}"),
        }
    }

    #[test]
    fn resolve_stage1_unknown_row_returns_unknown_not_cancelled() {
        // Picker returned a row that wasn't in the items (contract
        // violation). Must surface as Unknown, not silently as Cancelled.
        let acts = vec![act(1, "Work", true)];
        let sentinels = sentinel_names(&["Work"]);
        let picked = PickerOutcome::Selected("NotAnActivity".into());
        match resolve_stage1(picked, &sentinels, &acts) {
            Stage1Resolution::Unknown(row) => assert_eq!(row, "NotAnActivity"),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn resolve_stage2_with_new_unknown_label_returns_unknown_not_cancelled() {
        // Picker returned a label that wasn't in the items (contract
        // violation). Must surface as Unknown, not silently as Cancelled.
        let a = ws(1, 0, false, Some("DP-1"), vec![1], None);
        let filtered = vec![&a];
        let sentinel = workspace_sentinel_names(&["idx 0"]);
        let picked = PickerOutcome::Selected("not-a-workspace".into());
        match resolve_stage2_with_new(picked, sentinel, &filtered, None, 1, &HashMap::new()) {
            Stage2ResolutionWithNew::Unknown(label) => assert_eq!(label, "not-a-workspace"),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn resolve_stage2_literal_only_unknown_label_returns_unknown_not_cancelled() {
        // Same contract-violation pin as the with-new path, but on the
        // literal-only path: ensure both enums route picker-side
        // contract violations to `Unknown(_)` rather than `Cancelled`.
        let a = ws(1, 0, false, Some("DP-1"), vec![1], None);
        let filtered = vec![&a];
        let sentinel = workspace_sentinel_names(&["idx 0"]);
        let picked = PickerOutcome::Selected("not-a-workspace".into());
        match resolve_stage2_literal_only(picked, sentinel, &filtered, None, 1, &HashMap::new()) {
            Stage2ResolutionLiteralOnly::Unknown(label) => assert_eq!(label, "not-a-workspace"),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn resolve_stage2_literal_only_recognises_new_workspace_sentinel_when_threaded_through_resolver()
     {
        // Pins the type-state symmetry: although the literal-only
        // composer does NOT inject the sentinel, the resolver still
        // recognises it when handed one. This decouples "composer
        // omits the sentinel" (current invariant) from "resolver
        // detects the sentinel" (futureproofing).
        let a = ws(1, 0, false, Some("DP-1"), vec![1], None);
        let filtered = vec![&a];
        let sentinel = workspace_sentinel_names(&["idx 0"]);
        let picked = PickerOutcome::Selected(sentinel.to_owned());
        match resolve_stage2_literal_only(picked, sentinel, &filtered, None, 1, &HashMap::new()) {
            Stage2ResolutionLiteralOnly::NewWorkspace => {}
            other => panic!("expected NewWorkspace, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_stage2_literal_only_new_workspace_resolver_arm_routes_to_malformed_response_server()
    {
        // Pin the defensive guard on the statically-unreachable
        // `NewWorkspace` arm of `dispatch_stage2_literal_only`. We
        // drive the dispatcher with a fake picker that synthesises the
        // sentinel string (simulating the composer-regression or
        // fuzzel-contract-drift case). The arm must surface as
        // MalformedResponse(Server(_)) → exit 65, not unreachable!().
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![
                act(1, "Work", true),
                act(2, "Personal", false),
            ])),
        );
        client.expect(
            Request::Workspaces,
            // Non-active activity 'Personal' has a workspace on the
            // focused output — drives the literal-only path.
            Reply::Ok(Response::Workspaces(vec![
                ws(10, 0, true, Some("DP-1"), vec![1], Some(99)),
                ws(20, 0, false, Some("DP-1"), vec![2], None),
            ])),
        );
        let pick = |prompt: &str, _items: &[String]| -> Result<PickerOutcome, CliError> {
            if prompt == "Move window to activity:" {
                Ok(PickerOutcome::Selected("Personal".into()))
            } else {
                // Synthesise the sentinel even though the composer did
                // not offer it — simulates the regression we're guarding
                // against.
                Ok(PickerOutcome::Selected("« New workspace »".into()))
            }
        };
        let err = run_picker(&mut client, pick, no_new_activity_prompt, false, false).expect_err(
            "synthetic sentinel on literal-only path must surface as MalformedResponse",
        );
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must be in chain");
        assert_eq!(cli_err.exit_code(), 65);
        match cli_err {
            CliError::MalformedResponse(MalformedResponseSource::Server(msg)) => {
                assert_eq!(
                    msg,
                    "stage-2 literal-only path produced new-workspace sentinel",
                );
            }
            other => panic!("expected MalformedResponse(Server), got {other:?}"),
        }
        client.assert_consumed_in_order();
    }

    // ---- run (named-arg) ---------------------------------------------------

    #[test]
    fn run_named_dispatches_move_window_to_workspace_with_id_focus_false_window_none() {
        // Pin three load-bearing fields:
        //   window_id: None  (focused window)
        //   reference: Id(_) (focus-drift guard, not Name/Index)
        //   focus: false     (move-only, not move+switch)
        // MockClient queue-equality enforces the exact request shape.
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![
                act(1, "Work", true),
                act(2, "Personal", false),
            ])),
        );
        // Workspaces: one focused on DP-1 in activity 1, one trailing-empty
        // in activity 2 on DP-1 (target for move-window Personal).
        client.expect(
            Request::Workspaces,
            Reply::Ok(Response::Workspaces(vec![
                ws(10, 0, true, Some("DP-1"), vec![1], Some(99)),
                ws(20, 0, false, Some("DP-1"), vec![2], None),
            ])),
        );
        client.expect(move_req(20), Reply::Ok(Response::Handled));

        run(&mut client, "Personal", false, false).expect("named-arg succeeds");
        client.assert_consumed_in_order();
    }

    #[test]
    fn run_named_unknown_activity_maps_to_activity_not_found_exit_66() {
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![act(1, "Work", true)])),
        );
        let err = run(&mut client, "Nope", false, false).expect_err("unknown name must fail");
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must be in chain");
        match cli_err {
            CliError::ActivityNotFound(name) => assert_eq!(name, "Nope"),
            other => panic!("expected ActivityNotFound, got {other:?}"),
        }
        assert_eq!(cli_err.exit_code(), 66);
        client.assert_consumed_in_order();
    }

    #[test]
    fn run_named_no_trailing_empty_for_non_active_activity_eprintln_exits_zero() {
        // Named-arg form, non-active activity 'Personal' with workspaces
        // that all have an active_window_id → no trailing-empty.
        // Must eprintln + Ok(()), NOT panic, NOT dispatch.
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![
                act(1, "Work", true),
                act(2, "Personal", false),
            ])),
        );
        client.expect(
            Request::Workspaces,
            Reply::Ok(Response::Workspaces(vec![
                ws(10, 0, true, Some("DP-1"), vec![1], Some(99)),
                ws(20, 0, false, Some("DP-1"), vec![2], Some(77)),
            ])),
        );
        run(&mut client, "Personal", false, false).expect("zero-case must exit Ok");
        // No third IPC call queued — dispatch was skipped.
        client.assert_consumed_in_order();
    }

    #[test]
    fn run_named_no_workspaces_on_focused_output_eprintln_exits_zero() {
        // Named-arg form, non-active activity 'Personal' with no workspaces
        // on the focused output at all (different from no trailing-empty).
        // Must eprintln + Ok(()), NOT panic, NOT dispatch.
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![
                act(1, "Work", true),
                act(2, "Personal", false),
            ])),
        );
        client.expect(
            Request::Workspaces,
            // Personal's workspace is on DP-2, not the focused output DP-1.
            Reply::Ok(Response::Workspaces(vec![
                ws(10, 0, true, Some("DP-1"), vec![1], None),
                ws(20, 0, false, Some("DP-2"), vec![2], None),
            ])),
        );
        run(&mut client, "Personal", false, false).expect("zero-case must exit Ok");
        client.assert_consumed_in_order();
    }

    // ---- run_picker (no-arg, two-stage) ------------------------------------

    #[test]
    fn run_picker_empty_activities_short_circuits_without_spawning_picker() {
        // move-window's own empty-activities branch: when Activities is
        // empty the stage-1 picker must NOT be spawned and Ok(()) is
        // returned (separate from the switch verb's analogous branch).
        let mut client = MockClient::new();
        client.expect(Request::Activities, Reply::Ok(Response::Activities(vec![])));
        let pick = |_prompt: &str, _items: &[String]| -> Result<PickerOutcome, CliError> {
            panic!("stage-1 picker must NOT be spawned for empty activities list");
        };
        run_picker(&mut client, pick, no_new_activity_prompt, false, false)
            .expect("empty activities must exit Ok");
        client.assert_consumed_in_order();
    }

    #[test]
    fn run_picker_stage1_cancel_skips_dispatch() {
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![act(1, "Work", true)])),
        );
        let pick = |_prompt: &str, _items: &[String]| -> Result<PickerOutcome, CliError> {
            Ok(PickerOutcome::Cancelled)
        };
        run_picker(&mut client, pick, no_new_activity_prompt, false, false)
            .expect("stage1 cancel is silent Ok");
        client.assert_consumed_in_order();
    }

    #[test]
    fn run_picker_stage1_current_sentinel_proceeds_to_stage2_with_active_activity() {
        // User picks « Current activity » → stage 2 fires via the
        // with-new path for the active activity (Work, id 1). Two
        // workspaces in Work; the focused one is id 10 (would render
        // as `idx 0 (current)` and resolve to AlreadyCurrent), so the
        // test picks the non-focused row id 20 to drive the dispatch
        // path.
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![
                act(1, "Work", true),
                act(2, "Personal", false),
            ])),
        );
        client.expect(
            Request::Workspaces,
            Reply::Ok(Response::Workspaces(vec![
                ws(10, 0, true, Some("DP-1"), vec![1], Some(99)),
                ws(20, 1, false, Some("DP-1"), vec![1], None),
            ])),
        );
        client.expect(move_req(20), Reply::Ok(Response::Handled));

        let pick = |prompt: &str, items: &[String]| -> Result<PickerOutcome, CliError> {
            if prompt == "Move window to activity:" {
                // Stage 1: sentinel first.
                assert_eq!(items[0], "« Current activity »");
                Ok(PickerOutcome::Selected("« Current activity »".into()))
            } else if prompt == "Move window to workspace:" {
                // Stage 2: « New workspace » appended for active activity.
                assert!(items.last().is_some_and(|s| s == "« New workspace »"));
                // items[0] is the focused row with `(current)` annotation;
                // pick items[1] (non-focused) so dispatch fires.
                assert_eq!(items[0], "idx 0 (current)");
                Ok(PickerOutcome::Selected(items[1].clone()))
            } else {
                panic!("unexpected prompt: {prompt}");
            }
        };
        run_picker(&mut client, pick, no_new_activity_prompt, false, false)
            .expect("happy path succeeds");
        client.assert_consumed_in_order();
    }

    #[test]
    fn run_picker_stage2_cancel_skips_dispatch() {
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![act(1, "Work", true)])),
        );
        client.expect(
            Request::Workspaces,
            Reply::Ok(Response::Workspaces(vec![ws(
                10,
                0,
                true,
                Some("DP-1"),
                vec![1],
                None,
            )])),
        );
        let pick = |prompt: &str, _items: &[String]| -> Result<PickerOutcome, CliError> {
            if prompt == "Move window to activity:" {
                Ok(PickerOutcome::Selected("Work".into()))
            } else {
                Ok(PickerOutcome::Cancelled)
            }
        };
        run_picker(&mut client, pick, no_new_activity_prompt, false, false)
            .expect("stage2 cancel is silent Ok");
        client.assert_consumed_in_order();
    }

    #[test]
    fn run_picker_stage2_new_workspace_sentinel_resolves_to_trailing_empty() {
        // Stage 2 returns « New workspace » → resolves to the
        // trailing-empty workspace (id 30, idx 2, no active window).
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![act(1, "Work", true)])),
        );
        client.expect(
            Request::Workspaces,
            Reply::Ok(Response::Workspaces(vec![
                ws(10, 0, true, Some("DP-1"), vec![1], Some(99)),
                ws(20, 1, false, Some("DP-1"), vec![1], Some(88)),
                ws(30, 2, false, Some("DP-1"), vec![1], None),
            ])),
        );
        client.expect(move_req(30), Reply::Ok(Response::Handled));

        let pick = |prompt: &str, _items: &[String]| -> Result<PickerOutcome, CliError> {
            if prompt == "Move window to activity:" {
                Ok(PickerOutcome::Selected("Work".into()))
            } else {
                Ok(PickerOutcome::Selected("« New workspace »".into()))
            }
        };
        run_picker(&mut client, pick, no_new_activity_prompt, false, false)
            .expect("new-workspace sentinel succeeds");
        client.assert_consumed_in_order();
    }

    #[test]
    fn run_picker_stage2_named_activity_non_active_no_workspaces_eprintln_exits_zero() {
        // Stage 1 picks 'Personal' (non-active) → stage 2 has no
        // workspaces on the focused output for activity 2 → eprintln +
        // Ok(()) without spawning stage 2.
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![
                act(1, "Work", true),
                act(2, "Personal", false),
            ])),
        );
        client.expect(
            Request::Workspaces,
            Reply::Ok(Response::Workspaces(vec![
                // Focused workspace, but in activity 1 — not 'Personal'.
                ws(10, 0, true, Some("DP-1"), vec![1], None),
            ])),
        );
        let pick = |prompt: &str, _items: &[String]| -> Result<PickerOutcome, CliError> {
            if prompt == "Move window to activity:" {
                Ok(PickerOutcome::Selected("Personal".into()))
            } else {
                panic!("stage 2 must NOT be spawned for non-active activity with no workspaces");
            }
        };
        run_picker(&mut client, pick, no_new_activity_prompt, false, false)
            .expect("zero-case must exit Ok");
        client.assert_consumed_in_order();
    }

    #[test]
    fn run_picker_new_activity_happy_path_creates_then_dispatches_stage2() {
        // Full NewActivity flow:
        // 1. Stage-1 activities fetch (Work, active).
        // 2. Stage-1 picker: user picks « New activity ».
        // 3. prompt_name_fn returns Typed("Personal").
        // 4. CreateActivity IPC dispatched → Handled.
        // 5. Activities refetched: now [Work(active), Personal(non-active)].
        // 6. Personal is not active → literal-only stage-2 path.
        // 7. Stage-2 Workspaces fetch.
        // 8. Stage-2 picker: user picks Personal's workspace.
        // 9. MoveWindowToWorkspace dispatched.
        // The post-create activity-id lookup and is_active branch (literal-only)
        // are pinned via the IPC queue assertion.
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![act(1, "Work", true)])),
        );
        client.expect(
            Request::Action(Action::CreateActivity {
                name: "Personal".to_owned(),
            }),
            Reply::Ok(Response::Handled),
        );
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![
                act(1, "Work", true),
                act(2, "Personal", false),
            ])),
        );
        client.expect(
            Request::Workspaces,
            Reply::Ok(Response::Workspaces(vec![
                ws(10, 0, true, Some("DP-1"), vec![1], Some(99)),
                ws(20, 0, false, Some("DP-1"), vec![2], None),
            ])),
        );
        client.expect(move_req(20), Reply::Ok(Response::Handled));

        let pick = |prompt: &str, _items: &[String]| -> Result<PickerOutcome, CliError> {
            if prompt == "Move window to activity:" {
                Ok(PickerOutcome::Selected("« New activity »".into()))
            } else {
                // Stage 2: pick the only workspace in Personal.
                Ok(PickerOutcome::Selected("id 20".into()))
            }
        };
        let prompt = |_prompt: &str| -> Result<NameOutcome, CliError> {
            Ok(NameOutcome::Typed("Personal".to_owned()))
        };
        run_picker(&mut client, pick, prompt, false, false)
            .expect("new-activity happy path must succeed");
        client.assert_consumed_in_order();
    }

    #[test]
    fn run_picker_new_activity_is_active_uses_with_new_stage2_path() {
        // When the newly-created activity lands as is_active=true
        // (the compositor made it active on create), the with-new stage-2
        // path is selected (« New workspace » sentinel offered).
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![act(1, "Work", true)])),
        );
        client.expect(
            Request::Action(Action::CreateActivity {
                name: "Personal".to_owned(),
            }),
            Reply::Ok(Response::Handled),
        );
        // Post-create snapshot: Personal is now active (compositor
        // activated it on creation).
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![
                act(1, "Work", false),
                act(2, "Personal", true),
            ])),
        );
        client.expect(
            Request::Workspaces,
            Reply::Ok(Response::Workspaces(vec![
                ws(10, 0, false, Some("DP-1"), vec![1], None),
                // Personal's trailing-empty workspace (is_in_active_activity=true).
                ws(20, 0, true, Some("DP-1"), vec![2], None),
            ])),
        );
        client.expect(move_req(20), Reply::Ok(Response::Handled));

        let pick = |prompt: &str, items: &[String]| -> Result<PickerOutcome, CliError> {
            if prompt == "Move window to activity:" {
                Ok(PickerOutcome::Selected("« New activity »".into()))
            } else {
                // Stage 2 on with-new path: « New workspace » must be offered.
                assert!(
                    items.last().is_some_and(|s| s == "« New workspace »"),
                    "with-new path must offer « New workspace »; items: {items:?}",
                );
                Ok(PickerOutcome::Selected("« New workspace »".into()))
            }
        };
        let prompt_fn = |_prompt: &str| -> Result<NameOutcome, CliError> {
            Ok(NameOutcome::Typed("Personal".to_owned()))
        };
        run_picker(&mut client, pick, prompt_fn, false, false)
            .expect("new-activity active path must succeed");
        client.assert_consumed_in_order();
    }

    #[test]
    fn run_picker_stage1_wrong_activities_variant_is_malformed_exit_65() {
        // `send_expect_activities` gets a wrong-variant reply → WrongVariant
        // MalformedResponse propagates before the stage-1 picker fires.
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Version("v".into())),
        );
        let pick = |_prompt: &str, _items: &[String]| -> Result<PickerOutcome, CliError> {
            panic!("stage 1 must NOT be spawned on malformed Activities response");
        };
        let err = run_picker(&mut client, pick, no_new_activity_prompt, false, false)
            .expect_err("wrong variant must fail");
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must be in chain");
        assert_eq!(cli_err.exit_code(), 65);
        match cli_err {
            CliError::MalformedResponse(MalformedResponseSource::WrongVariant {
                expected, ..
            }) => assert_eq!(*expected, "Response::Activities"),
            other => panic!("expected WrongVariant, got {other:?}"),
        }
        client.assert_consumed_in_order();
    }

    #[test]
    fn run_picker_stage2_wrong_workspaces_variant_is_malformed_exit_65() {
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![act(1, "Work", true)])),
        );
        client.expect(
            Request::Workspaces,
            Reply::Ok(Response::Version("v".into())),
        );
        let pick = |prompt: &str, _items: &[String]| -> Result<PickerOutcome, CliError> {
            if prompt == "Move window to activity:" {
                Ok(PickerOutcome::Selected("Work".into()))
            } else {
                panic!("stage 2 must NOT be spawned on malformed Workspaces response");
            }
        };
        let err = run_picker(&mut client, pick, no_new_activity_prompt, false, false)
            .expect_err("wrong variant must fail");
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must be in chain");
        assert_eq!(cli_err.exit_code(), 65);
        match cli_err {
            CliError::MalformedResponse(MalformedResponseSource::WrongVariant {
                expected, ..
            }) => assert_eq!(*expected, "Response::Workspaces"),
            other => panic!("expected WrongVariant, got {other:?}"),
        }
        client.assert_consumed_in_order();
    }

    // ---- run_here_picker (no-arg, single-stage) ----------------------------

    #[test]
    fn run_here_picker_dispatches_against_current_activity_only() {
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![
                act(1, "Work", true),
                act(2, "Personal", false),
            ])),
        );
        client.expect(
            Request::Workspaces,
            Reply::Ok(Response::Workspaces(vec![
                // Active activity = 1; focused workspace on DP-1.
                ws(10, 0, true, Some("DP-1"), vec![1], Some(99)),
                // Second workspace in activity 1 — the picker selects
                // this one so dispatch fires (selecting the focused row
                // would route to AlreadyCurrent).
                ws(11, 1, false, Some("DP-1"), vec![1], None),
                // Workspace in activity 2 on DP-1 — MUST NOT be offered.
                ws(20, 0, false, Some("DP-1"), vec![2], None),
            ])),
        );
        client.expect(move_req(11), Reply::Ok(Response::Handled));

        let pick = |prompt: &str, items: &[String]| -> Result<PickerOutcome, CliError> {
            assert_eq!(prompt, "Move window to workspace:");
            // Stage 2 only — no activity prompt. Items: two workspaces
            // (activity 1) + « New workspace » sentinel. The focused
            // row carries `(current)`, so pick the second row to drive
            // dispatch.
            assert_eq!(items.len(), 3, "items: {items:?}");
            assert!(items.last().is_some_and(|s| s == "« New workspace »"));
            assert_eq!(items[0], "idx 0 (current)");
            Ok(PickerOutcome::Selected(items[1].clone()))
        };
        run_here_picker(&mut client, pick, false, false).expect("happy path");
        client.assert_consumed_in_order();
    }

    #[test]
    fn run_here_picker_cancel_skips_dispatch() {
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![act(1, "Work", true)])),
        );
        client.expect(
            Request::Workspaces,
            Reply::Ok(Response::Workspaces(vec![ws(
                10,
                0,
                true,
                Some("DP-1"),
                vec![1],
                None,
            )])),
        );
        let pick = |_prompt: &str, _items: &[String]| -> Result<PickerOutcome, CliError> {
            Ok(PickerOutcome::Cancelled)
        };
        run_here_picker(&mut client, pick, false, false).expect("cancellation is silent Ok");
        client.assert_consumed_in_order();
    }

    #[test]
    fn run_here_picker_new_workspace_sentinel_resolves_to_trailing_empty() {
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![act(1, "Work", true)])),
        );
        client.expect(
            Request::Workspaces,
            Reply::Ok(Response::Workspaces(vec![
                ws(10, 0, true, Some("DP-1"), vec![1], Some(42)),
                ws(20, 1, false, Some("DP-1"), vec![1], None),
            ])),
        );
        client.expect(move_req(20), Reply::Ok(Response::Handled));

        let pick = |_prompt: &str, _items: &[String]| -> Result<PickerOutcome, CliError> {
            Ok(PickerOutcome::Selected("« New workspace »".into()))
        };
        run_here_picker(&mut client, pick, false, false).expect("new-workspace sentinel succeeds");
        client.assert_consumed_in_order();
    }

    #[test]
    fn run_here_picker_no_active_activity_routes_to_malformed_exit_65() {
        // No activity has is_active=true → current_activity_id returns
        // MalformedResponse(Server("no active activity")) → exit 65.
        // Mirrors run_picker_stage2_wrong_workspaces_variant_is_malformed_exit_65.
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![
                act(1, "Work", false),
                act(2, "Personal", false),
            ])),
        );
        let pick = |_prompt: &str, _items: &[String]| -> Result<PickerOutcome, CliError> {
            panic!("picker must NOT be spawned when no active activity");
        };
        let err = run_here_picker(&mut client, pick, false, false)
            .expect_err("no active activity must fail");
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must be in chain");
        assert_eq!(cli_err.exit_code(), 65);
        match cli_err {
            CliError::MalformedResponse(MalformedResponseSource::Server(msg)) => {
                assert_eq!(msg, "no active activity");
            }
            other => panic!("expected MalformedResponse(Server), got {other:?}"),
        }
        client.assert_consumed_in_order();
    }

    #[test]
    fn run_picker_stage1_new_activity_empty_name_rejects_as_usage_exit_64() {
        // User picks « New activity » and presses Enter on an empty
        // prompt. The CLI MUST reject client-side as
        // CliError::Usage (exit 64) BEFORE any IPC round-trip — the
        // compositor would also reject "" via CreateActivityError::EmptyName,
        // but a CLI-side pre-reject is faster and produces a clearer error.
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![act(1, "Work", true)])),
        );
        // No further IPC expected — the Usage rejection short-circuits
        // before CreateActivity is dispatched.
        let pick = |_prompt: &str, _items: &[String]| -> Result<PickerOutcome, CliError> {
            Ok(PickerOutcome::Selected("« New activity »".into()))
        };
        let prompt = |_prompt: &str| -> Result<NameOutcome, CliError> { Ok(NameOutcome::Unnamed) };
        let err = run_picker(&mut client, pick, prompt, false, false)
            .expect_err("empty-Enter must surface as Usage");
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must be in chain");
        assert_eq!(cli_err.exit_code(), 64);
        match cli_err {
            CliError::Usage(msg) => {
                assert!(
                    msg.contains("must not be empty"),
                    "Usage message must explain the empty-name cause; got: {msg}",
                );
            }
            other => panic!("expected Usage, got {other:?}"),
        }
        client.assert_consumed_in_order();
    }

    // ---- create_activity_via_ipc wire-error matrix -------------------------

    fn create_activity_req(name: &str) -> Request {
        Request::Action(Action::CreateActivity {
            name: name.to_owned(),
        })
    }

    #[test]
    fn create_activity_via_ipc_name_already_exists_maps_to_cant_create() {
        let mut client = MockClient::new();
        client.expect(
            create_activity_req("Work"),
            Err("activity name already exists".to_owned()),
        );
        let err = create_activity_via_ipc(&mut client, "Work").expect_err("collision must fail");
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must be in chain");
        match cli_err {
            CliError::CantCreate(msg) => {
                assert!(
                    msg.contains("already exists"),
                    "message must mention cause; got: {msg}"
                );
            }
            other => panic!("expected CantCreate, got {other:?}"),
        }
        assert_eq!(cli_err.exit_code(), 73);
        client.assert_consumed_in_order();
    }

    #[test]
    fn create_activity_via_ipc_name_must_not_be_empty_maps_to_usage() {
        let mut client = MockClient::new();
        client.expect(
            create_activity_req("   "),
            Err("activity name must not be empty".to_owned()),
        );
        let err = create_activity_via_ipc(&mut client, "   ").expect_err("empty-name must fail");
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must be in chain");
        match cli_err {
            CliError::Usage(msg) => {
                assert_eq!(msg, "activity name must not be empty");
            }
            other => panic!("expected Usage, got {other:?}"),
        }
        assert_eq!(cli_err.exit_code(), 64);
        client.assert_consumed_in_order();
    }

    #[test]
    fn create_activity_via_ipc_name_already_exists_suffixed_routes_to_malformed_response_server() {
        // Pins strict-equality on the DuplicateName wire-string match.
        // A suffixed variant ("activity name already exists: Work") — which
        // the compositor could produce after a Display change — must NOT route
        // to CantCreate; it must fall through to MalformedResponse(Server).
        let mut client = MockClient::new();
        client.expect(
            create_activity_req("Work"),
            Err("activity name already exists: Work".to_owned()),
        );
        let err = create_activity_via_ipc(&mut client, "Work")
            .expect_err("suffixed string must not match CantCreate");
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must be in chain");
        match cli_err {
            CliError::MalformedResponse(MalformedResponseSource::Server(_)) => {}
            other => panic!("expected MalformedResponse(Server), not CantCreate; got {other:?}"),
        }
        assert_eq!(cli_err.exit_code(), 65);
        client.assert_consumed_in_order();
    }

    #[test]
    fn create_activity_via_ipc_name_must_not_be_empty_capitalized_routes_to_malformed_response_server()
     {
        // Pins strict-equality on the EmptyName wire-string match.
        // A capitalized variant ("Activity name must not be empty") must NOT
        // route to Usage — it must fall through to MalformedResponse(Server).
        let mut client = MockClient::new();
        client.expect(
            create_activity_req("   "),
            Err("Activity name must not be empty".to_owned()),
        );
        let err = create_activity_via_ipc(&mut client, "   ")
            .expect_err("capitalized string must not match Usage");
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must be in chain");
        match cli_err {
            CliError::MalformedResponse(MalformedResponseSource::Server(_)) => {}
            other => panic!("expected MalformedResponse(Server), not Usage; got {other:?}"),
        }
        assert_eq!(cli_err.exit_code(), 65);
        client.assert_consumed_in_order();
    }

    #[test]
    fn create_activity_via_ipc_context_wrapper_does_not_bury_cli_error_variants() {
        // Pins that CliError variants survive the `.context("creating activity
        // from move-window picker")` wrapper: `err.chain().find_map(downcast_ref
        // ::<CliError>())` must still find them. This matters for callers that
        // inspect the chain to map to exit codes.
        let mut client = MockClient::new();
        client.expect(
            create_activity_req("Work"),
            Err("activity name already exists".to_owned()),
        );
        let err = create_activity_via_ipc(&mut client, "Work").expect_err("must fail");
        // The anyhow chain must contain the context string.
        let formatted = format!("{err:#}");
        assert!(
            formatted.contains("creating activity from move-window picker"),
            "context string missing from error chain: {formatted}",
        );
        // And the typed CliError must still be findable via downcast.
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must survive .context() wrapping");
        assert!(
            matches!(cli_err, CliError::CantCreate(_)),
            "CantCreate variant must survive .context(); got {cli_err:?}",
        );
        client.assert_consumed_in_order();
    }

    // ---- dispatch_stage2_{with_new,literal_only} ---------------------------

    #[test]
    fn dispatch_stage2_literal_only_empty_filtered_short_circuits_with_diagnostic() {
        // Drive `dispatch_stage2_literal_only` via `run_picker`: pick a
        // non-active activity ('Personal') whose workspaces are not on
        // the focused output. The literal-only path's zero-case
        // short-circuit must fire: eprintln + Ok(()) without spawning
        // stage 2 and without dispatching the move action.
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![
                act(1, "Work", true),
                act(2, "Personal", false),
            ])),
        );
        client.expect(
            Request::Workspaces,
            Reply::Ok(Response::Workspaces(vec![
                ws(10, 0, true, Some("DP-1"), vec![1], None),
                // Activity-2 workspace lives on DP-2, not the focused output.
                ws(20, 0, false, Some("DP-2"), vec![2], None),
            ])),
        );
        let pick = |prompt: &str, _items: &[String]| -> Result<PickerOutcome, CliError> {
            if prompt == "Move window to activity:" {
                Ok(PickerOutcome::Selected("Personal".into()))
            } else {
                panic!("stage 2 must NOT be spawned on literal-only empty zero-case");
            }
        };
        run_picker(&mut client, pick, no_new_activity_prompt, false, false)
            .expect("zero-case must exit Ok");
        client.assert_consumed_in_order();
    }

    #[test]
    fn dispatch_stage2_with_new_returns_server_error_when_trailing_empty_invariant_breached() {
        // Drive `dispatch_stage2_with_new` via `run_here_picker`: the
        // active activity has workspaces on the focused output but ALL
        // of them have an active window (compositor invariant violated).
        // Picking « New workspace » must surface as
        // MalformedResponse(Server("trailing-empty workspace expected
        // for active activity")) → exit 65, NOT panic.
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![act(1, "Work", true)])),
        );
        client.expect(
            Request::Workspaces,
            Reply::Ok(Response::Workspaces(vec![
                // Focused workspace with a window; no trailing-empty.
                ws(10, 0, true, Some("DP-1"), vec![1], Some(99)),
                ws(20, 1, false, Some("DP-1"), vec![1], Some(88)),
            ])),
        );
        let pick = |_prompt: &str, _items: &[String]| -> Result<PickerOutcome, CliError> {
            Ok(PickerOutcome::Selected("« New workspace »".into()))
        };
        let err = run_here_picker(&mut client, pick, false, false)
            .expect_err("trailing-empty breach must surface as MalformedResponse");
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must be in chain");
        assert_eq!(cli_err.exit_code(), 65);
        match cli_err {
            CliError::MalformedResponse(MalformedResponseSource::Server(msg)) => {
                assert_eq!(msg, "trailing-empty workspace expected for active activity");
            }
            other => panic!("expected MalformedResponse(Server), got {other:?}"),
        }
        client.assert_consumed_in_order();
    }

    // ---- capture_focused_window_id / --follow thread-through ---------------

    #[test]
    fn capture_focused_window_id_returns_active_window_when_focused_workspace_has_window() {
        let workspaces = vec![
            ws(1, 0, false, Some("DP-1"), vec![1], None),
            ws(2, 1, true, Some("DP-1"), vec![1], Some(42)),
        ];
        assert_eq!(capture_focused_window_id(&workspaces), Some(42));
    }

    #[test]
    fn capture_focused_window_id_returns_none_when_focused_workspace_has_no_active_window() {
        // Focused workspace exists but `active_window_id: None`. The
        // "empty focused workspace" case the helper must collapse to
        // None (the dispatcher then emits the eprintln fallback).
        let workspaces = vec![ws(1, 0, true, Some("DP-1"), vec![1], None)];
        assert_eq!(capture_focused_window_id(&workspaces), None);
    }

    #[test]
    fn capture_focused_window_id_returns_none_when_no_workspace_is_focused() {
        // Defensive: in production `focused_workspace` already surfaces
        // this as `MalformedResponse(Server("no focused workspace"))`
        // before any caller reaches `capture_focused_window_id`. Pinned
        // here so a refactor that bypasses the `focused_workspace`
        // probe cannot silently turn this into a panic.
        let workspaces = vec![ws(1, 0, false, Some("DP-1"), vec![1], Some(42))];
        assert_eq!(capture_focused_window_id(&workspaces), None);
    }

    #[test]
    fn run_with_follow_and_captured_window_dispatches_move_with_window_id_some() {
        // Named-arg form with `--follow` set and a focused window present.
        // `dispatch_move` must be called with `window_id: Some(99)`; the
        // MockClient queue compares the full Request payload, so a
        // regression to `None` would surface as a request-mismatch
        // failure on the third `send`.
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![
                act(1, "Work", true),
                act(2, "Personal", false),
            ])),
        );
        client.expect(
            Request::Workspaces,
            Reply::Ok(Response::Workspaces(vec![
                ws(10, 0, true, Some("DP-1"), vec![1], Some(99)),
                ws(20, 0, false, Some("DP-1"), vec![2], None),
            ])),
        );
        client.expect(move_req_with_window(20, 99), Reply::Ok(Response::Handled));

        run(&mut client, "Personal", true, false).expect("--follow named-arg succeeds");
        client.assert_consumed_in_order();
    }

    #[test]
    fn run_with_follow_and_no_captured_window_falls_back_to_window_id_none_with_eprintln() {
        // Named-arg form with `--follow` set BUT the focused workspace
        // has `active_window_id: None`. The helper must still dispatch
        // (preserving the user's primary intent) — with `window_id: None`
        // — and `decide_window_id_for_dispatch` will emit a stderr
        // fallback diagnostic that this test does not capture but
        // structurally relies on (covered by the helper's rustdoc).
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![
                act(1, "Work", true),
                act(2, "Personal", false),
            ])),
        );
        client.expect(
            Request::Workspaces,
            Reply::Ok(Response::Workspaces(vec![
                // Focused workspace has NO active window.
                ws(10, 0, true, Some("DP-1"), vec![1], None),
                ws(20, 0, false, Some("DP-1"), vec![2], None),
            ])),
        );
        client.expect(move_req(20), Reply::Ok(Response::Handled));

        run(&mut client, "Personal", true, false).expect("--follow fallback succeeds");
        client.assert_consumed_in_order();
    }

    #[test]
    fn run_without_follow_keeps_window_id_none_regardless_of_active_window() {
        // No-regression pin: `--follow` off must always dispatch with
        // `window_id: None`, even when an `active_window_id` is present
        // in the focused-workspace snapshot. Wire-shape regression on
        // the no-follow path is one of the spec's review-stop
        // conditions.
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![
                act(1, "Work", true),
                act(2, "Personal", false),
            ])),
        );
        client.expect(
            Request::Workspaces,
            Reply::Ok(Response::Workspaces(vec![
                ws(10, 0, true, Some("DP-1"), vec![1], Some(99)),
                ws(20, 0, false, Some("DP-1"), vec![2], None),
            ])),
        );
        // window_id: None, not Some(99) — `--follow` is off.
        client.expect(move_req(20), Reply::Ok(Response::Handled));

        run(&mut client, "Personal", false, false).expect("no-follow named-arg succeeds");
        client.assert_consumed_in_order();
    }

    #[test]
    fn dispatch_stage2_with_new_with_follow_threads_captured_window_id_into_move() {
        // Drive `dispatch_stage2_with_new` via `run_here_picker` with
        // `--follow: true`. The user picks a non-(current) row (id 20);
        // the dispatched `MoveWindowToWorkspace` must carry the
        // captured focused-window id (99).
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![act(1, "Work", true)])),
        );
        client.expect(
            Request::Workspaces,
            Reply::Ok(Response::Workspaces(vec![
                ws(10, 0, true, Some("DP-1"), vec![1], Some(99)),
                ws(20, 1, false, Some("DP-1"), vec![1], None),
            ])),
        );
        client.expect(move_req_with_window(20, 99), Reply::Ok(Response::Handled));

        let pick = |_prompt: &str, items: &[String]| -> Result<PickerOutcome, CliError> {
            // Skip items[0] (the focused row with `(current)`) and pick
            // items[1] to drive an actual dispatch.
            assert_eq!(items[0], "idx 0 (current)");
            Ok(PickerOutcome::Selected(items[1].clone()))
        };
        run_here_picker(&mut client, pick, true, false).expect("--follow stage 2 succeeds");
        client.assert_consumed_in_order();
    }

    #[test]
    fn run_picker_with_follow_threads_window_id_through_literal_only_stage2() {
        // Pin the `run_picker` → `Stage1Resolution::Selected { is_active:
        // false }` → `dispatch_stage2_literal_only` path under `follow: true`.
        // The user-facing entry-point arm (non-active activity literal-only
        // dispatch) was previously exercised only with `follow: false`; a
        // regression that drops the `follow` parameter on this branch would
        // not surface without this test.
        //
        // Fixture: Work (active, id 1), Personal (non-active, id 2).
        // Focused workspace id 10 in Work has active_window_id 99.
        // Personal has one non-focused workspace (id 20) on DP-1 — the
        // literal-only path is selected, and dispatch must carry
        // `window_id: Some(99)`.
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![
                act(1, "Work", true),
                act(2, "Personal", false),
            ])),
        );
        let mut personal_ws = ws(20, 0, false, Some("DP-1"), vec![2], None);
        personal_ws.is_in_active_activity = false;
        client.expect(
            Request::Workspaces,
            Reply::Ok(Response::Workspaces(vec![
                ws(10, 0, true, Some("DP-1"), vec![1], Some(99)),
                personal_ws,
            ])),
        );
        client.expect(move_req_with_window(20, 99), Reply::Ok(Response::Handled));

        let pick = |prompt: &str, items: &[String]| -> Result<PickerOutcome, CliError> {
            if prompt == "Move window to activity:" {
                Ok(PickerOutcome::Selected("Personal".into()))
            } else {
                // Stage 2, literal-only path: no « New workspace » sentinel.
                // Pick the single Personal workspace (id 20, label "id 20").
                assert!(
                    items.iter().all(|s| s != "« New workspace »"),
                    "literal-only path must not offer « New workspace »; items: {items:?}",
                );
                Ok(PickerOutcome::Selected(items[0].clone()))
            }
        };
        run_picker(&mut client, pick, no_new_activity_prompt, true, false)
            .expect("--follow literal-only stage 2 succeeds");
        client.assert_consumed_in_order();
    }

    #[test]
    fn dispatch_stage2_with_new_new_workspace_arm_with_follow_threads_captured_window_id() {
        // Pin the `dispatch_stage2_with_new::NewWorkspace` arm (line
        // 736-737 in the original) under `--follow: true`. The user picks
        // `« New workspace »`; the dispatched `MoveWindowToWorkspace` must
        // carry `window_id: Some(99)` (the focused-window id captured from
        // the snapshot). Drives via `run_here_picker` so the with-new path
        // is guaranteed without constructing the full stage-1 picker chain.
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![act(1, "Work", true)])),
        );
        client.expect(
            Request::Workspaces,
            Reply::Ok(Response::Workspaces(vec![
                // Focused workspace (id 10) with active window 99.
                ws(10, 0, true, Some("DP-1"), vec![1], Some(99)),
                // Trailing-empty workspace (id 20) — what « New workspace » resolves to.
                ws(20, 1, false, Some("DP-1"), vec![1], None),
            ])),
        );
        client.expect(move_req_with_window(20, 99), Reply::Ok(Response::Handled));

        let pick = |_prompt: &str, _items: &[String]| -> Result<PickerOutcome, CliError> {
            Ok(PickerOutcome::Selected("« New workspace »".into()))
        };
        run_here_picker(&mut client, pick, true, false)
            .expect("--follow new-workspace arm succeeds");
        client.assert_consumed_in_order();
    }

    #[test]
    fn run_with_follow_no_focused_workspace_exits_65_before_capture() {
        // Ordering invariant: `focused_output_name?` fires before
        // `decide_window_id_for_dispatch`, so a Workspaces snapshot with no
        // focused workspace must surface as
        // `MalformedResponse(Server("no focused workspace"))` exit 65
        // regardless of whether `--follow` is set. The synthetic error must
        // NOT be swallowed by the capture fallback path (which only fires
        // when `focused_workspace` succeeds but the focused workspace has no
        // active window).
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![
                act(1, "Work", true),
                act(2, "Personal", false),
            ])),
        );
        // No focused workspace in the snapshot.
        client.expect(
            Request::Workspaces,
            Reply::Ok(Response::Workspaces(vec![
                ws(10, 0, false, Some("DP-1"), vec![1], Some(99)),
                ws(20, 0, false, Some("DP-1"), vec![2], None),
            ])),
        );
        let err = run(&mut client, "Personal", true, false)
            .expect_err("no focused workspace must fail with exit 65");
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must be in chain");
        assert_eq!(cli_err.exit_code(), 65);
        match cli_err {
            CliError::MalformedResponse(MalformedResponseSource::Server(msg)) => {
                assert_eq!(
                    msg.as_str(),
                    "no focused workspace",
                    "synthetic error string must be exact; got: {msg}",
                );
            }
            other => panic!("expected MalformedResponse(Server), got {other:?}"),
        }
        // No MoveWindowToWorkspace must have been dispatched.
        client.assert_consumed_in_order();
    }
}
