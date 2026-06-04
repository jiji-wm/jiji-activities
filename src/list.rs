//! `list` subcommand: enumerate activities with three output modes.
//!
//! Routes through three sequential IPC calls in fixed order
//! ([`Request::Activities`] → [`Request::Workspaces`] → [`Request::Windows`])
//! and joins the responses client-side into a single [`ListView`]. Output
//! is then rendered in one of three modes:
//!
//! - **plain** (default) — focus-marker + name + kind + counts.
//! - **`--json`** — versioned envelope `{"schema_version": 1, "activities": [...]}`.
//! - **`--format=<spec>`** — comma-separated subset of `name|kind|focused|
//!   workspace_count|window_count`, one line per activity.
//!
//! The three IPC calls are made unconditionally (even for `--format` specs
//! that wouldn't need windows or workspaces) so the call sequence stays
//! deterministic and easy to assert against in tests.

use std::io::Write;

use anyhow::{Context, Result};
use niri_ipc::{Activity, Request, Response, Window, Workspace};
use serde::Serialize;

use crate::cli::Order;
use crate::error::{CliError, MalformedResponseSource};
use crate::ipc::NiriClient;
use crate::ipc_helpers::{send_expect_activities, send_expect_workspaces, variant_name};

/// Options threaded from [`crate::cli::dispatch`].
pub(crate) struct ListOpts<'a> {
    /// `--json` was passed. Mutually exclusive with `format` (enforced
    /// by clap's `conflicts_with` attribute).
    pub(crate) json: bool,
    /// `--format=<spec>`. `None` means use plain mode.
    pub(crate) format: Option<&'a str>,
    /// `--activity=<name>`. When `Some`, narrow output to the named
    /// activity. An unknown name is rejected immediately after the
    /// `Activities` IPC response with [`CliError::ActivityNotFound`]
    /// (exit 66); no further IPC calls are issued.
    pub(crate) activity: Option<&'a str>,
    /// `--order`. Controls the order of activities in the output.
    /// [`Order::Static`] preserves the compositor-supplied declaration
    /// order; [`Order::Mru`] sorts by `last_active_seq` descending
    /// (stable, so `seq==0` ties retain declaration order).
    pub(crate) order: Order,
}

/// Runs the `list` subcommand against `client`, writing output to `out`.
///
/// **Contract:** issues exactly three IPC requests in order
/// (`Activities`, `Workspaces`, `Windows`); each is matched against its
/// expected `Response` variant. A shape mismatch produces
/// [`CliError::MalformedResponse`] with
/// [`MalformedResponseSource::WrongVariant`].
///
/// `out` is any `Write`. In production this is `std::io::stdout().lock()`
/// (see `cli::cmd_list`); tests pass a `Vec<u8>` to capture.
pub(crate) fn run(
    client: &mut dyn NiriClient,
    opts: ListOpts<'_>,
    out: &mut dyn Write,
) -> Result<()> {
    // Parse the format spec *before* issuing IPC. An invalid spec is a
    // usage error and shouldn't connect to the socket — let it short-
    // circuit with exit 64.
    let fields = match opts.format {
        Some(spec) => Some(parse_format_spec(spec)?),
        None => None,
    };

    let mut activities = send_expect_activities(client).context("requesting activities")?;

    // Validate --activity before issuing any further IPC. An unknown name
    // is a usage error (exit 66); failing fast here means the Workspaces
    // and Windows calls are skipped, which matches the early-exit behaviour
    // of the WrongVariant paths tested in `run_returns_wrong_variant_*`.
    if let Some(name) = opts.activity
        && !activities.iter().any(|a| a.name == name)
    {
        return Err(CliError::ActivityNotFound(name.to_string()).into());
    }

    // Apply MRU ordering before joining so all three render paths see
    // the same activity order. Stable sort: equal seq values (including
    // seq==0) retain their compositor-supplied declaration order.
    if opts.order == Order::Mru {
        activities.sort_by_key(|b| std::cmp::Reverse(b.last_active_seq));
    }

    let workspaces = send_expect_workspaces(client).context("requesting workspaces")?;
    let windows = send_expect_windows(client).context("requesting windows")?;

    let view = join(activities, workspaces, windows);

    // Narrow to the requested activity after join so all three render
    // paths see the same filtered shape uniformly.
    let view = if let Some(name) = opts.activity {
        ListView {
            activities: view
                .activities
                .into_iter()
                .filter(|a| a.name == name)
                .collect(),
        }
    } else {
        view
    };

    if opts.json {
        render_json(&view, out).map_err(classify_write_err)?;
    } else if let Some(fields) = fields {
        render_format(&view, &fields, out).map_err(classify_write_err)?;
    } else {
        render_plain(&view, out)
            .map_err(anyhow::Error::from)
            .map_err(classify_write_err)?;
    }
    out.flush()
        .map_err(anyhow::Error::from)
        .map_err(classify_write_err)?;
    Ok(())
}

// ---- Stdout write-error classifier ------------------------------------------

/// Maps a stdout write/flush error to [`CliError::OutputPipeClosed`] when the
/// root cause is a `BrokenPipe`, leaving all other errors unchanged.
///
/// **Scope contract:** call this only on errors that originate from writing to
/// `out` (the stdout sink). IPC-layer `BrokenPipe`s (compositor crash
/// mid-write) are classified by the IPC module as `SocketUnavailable` and
/// never reach this function.
fn classify_write_err(err: anyhow::Error) -> anyhow::Error {
    use std::io;
    if err
        .root_cause()
        .downcast_ref::<io::Error>()
        .is_some_and(|e| e.kind() == io::ErrorKind::BrokenPipe)
    {
        return CliError::OutputPipeClosed.into();
    }
    err
}

// ---- IPC: typed-variant expectation helpers ---------------------------------

fn send_expect_windows(client: &mut dyn NiriClient) -> Result<Vec<Window>> {
    let resp = client.send(Request::Windows).map_err(CliError::from)?;
    match resp {
        Response::Windows(v) => Ok(v),
        other => Err(wrong_variant("Response::Windows", &other).into()),
    }
}

fn wrong_variant(expected: &'static str, got: &Response) -> CliError {
    CliError::MalformedResponse(MalformedResponseSource::WrongVariant {
        expected,
        got: variant_name(got).into(),
    })
}

// ---- Join: build ListView from the three IPC payloads -----------------------

/// Internal column-projected workspace row.
///
/// `name` is `Option<String>` to round-trip the wire shape verbatim; the
/// JSON renderer emits `null` for unnamed workspaces rather than
/// synthesising from `idx`. Plain output does not surface workspaces
/// individually, so the `Option` only matters for the JSON path.
struct WorkspaceRow {
    id: u64,
    name: Option<String>,
    sticky: bool,
}

enum ActivityKind {
    Config,
    Runtime,
}

impl ActivityKind {
    fn as_str(&self) -> &'static str {
        match self {
            ActivityKind::Config => "config",
            ActivityKind::Runtime => "runtime",
        }
    }
}

struct ActivityRow {
    name: String,
    kind: ActivityKind,
    focused: bool,
    workspaces: Vec<WorkspaceRow>,
    window_count: usize,
}

struct ListView {
    activities: Vec<ActivityRow>,
}

/// Joins the three IPC payloads into a single view.
///
/// Activity order matches `activities` (compositor-supplied); workspace
/// order within each activity matches `workspaces` filtered by
/// membership. Windows with `workspace_id == None` (layer-shell /
/// unbound) are excluded from any activity's `window_count` — they
/// don't belong to a workspace and therefore not to an activity.
fn join(activities: Vec<Activity>, workspaces: Vec<Workspace>, windows: Vec<Window>) -> ListView {
    use std::collections::HashMap;

    // workspace_id → number of windows on that workspace.
    let mut windows_per_ws: HashMap<u64, usize> = HashMap::new();
    for w in &windows {
        if let Some(ws_id) = w.workspace_id {
            *windows_per_ws.entry(ws_id).or_default() += 1;
        }
    }

    let rows = activities
        .into_iter()
        .map(|a| {
            let ws_for_activity: Vec<&Workspace> = workspaces
                .iter()
                .filter(|w| w.activities.contains(&a.id))
                .collect();
            let window_count: usize = ws_for_activity
                .iter()
                .map(|w| windows_per_ws.get(&w.id).copied().unwrap_or(0))
                .sum();
            let ws_rows = ws_for_activity
                .into_iter()
                .map(|w| WorkspaceRow {
                    id: w.id,
                    name: w.name.clone(),
                    sticky: w.is_sticky,
                })
                .collect();
            ActivityRow {
                name: a.name,
                kind: if a.is_config_declared {
                    ActivityKind::Config
                } else {
                    ActivityKind::Runtime
                },
                focused: a.is_active,
                workspaces: ws_rows,
                window_count,
            }
        })
        .collect();

    ListView { activities: rows }
}

// ---- Plain rendering --------------------------------------------------------

fn pluralise<'a>(n: usize, singular: &'a str, plural: &'a str) -> &'a str {
    if n == 1 { singular } else { plural }
}

/// Renders the plain-text mode.
///
/// **Zero-activity case:** writes nothing — no header, no trailing newline.
/// This is achieved naturally because the loop has zero iterations.
fn render_plain(view: &ListView, out: &mut dyn Write) -> std::io::Result<()> {
    // Column widths: longest name + 2-space gutter; longest kind column
    // (always one of "(config)" / "(runtime)"). No truncation.
    let name_width = view
        .activities
        .iter()
        .map(|a| a.name.len())
        .max()
        .unwrap_or(0);
    let kind_width = view
        .activities
        .iter()
        .map(|a| match a.kind {
            ActivityKind::Config => "(config)".len(),
            ActivityKind::Runtime => "(runtime)".len(),
        })
        .max()
        .unwrap_or(0);

    for a in &view.activities {
        let marker = if a.focused { '*' } else { ' ' };
        let kind = match a.kind {
            ActivityKind::Config => "(config)",
            ActivityKind::Runtime => "(runtime)",
        };
        let ws_n = a.workspaces.len();
        let win_n = a.window_count;
        writeln!(
            out,
            "{marker} {name:name_w$}  {kind:kind_w$}  [{ws_n} {ws_noun}, {win_n} {win_noun}]",
            marker = marker,
            name = a.name,
            name_w = name_width,
            kind = kind,
            kind_w = kind_width,
            ws_n = ws_n,
            ws_noun = pluralise(ws_n, "workspace", "workspaces"),
            win_n = win_n,
            win_noun = pluralise(win_n, "window", "windows"),
        )?;
    }
    Ok(())
}

// ---- JSON rendering ---------------------------------------------------------

/// Schema version embedded in the `--json` envelope.
///
/// Bump this when the envelope shape changes in a backward-incompatible
/// way. Consumers should reject envelopes with an unrecognised version.
const SCHEMA_VERSION: u32 = 1;

#[derive(Serialize)]
struct JsonEnvelope<'a> {
    schema_version: u32,
    activities: Vec<JsonActivity<'a>>,
}

#[derive(Serialize)]
struct JsonActivity<'a> {
    name: &'a str,
    kind: &'a str,
    focused: bool,
    workspaces: Vec<JsonWorkspace<'a>>,
    window_count: usize,
}

#[derive(Serialize)]
struct JsonWorkspace<'a> {
    id: u64,
    /// Emits `null` for unnamed workspaces. We deliberately do not
    /// synthesise a name from `idx`.
    name: Option<&'a str>,
    sticky: bool,
}

/// Renders the `--json` mode.
///
/// **Empty-activity case:** still emits the envelope
/// `{"schema_version":1,"activities":[]}` so consumers parse one shape
/// unconditionally.
fn render_json(view: &ListView, out: &mut dyn Write) -> Result<()> {
    let envelope = JsonEnvelope {
        schema_version: SCHEMA_VERSION,
        activities: view
            .activities
            .iter()
            .map(|a| JsonActivity {
                name: &a.name,
                kind: a.kind.as_str(),
                focused: a.focused,
                workspaces: a
                    .workspaces
                    .iter()
                    .map(|w| JsonWorkspace {
                        id: w.id,
                        name: w.name.as_deref(),
                        sticky: w.sticky,
                    })
                    .collect(),
                window_count: a.window_count,
            })
            .collect(),
    };
    serde_json::to_writer_pretty(&mut *out, &envelope).context("serialising activities as JSON")?;
    // `to_writer_pretty` omits the trailing newline. Add one so the
    // output is well-formed for line-oriented consumers (e.g. `jq -r`).
    writeln!(out)?;
    Ok(())
}

// ---- --format=<spec> rendering ----------------------------------------------

#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy)]
enum Field {
    Name,
    Kind,
    Focused,
    WorkspaceCount,
    WindowCount,
}

/// Valid field names accepted by `--format`, in declaration order.
const FORMAT_FIELDS_HINT: &str = "name|kind|focused|workspace_count|window_count";

/// Parses a `--format=<spec>` string into an ordered list of fields.
///
/// Recognised field names (case-sensitive): `name`, `kind`, `focused`,
/// `workspace_count`, `window_count`. Unknown fields and empty specs
/// both produce [`CliError::Usage`] (exit 64). Whitespace around field
/// names is not trimmed — keep the contract strict so `--format=name,
/// kind` is a usage error rather than silent acceptance.
fn parse_format_spec(spec: &str) -> Result<Vec<Field>, CliError> {
    if spec.is_empty() {
        return Err(CliError::Usage(format!(
            "empty --format spec; expected comma-separated subset of {FORMAT_FIELDS_HINT}"
        )));
    }
    let mut seen = std::collections::HashSet::new();
    spec.split(',')
        .map(|f| {
            let field = match f {
                "name" => Ok(Field::Name),
                "kind" => Ok(Field::Kind),
                "focused" => Ok(Field::Focused),
                "workspace_count" => Ok(Field::WorkspaceCount),
                "window_count" => Ok(Field::WindowCount),
                other => Err(CliError::Usage(format!(
                    "unknown field: {other}; expected one of {FORMAT_FIELDS_HINT}"
                ))),
            }?;
            if !seen.insert(field) {
                return Err(CliError::Usage(format!("duplicate field: {f}")));
            }
            Ok(field)
        })
        .collect()
}

/// Renders the `--format=<spec>` mode.
///
/// One line per activity, fields comma-joined verbatim. No escaping for
/// activity names that contain commas — out of scope for v1.
fn render_format(view: &ListView, fields: &[Field], out: &mut dyn Write) -> Result<()> {
    for a in &view.activities {
        let cols: Vec<String> = fields
            .iter()
            .map(|f| match f {
                Field::Name => a.name.clone(),
                Field::Kind => a.kind.as_str().to_string(),
                Field::Focused => a.focused.to_string(),
                Field::WorkspaceCount => a.workspaces.len().to_string(),
                Field::WindowCount => a.window_count.to_string(),
            })
            .collect();
        writeln!(out, "{}", cols.join(","))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use niri_ipc::{Activity, Reply, Request, Response, Window, Workspace};

    use super::*;
    use crate::cli::Order;
    use crate::ipc::MockClient;

    // ---- Sample fixture builders ----

    fn act(id: u64, name: &str, is_active: bool, is_config: bool) -> Activity {
        Activity {
            id,
            name: name.into(),
            is_config_declared: is_config,
            is_active,
            ..Default::default()
        }
    }

    fn ws(id: u64, name: Option<&str>, activities: Vec<u64>, sticky: bool) -> Workspace {
        Workspace {
            id,
            name: name.map(String::from),
            activities,
            is_sticky: sticky,
            ..Default::default()
        }
    }

    fn win(id: u64, ws_id: Option<u64>) -> Window {
        Window {
            id,
            workspace_id: ws_id,
            ..Default::default()
        }
    }

    fn render_to_string<F>(f: F) -> String
    where
        F: FnOnce(&mut Vec<u8>) -> Result<()>,
    {
        let mut buf = Vec::new();
        f(&mut buf).expect("render must succeed");
        String::from_utf8(buf).expect("render output must be utf-8")
    }

    // ---- parse_format_spec ----

    #[test]
    fn parse_format_spec_recognizes_all_fields() {
        let parsed = parse_format_spec("name,kind,focused,workspace_count,window_count")
            .expect("all known fields parse");
        assert_eq!(
            parsed,
            vec![
                Field::Name,
                Field::Kind,
                Field::Focused,
                Field::WorkspaceCount,
                Field::WindowCount,
            ]
        );
    }

    #[test]
    fn parse_format_spec_unknown_field_is_usage() {
        let err = parse_format_spec("name,bogus,kind").expect_err("unknown field rejected");
        assert_eq!(err.exit_code(), 64);
        let msg = format!("{err}");
        assert!(
            msg.contains("unknown field: bogus"),
            "Display must name the offending field: {err}",
        );
        assert!(
            msg.contains("name|kind|focused"),
            "Display must hint the valid field set: {err}",
        );
    }

    #[test]
    fn parse_format_spec_empty_is_usage() {
        // Empty spec is a usage error, not a successful zero-field render.
        let err = parse_format_spec("").expect_err("empty spec rejected");
        assert_eq!(err.exit_code(), 64);
        assert!(
            format!("{err}").contains("name|kind|focused"),
            "empty-spec error must hint the valid field set: {err}",
        );
    }

    #[test]
    fn parse_format_spec_whitespace_treated_as_unknown_field() {
        // The strict-whitespace contract: whitespace is not trimmed, so
        // `"name, kind"` splits into `"name"` and `" kind"` (with leading
        // space). `" kind"` is an unknown field, not a duplicate of `"kind"`.
        let err = parse_format_spec("name, kind").expect_err("space-padded field is rejected");
        assert_eq!(err.exit_code(), 64);
        let msg = format!("{err}");
        assert!(
            msg.contains("unknown field"),
            "space-padded token must be unknown, not duplicate: {err}",
        );
    }

    #[test]
    fn parse_format_spec_rejects_duplicate_field() {
        let err =
            parse_format_spec("name,name,kind").expect_err("duplicate field must be rejected");
        assert_eq!(err.exit_code(), 64);
        assert!(
            format!("{err}").contains("duplicate field: name"),
            "Display must name the duplicated field: {err}",
        );
    }

    // ---- render_plain ----

    #[test]
    fn render_plain_zero_activities_writes_nothing() {
        let view = ListView { activities: vec![] };
        let out = render_to_string(|buf| Ok(render_plain(&view, buf)?));
        assert_eq!(
            out, "",
            "zero activities: empty stdout, no trailing newline"
        );
    }

    #[test]
    fn render_plain_focused_is_starred() {
        let view = ListView {
            activities: vec![
                ActivityRow {
                    name: "Work".into(),
                    kind: ActivityKind::Config,
                    focused: false,
                    workspaces: vec![WorkspaceRow {
                        id: 1,
                        name: None,
                        sticky: false,
                    }],
                    window_count: 3,
                },
                ActivityRow {
                    name: "Personal".into(),
                    kind: ActivityKind::Config,
                    focused: true,
                    workspaces: vec![
                        WorkspaceRow {
                            id: 2,
                            name: None,
                            sticky: false,
                        },
                        WorkspaceRow {
                            id: 3,
                            name: None,
                            sticky: false,
                        },
                    ],
                    window_count: 5,
                },
                ActivityRow {
                    name: "Gaming".into(),
                    kind: ActivityKind::Runtime,
                    focused: false,
                    workspaces: vec![],
                    window_count: 0,
                },
            ],
        };
        let out = render_to_string(|buf| Ok(render_plain(&view, buf)?));
        let expected = "\
            \u{20} Work      (config)   [1 workspace, 3 windows]\n\
            * Personal  (config)   [2 workspaces, 5 windows]\n\
            \u{20} Gaming    (runtime)  [0 workspaces, 0 windows]\n";
        assert_eq!(out, expected, "plain output mismatch\nactual:\n{out}");
    }

    #[test]
    fn render_plain_pluralisation_singular_and_plural() {
        // Pin n=0 (plural), n=1 (singular), n=12 (plural) for both nouns.
        let cases = [
            (0usize, 0usize, "[0 workspaces, 0 windows]"),
            (1, 1, "[1 workspace, 1 window]"),
            (2, 12, "[2 workspaces, 12 windows]"),
        ];
        for (ws_n, win_n, tail) in cases {
            let view = ListView {
                activities: vec![ActivityRow {
                    name: "X".into(),
                    kind: ActivityKind::Config,
                    focused: false,
                    workspaces: (0..ws_n)
                        .map(|i| WorkspaceRow {
                            id: i as u64,
                            name: None,
                            sticky: false,
                        })
                        .collect(),
                    window_count: win_n,
                }],
            };
            let out = render_to_string(|buf| Ok(render_plain(&view, buf)?));
            assert!(
                out.contains(tail),
                "n=({ws_n},{win_n}): expected suffix `{tail}`, got `{out}`",
            );
        }
    }

    #[test]
    fn render_plain_column_widths_handle_long_names() {
        // Long names just widen the column. No truncation; column 2
        // (the kind label) must still align across rows.
        let view = ListView {
            activities: vec![
                ActivityRow {
                    name: "A".into(),
                    kind: ActivityKind::Config,
                    focused: false,
                    workspaces: vec![],
                    window_count: 0,
                },
                ActivityRow {
                    name: "VeryLongActivityName".into(),
                    kind: ActivityKind::Runtime,
                    focused: false,
                    workspaces: vec![],
                    window_count: 0,
                },
            ],
        };
        let out = render_to_string(|buf| Ok(render_plain(&view, buf)?));
        // Both kind labels should start at the same column. Find their
        // byte-offset within each line (offsets are ASCII-safe here).
        let lines: Vec<&str> = out.lines().collect();
        let offset_a = lines[0].find("(config)").expect("col 2 in line 0");
        let offset_b = lines[1].find("(runtime)").expect("col 2 in line 1");
        assert_eq!(
            offset_a, offset_b,
            "kind column must align across rows; got offsets {offset_a} vs {offset_b}",
        );
        // And no truncation of the long name.
        assert!(
            lines[1].contains("VeryLongActivityName"),
            "long name must be preserved verbatim: {}",
            lines[1],
        );
    }

    // ---- render_json ----

    #[test]
    fn render_json_zero_activities_emits_envelope() {
        let view = ListView { activities: vec![] };
        let out = render_to_string(|buf| render_json(&view, buf));
        let parsed: serde_json::Value =
            serde_json::from_str(&out).expect("output must be valid JSON");
        assert_eq!(parsed["schema_version"], 1);
        assert!(parsed["activities"].is_array());
        assert_eq!(parsed["activities"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn render_json_full_shape_round_trips_named_workspace() {
        let view = ListView {
            activities: vec![ActivityRow {
                name: "Work".into(),
                kind: ActivityKind::Config,
                focused: true,
                workspaces: vec![WorkspaceRow {
                    id: 1,
                    name: Some("main".into()),
                    sticky: false,
                }],
                window_count: 12,
            }],
        };
        let out = render_to_string(|buf| render_json(&view, buf));
        let parsed: serde_json::Value =
            serde_json::from_str(&out).expect("output must be valid JSON");
        assert_eq!(parsed["schema_version"], 1);
        let act0 = &parsed["activities"][0];
        assert_eq!(act0["name"], "Work");
        assert_eq!(act0["kind"], "config");
        assert_eq!(act0["focused"], true);
        assert_eq!(act0["window_count"], 12);
        assert_eq!(act0["workspaces"][0]["id"], 1);
        assert_eq!(act0["workspaces"][0]["name"], "main");
        assert_eq!(act0["workspaces"][0]["sticky"], false);
    }

    #[test]
    fn render_json_unnamed_workspace_emits_null() {
        // Pin the decision: unnamed workspaces emit `null` rather than
        // synthesising a name from `idx`.
        let view = ListView {
            activities: vec![ActivityRow {
                name: "X".into(),
                kind: ActivityKind::Runtime,
                focused: false,
                workspaces: vec![WorkspaceRow {
                    id: 7,
                    name: None,
                    sticky: false,
                }],
                window_count: 0,
            }],
        };
        let out = render_to_string(|buf| render_json(&view, buf));
        let parsed: serde_json::Value =
            serde_json::from_str(&out).expect("output must be valid JSON");
        assert!(
            parsed["activities"][0]["workspaces"][0]["name"].is_null(),
            "unnamed workspace should serialise as null: {parsed}",
        );
    }

    // ---- render_format ----

    #[test]
    fn render_format_name_kind_focused() {
        let view = ListView {
            activities: vec![
                ActivityRow {
                    name: "Work".into(),
                    kind: ActivityKind::Config,
                    focused: false,
                    workspaces: vec![],
                    window_count: 0,
                },
                ActivityRow {
                    name: "Personal".into(),
                    kind: ActivityKind::Config,
                    focused: true,
                    workspaces: vec![],
                    window_count: 0,
                },
                ActivityRow {
                    name: "Gaming".into(),
                    kind: ActivityKind::Runtime,
                    focused: false,
                    workspaces: vec![],
                    window_count: 0,
                },
            ],
        };
        let fields = vec![Field::Name, Field::Kind, Field::Focused];
        let out = render_to_string(|buf| render_format(&view, &fields, buf));
        assert_eq!(
            out,
            "Work,config,false\nPersonal,config,true\nGaming,runtime,false\n",
        );
    }

    #[test]
    fn render_format_workspace_count_and_window_count() {
        // Pins the two count arms. workspace_count derives from
        // `workspaces.len()`; window_count is the pre-joined sum.
        let view = ListView {
            activities: vec![ActivityRow {
                name: "Work".into(),
                kind: ActivityKind::Config,
                focused: false,
                workspaces: vec![
                    WorkspaceRow {
                        id: 1,
                        name: None,
                        sticky: false,
                    },
                    WorkspaceRow {
                        id: 2,
                        name: None,
                        sticky: false,
                    },
                ],
                window_count: 5,
            }],
        };
        let fields = vec![Field::WorkspaceCount, Field::WindowCount];
        let out = render_to_string(|buf| render_format(&view, &fields, buf));
        assert_eq!(out, "2,5\n");
    }

    #[test]
    fn render_format_zero_activities_writes_nothing() {
        let view = ListView { activities: vec![] };
        let fields = vec![Field::Name, Field::Kind];
        let out = render_to_string(|buf| render_format(&view, &fields, buf));
        assert_eq!(
            out, "",
            "zero activities: empty stdout, no trailing newline"
        );
    }

    // ---- join ----

    #[test]
    fn join_partitions_workspaces_and_windows_correctly() {
        // Two activities; a sticky workspace tagged with both; one
        // workspace tagged with only the second; one stray window
        // with `workspace_id == None` (excluded from every count).
        let activities = vec![
            act(10, "Work", true, true),
            act(20, "Personal", false, false),
        ];
        let workspaces = vec![
            ws(1, Some("a"), vec![10, 20], true), // sticky, both
            ws(2, Some("b"), vec![20], false),    // Personal-only
            ws(3, None, vec![], false),           // orphan: in no activity
        ];
        let windows = vec![
            win(100, Some(1)), // belongs to sticky ws → both activities
            win(101, Some(2)), // Personal only
            win(102, Some(2)), // Personal only
            win(103, None),    // unbound; excluded
        ];
        let view = join(activities, workspaces, windows);
        assert_eq!(view.activities.len(), 2);

        let work = &view.activities[0];
        assert_eq!(work.name, "Work");
        assert!(work.focused);
        assert!(matches!(work.kind, ActivityKind::Config));
        assert_eq!(work.workspaces.len(), 1, "sticky ws shared with Personal");
        assert_eq!(work.workspaces[0].id, 1);
        assert!(work.workspaces[0].sticky);
        assert_eq!(work.window_count, 1, "only window 100 lives on ws 1");

        let personal = &view.activities[1];
        assert_eq!(personal.name, "Personal");
        assert!(!personal.focused);
        assert!(matches!(personal.kind, ActivityKind::Runtime));
        assert_eq!(personal.workspaces.len(), 2, "sticky + own ws");
        assert_eq!(personal.window_count, 3, "window 100 + 101 + 102");
    }

    // ---- run: end-to-end through MockClient ----

    #[test]
    fn run_returns_wrong_variant_on_shape_mismatch() {
        // Queue a Version reply against an Activities request — the
        // first IPC call should fail with WrongVariant.
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Version("bogus".into())),
        );
        let mut buf = Vec::new();
        let err = run(
            &mut client,
            ListOpts {
                json: false,
                format: None,
                activity: None,
                order: Order::Static,
            },
            &mut buf,
        )
        .expect_err("must fail with WrongVariant");
        // Walk the chain to recover the typed CliError.
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must remain downcastable through .context wrap");
        match cli_err {
            CliError::MalformedResponse(MalformedResponseSource::WrongVariant {
                expected,
                got,
            }) => {
                assert_eq!(*expected, "Response::Activities");
                assert_eq!(got, "Response::Version");
            }
            other => panic!("expected WrongVariant, got {other:?}"),
        }
        // Verify the `.context("requesting activities")` layer appears in
        // the formatted chain.
        assert!(
            format!("{err:#}").contains("requesting activities"),
            "context string missing from chain: {err:#}",
        );
        // The queue entry was consumed by the failing Activities call.
        // No Workspaces or Windows call should have been issued; assert
        // the count is exactly zero rather than relying on the implicit
        // panic-on-drop path so a regression (e.g. swallowed WrongVariant
        // that continues to the next IPC call) surfaces a precise failure.
        assert_eq!(
            client.remaining_count(),
            0,
            "no IPC calls should follow a WrongVariant error",
        );
    }

    #[test]
    fn run_returns_wrong_variant_on_workspaces_shape_mismatch() {
        // The Activities call succeeds; the Workspaces call returns the
        // wrong variant. WrongVariant must name "Response::Workspaces".
        let mut client = MockClient::new();
        client.expect(Request::Activities, Reply::Ok(Response::Activities(vec![])));
        client.expect(
            Request::Workspaces,
            Reply::Ok(Response::Version("bogus".into())),
        );
        let mut buf = Vec::new();
        let err = run(
            &mut client,
            ListOpts {
                json: false,
                format: None,
                activity: None,
                order: Order::Static,
            },
            &mut buf,
        )
        .expect_err("must fail with WrongVariant on Workspaces");
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must be in chain");
        match cli_err {
            CliError::MalformedResponse(MalformedResponseSource::WrongVariant {
                expected,
                got,
            }) => {
                assert_eq!(*expected, "Response::Workspaces");
                assert_eq!(got, "Response::Version");
            }
            other => panic!("expected WrongVariant(Response::Workspaces), got {other:?}"),
        }
        // Verify the `.context("requesting workspaces")` layer appears in
        // the formatted chain.
        assert!(
            format!("{err:#}").contains("requesting workspaces"),
            "context string missing from chain: {err:#}",
        );
        assert_eq!(
            client.remaining_count(),
            0,
            "Windows call must not be attempted after Workspaces mismatch",
        );
    }

    #[test]
    fn run_returns_wrong_variant_on_windows_shape_mismatch() {
        // Activities and Workspaces succeed; the Windows call returns
        // the wrong variant. WrongVariant must name "Response::Windows".
        let mut client = MockClient::new();
        client.expect(Request::Activities, Reply::Ok(Response::Activities(vec![])));
        client.expect(Request::Workspaces, Reply::Ok(Response::Workspaces(vec![])));
        client.expect(
            Request::Windows,
            Reply::Ok(Response::Version("bogus".into())),
        );
        let mut buf = Vec::new();
        let err = run(
            &mut client,
            ListOpts {
                json: false,
                format: None,
                activity: None,
                order: Order::Static,
            },
            &mut buf,
        )
        .expect_err("must fail with WrongVariant on Windows");
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must be in chain");
        match cli_err {
            CliError::MalformedResponse(MalformedResponseSource::WrongVariant {
                expected,
                got,
            }) => {
                assert_eq!(*expected, "Response::Windows");
                assert_eq!(got, "Response::Version");
            }
            other => panic!("expected WrongVariant(Response::Windows), got {other:?}"),
        }
        assert!(
            format!("{err:#}").contains("requesting windows"),
            "context string missing from chain: {err:#}",
        );
        assert_eq!(client.remaining_count(), 0);
    }

    #[test]
    fn run_happy_path_plain_consumes_all_three_calls() {
        // Pins the production-side IPC ordering: Activities, then
        // Workspaces, then Windows. MockClient panics on out-of-order
        // requests, so this test would fail noisily on a regression.
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![act(1, "Work", true, true)])),
        );
        client.expect(
            Request::Workspaces,
            Reply::Ok(Response::Workspaces(vec![ws(10, None, vec![1], false)])),
        );
        client.expect(
            Request::Windows,
            Reply::Ok(Response::Windows(vec![win(100, Some(10))])),
        );
        let mut buf = Vec::new();
        run(
            &mut client,
            ListOpts {
                json: false,
                format: None,
                activity: None,
                order: Order::Static,
            },
            &mut buf,
        )
        .expect("run must succeed");
        client.assert_consumed_in_order();
        let out = String::from_utf8(buf).expect("utf-8 stdout");
        assert!(out.starts_with("* Work"), "output: {out}");
        assert!(out.contains("[1 workspace, 1 window]"), "output: {out}");
    }

    fn queue_three_happy_replies(client: &mut MockClient) {
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![act(1, "Work", true, true)])),
        );
        client.expect(
            Request::Workspaces,
            Reply::Ok(Response::Workspaces(vec![ws(
                10,
                Some("main"),
                vec![1],
                false,
            )])),
        );
        client.expect(
            Request::Windows,
            Reply::Ok(Response::Windows(vec![win(100, Some(10))])),
        );
    }

    #[test]
    fn run_happy_path_json_routes_through_render_json() {
        // Verifies that `--json` triggers the JSON branch: output starts
        // with `{` and contains the schema_version key.
        let mut client = MockClient::new();
        queue_three_happy_replies(&mut client);
        let mut buf = Vec::new();
        run(
            &mut client,
            ListOpts {
                json: true,
                format: None,
                activity: None,
                order: Order::Static,
            },
            &mut buf,
        )
        .expect("run --json must succeed");
        client.assert_consumed_in_order();
        let out = String::from_utf8(buf).expect("utf-8 stdout");
        assert!(
            out.trim_start().starts_with('{'),
            "--json output must be JSON-shaped; got: {out}",
        );
        assert!(
            out.contains("schema_version"),
            "--json output must contain schema_version; got: {out}",
        );
    }

    #[test]
    fn run_happy_path_format_routes_through_render_format() {
        // Verifies that `--format=name` triggers the format branch:
        // output is plain text containing the activity name, not `{`.
        let mut client = MockClient::new();
        queue_three_happy_replies(&mut client);
        let mut buf = Vec::new();
        run(
            &mut client,
            ListOpts {
                json: false,
                format: Some("name"),
                activity: None,
                order: Order::Static,
            },
            &mut buf,
        )
        .expect("run --format=name must succeed");
        client.assert_consumed_in_order();
        let out = String::from_utf8(buf).expect("utf-8 stdout");
        assert!(
            !out.trim_start().starts_with('{'),
            "--format output must not be JSON-shaped; got: {out}",
        );
        assert!(
            out.contains("Work"),
            "--format=name output must contain activity name; got: {out}",
        );
    }

    // ---- --activity filter ----

    /// Fixture: two activities ("Work" focused, "Personal" not), one
    /// workspace per activity, one window each.
    fn queue_two_activity_replies(client: &mut MockClient) {
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![
                act(1, "Work", true, true),
                act(2, "Personal", false, true),
            ])),
        );
        client.expect(
            Request::Workspaces,
            Reply::Ok(Response::Workspaces(vec![
                ws(10, None, vec![1], false),
                ws(20, None, vec![2], false),
            ])),
        );
        client.expect(
            Request::Windows,
            Reply::Ok(Response::Windows(vec![
                win(100, Some(10)),
                win(200, Some(20)),
            ])),
        );
    }

    #[test]
    fn run_activity_filter_narrows_plain() {
        // --activity Work → plain output contains only "Work", not "Personal".
        let mut client = MockClient::new();
        queue_two_activity_replies(&mut client);
        let mut buf = Vec::new();
        run(
            &mut client,
            ListOpts {
                json: false,
                format: None,
                activity: Some("Work"),
                order: Order::Static,
            },
            &mut buf,
        )
        .expect("run --activity Work must succeed");
        client.assert_consumed_in_order();
        let out = String::from_utf8(buf).expect("utf-8 stdout");
        assert!(out.contains("Work"), "Work must appear in output: {out}");
        assert!(
            !out.contains("Personal"),
            "Personal must not appear in filtered output: {out}",
        );
    }

    #[test]
    fn run_activity_filter_narrows_json() {
        // --activity Work with --json → envelope contains exactly one activity.
        let mut client = MockClient::new();
        queue_two_activity_replies(&mut client);
        let mut buf = Vec::new();
        run(
            &mut client,
            ListOpts {
                json: true,
                format: None,
                activity: Some("Work"),
                order: Order::Static,
            },
            &mut buf,
        )
        .expect("run --activity Work --json must succeed");
        client.assert_consumed_in_order();
        let out = String::from_utf8(buf).expect("utf-8 stdout");
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("output must be JSON");
        let acts = parsed["activities"]
            .as_array()
            .expect("activities must be array");
        assert_eq!(acts.len(), 1, "exactly one activity after filter: {out}");
        assert_eq!(
            acts[0]["name"], "Work",
            "filtered activity must be Work: {out}"
        );
    }

    #[test]
    fn run_activity_filter_narrows_format() {
        // --activity Work with --format=name → only "Work\n" on stdout.
        let mut client = MockClient::new();
        queue_two_activity_replies(&mut client);
        let mut buf = Vec::new();
        run(
            &mut client,
            ListOpts {
                json: false,
                format: Some("name"),
                activity: Some("Work"),
                order: Order::Static,
            },
            &mut buf,
        )
        .expect("run --activity Work --format=name must succeed");
        client.assert_consumed_in_order();
        let out = String::from_utf8(buf).expect("utf-8 stdout");
        assert_eq!(out, "Work\n", "filtered --format=name output: {out}");
    }

    #[test]
    fn run_activity_unknown_returns_not_found() {
        // An unknown --activity name must fail immediately after Activities
        // (no Workspaces or Windows IPC call issued) with ActivityNotFound.
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![act(1, "Work", true, true)])),
        );
        // Intentionally no Workspaces or Windows queue entries — the
        // remaining_count() assert below enforces they were not issued.
        let mut buf = Vec::new();
        let err = run(
            &mut client,
            ListOpts {
                json: false,
                format: None,
                activity: Some("Bogus"),
                order: Order::Static,
            },
            &mut buf,
        )
        .expect_err("unknown activity must be an error");
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must be in chain");
        match cli_err {
            CliError::ActivityNotFound(name) => {
                assert_eq!(name, "Bogus", "error must name the unknown activity");
            }
            other => panic!("expected ActivityNotFound, got {other:?}"),
        }
        // Chain-walk must recover exit code 66.
        assert_eq!(
            err.chain()
                .find_map(|e| e.downcast_ref::<CliError>())
                .map(|e| e.exit_code()),
            Some(66),
            "ActivityNotFound must produce exit code 66",
        );
        assert_eq!(
            client.remaining_count(),
            0,
            "no Workspaces/Windows call should be issued for an unknown activity",
        );
    }

    // ---- --order flag ----

    fn act_seq(id: u64, name: &str, is_active: bool, seq: u64) -> Activity {
        Activity {
            id,
            name: name.into(),
            is_config_declared: true,
            is_active,
            last_active_seq: seq,
            ..Default::default()
        }
    }

    #[test]
    fn list_mru_orders_by_recency() {
        // MRU: Gaming (seq=3) before Personal (seq=1) before Work (seq=0).
        // Work appears last because it was never activated (seq=0).
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![
                act_seq(1, "Work", false, 0),
                act_seq(2, "Personal", true, 1),
                act_seq(3, "Gaming", false, 3),
            ])),
        );
        client.expect(Request::Workspaces, Reply::Ok(Response::Workspaces(vec![])));
        client.expect(Request::Windows, Reply::Ok(Response::Windows(vec![])));
        let mut buf = Vec::new();
        run(
            &mut client,
            ListOpts {
                json: false,
                format: Some("name"),
                activity: None,
                order: Order::Mru,
            },
            &mut buf,
        )
        .expect("list --order=mru must succeed");
        client.assert_consumed_in_order();
        let out = String::from_utf8(buf).expect("utf-8 stdout");
        let names: Vec<&str> = out.lines().collect();
        assert_eq!(
            names,
            vec!["Gaming", "Personal", "Work"],
            "MRU order must be Gaming (seq=3), Personal (seq=1), Work (seq=0); got: {names:?}",
        );
    }

    #[test]
    fn list_static_is_declaration_order() {
        // Static: declaration order (Work, Personal, Gaming) preserved
        // regardless of last_active_seq values.
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![
                act_seq(1, "Work", false, 0),
                act_seq(2, "Personal", true, 1),
                act_seq(3, "Gaming", false, 3),
            ])),
        );
        client.expect(Request::Workspaces, Reply::Ok(Response::Workspaces(vec![])));
        client.expect(Request::Windows, Reply::Ok(Response::Windows(vec![])));
        let mut buf = Vec::new();
        run(
            &mut client,
            ListOpts {
                json: false,
                format: Some("name"),
                activity: None,
                order: Order::Static,
            },
            &mut buf,
        )
        .expect("list --order=static must succeed");
        client.assert_consumed_in_order();
        let out = String::from_utf8(buf).expect("utf-8 stdout");
        let names: Vec<&str> = out.lines().collect();
        assert_eq!(
            names,
            vec!["Work", "Personal", "Gaming"],
            "Static order must match declaration order; got: {names:?}",
        );
    }
}
