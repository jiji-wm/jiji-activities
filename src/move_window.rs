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
//! - `Action::MoveWindowToWorkspace` reply handling mirrors the
//!   `move-workspace` matrix via [`send_expect_handled`] (with
//!   `activity_name: None` — there is no activity-name in scope for the
//!   final dispatch, only a workspace id).
//! - Client-side `ActivityNotFound` (named-arg only) is produced by
//!   walking the `Activities` snapshot, mirroring how
//!   `move-workspace` lets the compositor produce it.
//! - Synthetic `MalformedResponse(Server(_))` carriers fire when the
//!   compositor's snapshot violates an invariant we depend on. The four
//!   CLI-internal synthetic strings (not on the wire):
//!   - `"no focused workspace"` — [`focused_workspace`] (via
//!     [`focused_output_name`])
//!   - `"focused workspace has no output"` — [`focused_output_name`]
//!   - `"no active activity"` — [`current_activity_id`]
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

use anyhow::{Context, Result};
use niri_ipc::{Action, Activity, Request, Workspace, WorkspaceReferenceArg};

use crate::error::{CliError, MalformedResponseSource};
use crate::ipc::NiriClient;
use crate::ipc_helpers::{send_expect_activities, send_expect_handled, send_expect_workspaces};
use crate::picker::PickerOutcome;

// ---- Stage sentinels -------------------------------------------------------

// Each stage of the move-window picker carries a single sentinel string:
// the unicode form is preferred, and the underscore-fallback form is
// substituted iff a collision against the user-visible row set is
// detected at composition time. Both sentinels are CLI-internal (never
// emitted on the wire) and resolved by strict string equality in the
// stage resolvers.
//
// Contrast with [`crate::picker::multi_select::SentinelNames`], which is
// a genuine matched pair of distinct sentinels carried atomically — that
// shape is justified there and intentionally retained. Here, each stage
// has only one sentinel, so a plain `&'static str` threads through the
// composer/resolver pair with no abstraction in between.

/// Stage-1 sentinel (preferred unicode form): the `« Current activity »`
/// row that opens stage 2 against the focused activity.
const UNICODE_CURRENT_ACTIVITY: &str = "« Current activity »";

/// Stage-1 sentinel fallback used iff any activity name collides with
/// the unicode form. Selected by [`sentinel_names`] per picker invocation
/// against the live activity name set.
const FALLBACK_CURRENT_ACTIVITY: &str = "__niri_activities_current_activity__";

/// Stage-2 sentinel (preferred unicode form): the `« New workspace »`
/// row that resolves to the active activity's trailing-empty workspace.
const UNICODE_NEW_WORKSPACE: &str = "« New workspace »";

/// Stage-2 sentinel fallback used iff any workspace label collides with
/// the unicode form. Selected by [`workspace_sentinel_names`] per picker
/// invocation against the live workspace label set.
const FALLBACK_NEW_WORKSPACE: &str = "__niri_activities_new_workspace__";

// ---- Stage-resolution enums ------------------------------------------------

/// Post-picker resolution for stage 1.
#[derive(Debug)]
enum Stage1Resolution<'a> {
    CurrentActivity,
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
    Cancelled,
    /// The stage-2 picker returned a label that was not in the items we
    /// passed — a picker-side contract violation. Propagated as
    /// `MalformedResponse(Server(...))` at the call site.
    Unknown(String),
}

/// Post-picker resolution for stage 2 when the `« New workspace »`
/// sentinel is structurally absent from the composed item list
/// (non-active activity path).
///
/// The absence of a `NewWorkspace` variant here is load-bearing: the
/// compositor's trailing-empty invariant only applies to the active
/// activity, so on the non-active path the sentinel was never injected,
/// and there is no `NewWorkspace` outcome to resolve.
#[derive(Debug)]
enum Stage2ResolutionLiteralOnly<'a> {
    Selected(&'a Workspace),
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
///   [`send_expect_handled`].
pub(crate) fn run(client: &mut dyn NiriClient, activity_name: &str) -> Result<()> {
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
    dispatch_move(client, ws.id)
}

/// Two-stage picker form for `move-window`.
///
/// **Contract:**
/// 1. Issues `Request::Activities`. If the list is empty, writes a
///    single-line stderr diagnostic and returns `Ok(())` — the
///    stage-1 picker is never spawned.
/// 2. Opens stage 1 (activity picker) with a `« Current activity »`
///    sentinel as the first row. Cancellation returns `Ok(())`.
/// 3. Dispatches to [`dispatch_stage2_with_new`] (active activity, or
///    `« Current activity »` sentinel) or
///    [`dispatch_stage2_literal_only`] (non-active activity), per the
///    `Stage1Resolution::Selected(activity).is_active` discriminator.
///    The dispatch helpers issue `Request::Workspaces`, filter to
///    workspaces in the chosen activity on the focused output, and
///    manage zero-case diagnostics and sentinel composition.
/// 4. **Zero-case:** on the literal-only path, when the filtered
///    workspace list is empty, the helper writes a stderr diagnostic and
///    returns `Ok(())` — stage 2 is **not** spawned. On the with-new
///    path the `« New workspace »` sentinel covers the empty case so no
///    short-circuit fires there.
/// 5. Opens stage 2 (workspace picker). `« New workspace »` is only
///    offered on the with-new path. Cancellation returns `Ok(())`.
/// 6. Dispatches `Action::MoveWindowToWorkspace { window_id: None,
///    reference: Id(ws.id), focus: false }`.
///
/// The `pick` parameter is a closure (not `FnOnce`) because it is called
/// twice — once per stage. Production wiring passes
/// [`crate::picker::pick_one`].
pub(crate) fn run_picker<F>(client: &mut dyn NiriClient, pick: F) -> Result<()>
where
    F: Fn(&str, &[String]) -> Result<PickerOutcome, CliError>,
{
    let activities = send_expect_activities(client).context("requesting activities")?;
    if activities.is_empty() {
        eprintln!("niri-activities: no activities configured; nothing to move window to");
        return Ok(());
    }

    let activity_name_refs: Vec<&str> = activities.iter().map(|a| a.name.as_str()).collect();
    let stage1_sentinel = sentinel_names(&activity_name_refs);
    let stage1_items = compose_stage1_items(&activities, stage1_sentinel);
    let stage1_picked = pick("Move window to activity:", &stage1_items)?;

    match resolve_stage1(stage1_picked, stage1_sentinel, &activities) {
        Stage1Resolution::Cancelled => Ok(()),
        Stage1Resolution::CurrentActivity => {
            let id = current_activity_id(&activities)?;
            dispatch_stage2_with_new(client, id, &pick)
        }
        // Active activity → with-new path (compositor trailing-empty
        // invariant guarantees a landing slot for « New workspace »).
        // Non-active → literal-only path (no auto-materialised
        // trailing-empty, so no sentinel offered).
        Stage1Resolution::Selected(activity) if activity.is_active => {
            dispatch_stage2_with_new(client, activity.id, &pick)
        }
        Stage1Resolution::Selected(activity) => {
            let name = activity.name.clone();
            dispatch_stage2_literal_only(client, activity.id, &name, &pick)
        }
        Stage1Resolution::Unknown(row) => Err(CliError::MalformedResponse(
            MalformedResponseSource::Server(format!(
                "stage-1 picker returned row not in items: {row:?}"
            )),
        )
        .into()),
    }
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
/// - Reply / variant handling matches [`send_expect_handled`].
pub(crate) fn run_here_picker<F>(client: &mut dyn NiriClient, pick: F) -> Result<()>
where
    F: FnOnce(&str, &[String]) -> Result<PickerOutcome, CliError>,
{
    let activities = send_expect_activities(client).context("requesting activities")?;
    let activity_id = current_activity_id(&activities)?;
    dispatch_stage2_with_new(client, activity_id, pick)
}

// ---- Stage-2 dispatch (with-new vs literal-only) ---------------------------

/// Runs stage 2 against a pinned `target_activity_id` on the with-new
/// path: the `« New workspace »` sentinel is always appended to the
/// composed item list. Selected by callers when the target activity is
/// the currently-active one (compositor trailing-empty invariant
/// guarantees a landing slot exists).
///
/// No `target_activity_name` parameter: the with-new path has no
/// zero-case `eprintln!` diagnostic that would need it. Under the
/// compositor's trailing-empty invariant, `filtered` contains at least
/// the active activity's trailing-empty workspace, so the sentinel always
/// resolves. The synthetic `MalformedResponse(Server("trailing-empty
/// workspace expected for active activity"))` in the `NewWorkspace` arm
/// exists only as a defensive guard against invariant violation — it is
/// not the normal zero-case path.
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
    pick: F,
) -> Result<()>
where
    F: FnOnce(&str, &[String]) -> Result<PickerOutcome, CliError>,
{
    let workspaces = send_expect_workspaces(client).context("requesting workspaces")?;
    let focused_output = focused_output_name(&workspaces)?;
    let filtered =
        workspaces_in_activity_on_focused_output(&workspaces, target_activity_id, focused_output);

    let workspace_labels: Vec<String> = filtered.iter().map(|w| workspace_label(w)).collect();
    let workspace_label_refs: Vec<&str> = workspace_labels.iter().map(String::as_str).collect();
    let stage2_sentinel = workspace_sentinel_names(&workspace_label_refs);
    let stage2_items = compose_stage2_items_with_new(&filtered, stage2_sentinel);

    let stage2_picked = pick("Move window to workspace:", &stage2_items)?;
    match resolve_stage2_with_new(stage2_picked, stage2_sentinel, &filtered) {
        Stage2ResolutionWithNew::Cancelled => Ok(()),
        Stage2ResolutionWithNew::Selected(ws) => dispatch_move(client, ws.id),
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
            dispatch_move(client, ws.id)
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
    pick: F,
) -> Result<()>
where
    F: FnOnce(&str, &[String]) -> Result<PickerOutcome, CliError>,
{
    let workspaces = send_expect_workspaces(client).context("requesting workspaces")?;
    let focused_output = focused_output_name(&workspaces)?;
    let filtered =
        workspaces_in_activity_on_focused_output(&workspaces, target_activity_id, focused_output);

    if filtered.is_empty() {
        eprintln!(
            "niri-activities: activity '{target_activity_name}' has no workspaces on the focused output; nothing to move window to"
        );
        return Ok(());
    }

    let stage2_items = compose_stage2_items_literal_only(&filtered);

    let stage2_picked = pick("Move window to workspace:", &stage2_items)?;
    match resolve_stage2_literal_only(stage2_picked, &filtered) {
        Stage2ResolutionLiteralOnly::Cancelled => Ok(()),
        Stage2ResolutionLiteralOnly::Selected(ws) => dispatch_move(client, ws.id),
        Stage2ResolutionLiteralOnly::Unknown(label) => Err(CliError::MalformedResponse(
            MalformedResponseSource::Server(format!(
                "stage-2 picker returned label not in items: {label:?}"
            )),
        )
        .into()),
    }
}

/// Dispatches the `MoveWindowToWorkspace` action against `ws_id`.
///
/// `window_id: None`, `focus: false`, `reference: Id(ws_id)` are all
/// load-bearing — see module docs for the rationale. The IPC error is
/// wrapped with `.context("moving window to workspace")` so the
/// operation surfaces in the stderr chain.
fn dispatch_move(client: &mut dyn NiriClient, ws_id: u64) -> Result<()> {
    let req = Request::Action(Action::MoveWindowToWorkspace {
        window_id: None,
        reference: WorkspaceReferenceArg::Id(ws_id),
        focus: false,
    });
    send_expect_handled(client, req, None).context("moving window to workspace")
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

/// Returns the id of the currently-active activity, or
/// `MalformedResponse(Server("no active activity"))` when none has
/// `is_active: true`.
///
/// **Synthetic-string discipline.** Same as [`focused_workspace`] —
/// CLI-internal, not on the wire. Defensive: the compositor invariant
/// is that exactly one activity is active at a time, but a
/// hand-constructed test snapshot or a future protocol drift could
/// violate that.
fn current_activity_id(activities: &[Activity]) -> Result<u64, CliError> {
    activities
        .iter()
        .find(|a| a.is_active)
        .map(|a| a.id)
        .ok_or_else(|| {
            CliError::MalformedResponse(MalformedResponseSource::Server(
                "no active activity".to_owned(),
            ))
        })
}

/// Filters `workspaces` to those that belong to `activity_id` and live
/// on `focused_output`. Compositor-supplied order is preserved (we walk
/// the slice; no sort).
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

/// Returns the stage-1 sentinel guaranteed not to collide with any element
/// of `activity_names`. Prefers the unicode form; substitutes the
/// underscore fallback iff a collision would occur.
fn sentinel_names(activity_names: &[&str]) -> &'static str {
    if activity_names.contains(&UNICODE_CURRENT_ACTIVITY) {
        FALLBACK_CURRENT_ACTIVITY
    } else {
        UNICODE_CURRENT_ACTIVITY
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

/// Composes the stage-1 item list: `« Current activity »` (or its
/// underscore fallback) first, then compositor-supplied activity names
/// in their original order.
///
/// **Ordering invariant.** The sentinel is **always** the first row;
/// activity order is **never** reshuffled by `names_focused_first` —
/// the sentinel already covers the focused-activity shortcut.
fn compose_stage1_items(activities: &[Activity], sentinel: &str) -> Vec<String> {
    let mut out = Vec::with_capacity(activities.len() + 1);
    out.push(sentinel.to_owned());
    for a in activities {
        out.push(a.name.clone());
    }
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
fn compose_stage2_items_with_new(workspaces: &[&Workspace], sentinel: &str) -> Vec<String> {
    let mut out: Vec<String> = workspaces.iter().map(|w| workspace_label(w)).collect();
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
fn compose_stage2_items_literal_only(workspaces: &[&Workspace]) -> Vec<String> {
    workspaces.iter().map(|w| workspace_label(w)).collect()
}

/// Resolves the stage-1 picker outcome to one of four branches:
/// cancellation, the `« Current activity »` sentinel, a literal
/// activity selection, or `Unknown` when the stage-1 picker returns a
/// row not in the items we passed (a picker-side contract violation).
///
/// Sentinel match is strict equality against the unicode or
/// underscore-fallback form passed as `sentinel`. Activity match walks
/// the snapshot by name. `Unknown(name)` is returned rather than
/// silently folding contract violations into `Cancelled` so callers
/// can surface the anomaly as `MalformedResponse`.
fn resolve_stage1<'a>(
    picked: PickerOutcome,
    sentinel: &str,
    activities: &'a [Activity],
) -> Stage1Resolution<'a> {
    match picked {
        PickerOutcome::Cancelled => Stage1Resolution::Cancelled,
        PickerOutcome::Selected(name) => {
            if name == sentinel {
                Stage1Resolution::CurrentActivity
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
) -> Stage2ResolutionWithNew<'a> {
    match picked {
        PickerOutcome::Cancelled => Stage2ResolutionWithNew::Cancelled,
        PickerOutcome::Selected(label) => {
            if label == sentinel {
                Stage2ResolutionWithNew::NewWorkspace
            } else if let Some(ws) = candidates
                .iter()
                .find(|w| workspace_label(w) == label)
                .copied()
            {
                Stage2ResolutionWithNew::Selected(ws)
            } else {
                Stage2ResolutionWithNew::Unknown(label)
            }
        }
    }
}

/// Resolves the stage-2 picker outcome on the literal-only path to one
/// of three branches: cancellation, a literal workspace selection, or
/// `Unknown` when the stage-2 picker returns a label not in the items we
/// passed (a picker-side contract violation).
///
/// Takes no `sentinels` argument: on the literal-only path the
/// `« New workspace »` sentinel is structurally absent from the composed
/// item list, so no sentinel match needs to fire.
///
/// `Unknown(label)` is returned rather than silently folding contract
/// violations into `Cancelled` so callers can surface the anomaly as
/// `MalformedResponse`.
fn resolve_stage2_literal_only<'a>(
    picked: PickerOutcome,
    candidates: &'a [&'a Workspace],
) -> Stage2ResolutionLiteralOnly<'a> {
    match picked {
        PickerOutcome::Cancelled => Stage2ResolutionLiteralOnly::Cancelled,
        PickerOutcome::Selected(label) => {
            if let Some(ws) = candidates
                .iter()
                .find(|w| workspace_label(w) == label)
                .copied()
            {
                Stage2ResolutionLiteralOnly::Selected(ws)
            } else {
                Stage2ResolutionLiteralOnly::Unknown(label)
            }
        }
    }
}

/// Renders a workspace as a single-line label for the stage-2 picker menu.
///
/// Format:
/// - Named workspace → `<name> (idx N)`.
/// - Unnamed workspace → `idx N`.
///
/// `idx` is included unconditionally so the user can disambiguate two
/// workspaces that share a name (or none); the compositor invariant that
/// `idx` is only meaningful when `is_in_active_activity == true` is
/// acceptable here because both stage 2 paths filter to workspaces in
/// the chosen activity, and when the chosen activity is non-active the
/// label still distinguishes rows uniquely by `id` ordering — duplicate
/// idx values across non-active workspaces are tolerated as long as the
/// resolution path (which walks the same filtered slice) picks the
/// first match.
fn workspace_label(ws: &Workspace) -> String {
    match &ws.name {
        Some(name) => format!("{name} (idx {})", ws.idx),
        None => format!("idx {}", ws.idx),
    }
}

#[cfg(test)]
mod tests {
    use niri_ipc::{Action, Activity, Reply, Request, Response, Workspace, WorkspaceReferenceArg};

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

    fn move_req(ws_id: u64) -> Request {
        Request::Action(Action::MoveWindowToWorkspace {
            window_id: None,
            reference: WorkspaceReferenceArg::Id(ws_id),
            focus: false,
        })
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

    // ---- current_activity_id -----------------------------------------------

    #[test]
    fn current_activity_id_returns_active_one() {
        let acts = vec![act(1, "Work", false), act(2, "Personal", true)];
        let id = current_activity_id(&acts).expect("active exists");
        assert_eq!(id, 2);
    }

    #[test]
    fn current_activity_id_no_active_routes_to_malformed_server() {
        let acts = vec![act(1, "Work", false)];
        let err = current_activity_id(&acts).expect_err("no active must fail");
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

    // ---- workspace_label ---------------------------------------------------

    #[test]
    fn workspace_label_named_workspace_includes_name_and_idx() {
        let mut w = ws(1, 3, false, Some("DP-1"), vec![1], None);
        w.name = Some("Work".into());
        assert_eq!(workspace_label(&w), "Work (idx 3)");
    }

    #[test]
    fn workspace_label_unnamed_workspace_shows_idx_only() {
        let w = ws(1, 7, false, Some("DP-1"), vec![1], None);
        assert_eq!(workspace_label(&w), "idx 7");
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
        assert_eq!(s, "« Current activity »");
    }

    #[test]
    fn sentinel_names_collision_with_current_activity_uses_underscore_fallback() {
        let names = vec!["Work", "« Current activity »"];
        let s = sentinel_names(&names);
        assert_eq!(s, "__niri_activities_current_activity__");
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
        let sentinel = sentinel_names(&["Work", "Personal"]);
        let items = compose_stage1_items(&acts, sentinel);
        assert_eq!(items[0], "« Current activity »");
        assert_eq!(items[1], "Work");
        assert_eq!(items[2], "Personal");
    }

    #[test]
    fn compose_stage1_items_preserves_compositor_order_no_focused_first_reorder() {
        // 'Personal' is active here, but the sentinel covers the
        // focused-activity shortcut — the activity slice must NOT be
        // reordered to hoist 'Personal' above 'Work'.
        let acts = vec![act(1, "Work", false), act(2, "Personal", true)];
        let sentinel = sentinel_names(&["Work", "Personal"]);
        let items = compose_stage1_items(&acts, sentinel);
        assert_eq!(items, vec!["« Current activity »", "Work", "Personal"]);
    }

    #[test]
    fn compose_stage2_items_with_new_appends_new_workspace_sentinel() {
        let a = ws(1, 0, false, Some("DP-1"), vec![1], None);
        let b = ws(2, 1, false, Some("DP-1"), vec![1], None);
        let filtered = vec![&a, &b];
        let sentinel = workspace_sentinel_names(&["idx 0", "idx 1"]);
        let items = compose_stage2_items_with_new(&filtered, sentinel);
        assert_eq!(items.last().map(String::as_str), Some("« New workspace »"));
        assert_eq!(items.len(), 3);
    }

    #[test]
    fn compose_stage2_items_literal_only_omits_new_workspace_sentinel() {
        let a = ws(1, 0, false, Some("DP-1"), vec![1], None);
        let filtered = vec![&a];
        let items = compose_stage2_items_literal_only(&filtered);
        assert!(items.iter().all(|s| s != "« New workspace »"));
        assert!(
            items
                .iter()
                .all(|s| s != "__niri_activities_new_workspace__")
        );
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn dispatch_stage2_literal_only_does_not_offer_new_workspace_sentinel_to_picker() {
        // Structural pin: compose_stage2_items_literal_only's output
        // contains neither the unicode sentinel nor the underscore
        // fallback, even when the workspace labels would collide.
        let a = ws(1, 0, false, Some("DP-1"), vec![1], None);
        let b = ws(2, 1, false, Some("DP-1"), vec![1], None);
        let filtered = vec![&a, &b];
        let items = compose_stage2_items_literal_only(&filtered);
        assert!(items.iter().all(|s| s != "« New workspace »"));
        assert!(
            items
                .iter()
                .all(|s| s != "__niri_activities_new_workspace__")
        );
    }

    // ---- resolve_stage1 / resolve_stage2_{with_new,literal_only} ----------

    #[test]
    fn resolve_stage1_recognises_current_activity_sentinel_with_underscore_fallback() {
        let acts = vec![act(1, "Work", true)];
        // Force the underscore fallback by passing a colliding name.
        let sentinel = sentinel_names(&["« Current activity »"]);
        assert_eq!(sentinel, "__niri_activities_current_activity__");
        let picked = PickerOutcome::Selected("__niri_activities_current_activity__".into());
        match resolve_stage1(picked, sentinel, &acts) {
            Stage1Resolution::CurrentActivity => {}
            other => panic!("expected CurrentActivity, got {other:?}"),
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
        match resolve_stage2_with_new(picked, sentinel, &filtered) {
            Stage2ResolutionWithNew::NewWorkspace => {}
            other => panic!("expected NewWorkspace, got {other:?}"),
        }
    }

    #[test]
    fn resolve_stage1_unknown_row_returns_unknown_not_cancelled() {
        // Picker returned a row that wasn't in the items (contract
        // violation). Must surface as Unknown, not silently as Cancelled.
        let acts = vec![act(1, "Work", true)];
        let sentinel = sentinel_names(&["Work"]);
        let picked = PickerOutcome::Selected("NotAnActivity".into());
        match resolve_stage1(picked, sentinel, &acts) {
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
        match resolve_stage2_with_new(picked, sentinel, &filtered) {
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
        let picked = PickerOutcome::Selected("not-a-workspace".into());
        match resolve_stage2_literal_only(picked, &filtered) {
            Stage2ResolutionLiteralOnly::Unknown(label) => assert_eq!(label, "not-a-workspace"),
            other => panic!("expected Unknown, got {other:?}"),
        }
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

        run(&mut client, "Personal").expect("named-arg succeeds");
        client.assert_consumed_in_order();
    }

    #[test]
    fn run_named_unknown_activity_maps_to_activity_not_found_exit_66() {
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![act(1, "Work", true)])),
        );
        let err = run(&mut client, "Nope").expect_err("unknown name must fail");
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
        run(&mut client, "Personal").expect("zero-case must exit Ok");
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
        run(&mut client, "Personal").expect("zero-case must exit Ok");
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
        run_picker(&mut client, pick).expect("empty activities must exit Ok");
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
        run_picker(&mut client, pick).expect("stage1 cancel is silent Ok");
        client.assert_consumed_in_order();
    }

    #[test]
    fn run_picker_stage1_current_sentinel_proceeds_to_stage2_with_active_activity() {
        // User picks « Current activity » → stage 2 fires via the
        // with-new path for the active activity (Work, id 1).
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
            Reply::Ok(Response::Workspaces(vec![ws(
                10,
                0,
                true,
                Some("DP-1"),
                vec![1],
                None,
            )])),
        );
        client.expect(move_req(10), Reply::Ok(Response::Handled));

        let pick = |prompt: &str, items: &[String]| -> Result<PickerOutcome, CliError> {
            if prompt == "Move window to activity:" {
                // Stage 1: sentinel first.
                assert_eq!(items[0], "« Current activity »");
                Ok(PickerOutcome::Selected("« Current activity »".into()))
            } else if prompt == "Move window to workspace:" {
                // Stage 2: « New workspace » appended for active activity.
                assert!(items.last().is_some_and(|s| s == "« New workspace »"));
                // Pick the literal workspace label instead of the sentinel.
                Ok(PickerOutcome::Selected(items[0].clone()))
            } else {
                panic!("unexpected prompt: {prompt}");
            }
        };
        run_picker(&mut client, pick).expect("happy path succeeds");
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
        run_picker(&mut client, pick).expect("stage2 cancel is silent Ok");
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
        run_picker(&mut client, pick).expect("new-workspace sentinel succeeds");
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
        run_picker(&mut client, pick).expect("zero-case must exit Ok");
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
        let err = run_picker(&mut client, pick).expect_err("wrong variant must fail");
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
        let err = run_picker(&mut client, pick).expect_err("wrong variant must fail");
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
                // Active activity = 1; one workspace in it on DP-1.
                ws(10, 0, true, Some("DP-1"), vec![1], None),
                // Workspace in activity 2 on DP-1 — MUST NOT be offered.
                ws(20, 0, false, Some("DP-1"), vec![2], None),
            ])),
        );
        client.expect(move_req(10), Reply::Ok(Response::Handled));

        let pick = |prompt: &str, items: &[String]| -> Result<PickerOutcome, CliError> {
            assert_eq!(prompt, "Move window to workspace:");
            // Stage 2 only — no activity prompt. Items: one workspace
            // (activity 1) + « New workspace » sentinel.
            assert_eq!(items.len(), 2, "items: {items:?}");
            assert!(items.last().is_some_and(|s| s == "« New workspace »"));
            Ok(PickerOutcome::Selected(items[0].clone()))
        };
        run_here_picker(&mut client, pick).expect("happy path");
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
        run_here_picker(&mut client, pick).expect("cancellation is silent Ok");
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
        run_here_picker(&mut client, pick).expect("new-workspace sentinel succeeds");
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
        let err = run_here_picker(&mut client, pick).expect_err("no active activity must fail");
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
        run_picker(&mut client, pick).expect("zero-case must exit Ok");
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
        let err = run_here_picker(&mut client, pick)
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
}
