//! Portable Herdr 0.9.0 compatibility contract tests (issue #35).
//!
//! These tests deliberately use public synthetic traces instead of a live
//! Herdr session. Live 0.9.0 evidence is recorded separately by the lane; CI
//! must never inspect or mutate a maintainer's workspaces.

use herdr_fleet::observe::HERDR_MINIMUM;
use herdr_fleet::value::Val;

fn parse(text: &str) -> Val {
    Val::parse_json(text).unwrap_or_else(|error| panic!("fixture JSON: {error}"))
}

fn status_allows_api_calls(status: &Val) -> bool {
    let Some(server) = status.get("server") else {
        return false;
    };
    matches!(server.get("compatible"), Some(Val::Bool(true)))
        && !matches!(server.get("endpoint_compatible"), Some(Val::Bool(false)))
}

#[test]
fn mixed_082_090_protocols_are_explicitly_incompatible() {
    let same_version = parse(
        r#"{"client":{"version":"0.9.0","protocol":22},"server":{"version":"0.9.0","protocol":22,"compatible":true,"endpoint_compatible":true}}"#,
    );
    let old_client = parse(
        r#"{"client":{"version":"0.8.2","protocol":20},"server":{"version":"0.9.0","protocol":22,"compatible":false,"endpoint_compatible":null}}"#,
    );
    let old_server = parse(
        r#"{"client":{"version":"0.9.0","protocol":22},"server":{"version":"0.8.2","protocol":20,"compatible":false,"endpoint_compatible":null}}"#,
    );

    assert!(status_allows_api_calls(&same_version));
    assert!(!status_allows_api_calls(&old_client));
    assert!(!status_allows_api_calls(&old_server));
    assert_eq!(
        HERDR_MINIMUM,
        (0, 8, 2),
        "a red mixed-version matrix does not authorize a doctor-floor bump"
    );
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BootstrapStep {
    SubscribeAck,
    Snapshot(&'static [&'static str]),
    Event(&'static str),
}

fn validate_bootstrap(
    steps: &[BootstrapStep],
    pre_subscription_workspace: &'static str,
    post_subscription_workspace: &'static str,
) -> Result<(), &'static str> {
    let ack = steps
        .iter()
        .position(|step| *step == BootstrapStep::SubscribeAck)
        .ok_or("subscription missing")?;
    let snapshot = steps
        .iter()
        .position(|step| matches!(step, BootstrapStep::Snapshot(_)))
        .ok_or("snapshot missing")?;
    if ack > snapshot {
        return Err("snapshot taken before subscription acknowledgement");
    }
    let BootstrapStep::Snapshot(workspaces) = steps[snapshot] else {
        unreachable!();
    };
    if !workspaces.contains(&pre_subscription_workspace) {
        return Err("snapshot omitted pre-subscription state");
    }
    if steps.contains(&BootstrapStep::Event(pre_subscription_workspace)) {
        return Err("retained history was replayed");
    }
    if !steps.contains(&BootstrapStep::Event(post_subscription_workspace)) {
        return Err("live event missing");
    }
    Ok(())
}

#[test]
fn subscriber_bootstraps_before_snapshot_without_retained_replay() {
    let correct = [
        BootstrapStep::SubscribeAck,
        BootstrapStep::Snapshot(&["primary"]),
        BootstrapStep::Event("live"),
    ];
    assert_eq!(validate_bootstrap(&correct, "primary", "live"), Ok(()));

    let snapshot_first = [
        BootstrapStep::Snapshot(&["primary"]),
        BootstrapStep::SubscribeAck,
        BootstrapStep::Event("live"),
    ];
    assert_eq!(
        validate_bootstrap(&snapshot_first, "primary", "live"),
        Err("snapshot taken before subscription acknowledgement")
    );

    let retained_replay = [
        BootstrapStep::SubscribeAck,
        BootstrapStep::Event("primary"),
        BootstrapStep::Snapshot(&["primary"]),
        BootstrapStep::Event("live"),
    ];
    assert_eq!(
        validate_bootstrap(&retained_replay, "primary", "live"),
        Err("retained history was replayed")
    );
}

#[derive(Debug, PartialEq, Eq)]
struct WorkspaceGroup {
    primary_open: bool,
    worktree_open: bool,
}

impl WorkspaceGroup {
    fn close_primary(&mut self, close_group: bool) -> Result<(), &'static str> {
        if self.worktree_open && !close_group {
            return Err("workspace_group_close_required");
        }
        self.primary_open = false;
        if close_group {
            self.worktree_open = false;
        }
        Ok(())
    }
}

#[test]
fn primary_workspace_close_requires_group_intent() {
    let mut group = WorkspaceGroup {
        primary_open: true,
        worktree_open: true,
    };
    assert_eq!(
        group.close_primary(false),
        Err("workspace_group_close_required")
    );
    assert_eq!(
        group,
        WorkspaceGroup {
            primary_open: true,
            worktree_open: true,
        },
        "the refused default close must leave the whole group open"
    );

    assert_eq!(group.close_primary(true), Ok(()));
    assert_eq!(
        group,
        WorkspaceGroup {
            primary_open: false,
            worktree_open: false,
        }
    );
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PromptStep {
    Submitted,
    Idle,
    Working,
    Blocked,
    Done,
}

fn validate_prompt_wait(steps: &[PromptStep]) -> Result<(), &'static str> {
    if steps.first() != Some(&PromptStep::Submitted) {
        return Err("prompt_not_submitted");
    }
    let activity = steps
        .iter()
        .position(|step| matches!(step, PromptStep::Working | PromptStep::Blocked))
        .ok_or("agent_prompt_stalled")?;
    if steps[activity..].iter().any(|step| {
        matches!(
            step,
            PromptStep::Idle | PromptStep::Done | PromptStep::Blocked
        )
    }) {
        Ok(())
    } else {
        Err("settled_state_missing")
    }
}

#[test]
fn prompt_wait_requires_post_submission_activity() {
    let submitted_but_idle = [PromptStep::Submitted, PromptStep::Idle];
    assert_eq!(
        validate_prompt_wait(&submitted_but_idle),
        Err("agent_prompt_stalled"),
        "submission alone must not satisfy --wait"
    );

    let active_then_settled = [PromptStep::Submitted, PromptStep::Working, PromptStep::Idle];
    assert_eq!(validate_prompt_wait(&active_then_settled), Ok(()));

    let blocked = [PromptStep::Submitted, PromptStep::Blocked];
    assert_eq!(validate_prompt_wait(&blocked), Ok(()));

    let done = [PromptStep::Submitted, PromptStep::Working, PromptStep::Done];
    assert_eq!(validate_prompt_wait(&done), Ok(()));
}

fn pane_read_text(response: &Val) -> Option<&str> {
    response
        .get("result")?
        .get("read")?
        .get("text")?
        .as_str()
        .filter(|text| !text.is_empty())
}

#[test]
fn recent_pane_read_includes_unscrolled_viewport_output() {
    let current = parse(
        r#"{"result":{"read":{"source":"recent","text":"issue35-unscrolled-output\n","truncated":false}}}"#,
    );
    let old_empty_behavior =
        parse(r#"{"result":{"read":{"source":"recent","text":"","truncated":false}}}"#);
    assert_eq!(
        pane_read_text(&current),
        Some("issue35-unscrolled-output\n")
    );
    assert_eq!(pane_read_text(&old_empty_behavior), None);
}

#[test]
fn issue9_runtime_has_no_upstream_retained_replay_dependency() {
    let lifecycle = include_str!("../src/lifecycle.rs");
    let adapters = include_str!("../src/adapters.rs");
    assert!(!lifecycle.contains("\"events.subscribe\""));
    assert!(!adapters.contains("\"events.subscribe\""));
}

#[test]
fn compatibility_document_records_the_measured_matrix() {
    let policy = include_str!("../docs/contracts/compatibility.md");
    for required in [
        "Herdr 0.9.0 compatibility (issue #35)",
        "0.8.2 client → 0.9.0 server",
        "0.9.0 client → 0.8.2 server",
        "workspace_group_close_required",
        "agent_prompt_stalled",
        "live-only",
        "unscrolled",
    ] {
        assert!(
            policy.contains(required),
            "missing policy evidence: {required}"
        );
    }
}
