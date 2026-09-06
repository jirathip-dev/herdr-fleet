//! Workflow engine core (issue #6): typed DAG analysis, route-grant rules,
//! review-round policy, pause/resume digests, and the bundled Doctrine
//! default workflow.
//!
//! This module is deterministic and side-effect free: it validates and
//! decides, and the daemon owns every transition and durable write (the
//! engine never mutates anything by itself). LLM/advisory output is a typed
//! step that returns proposals/evidence with NO direct transition authority:
//! every transition in this slice is computed by the daemon-owned engine
//! functions below from closed inputs, and no document field can carry
//! policy, capabilities, target, or approval authority.
//!
//! Node kinds are the closed `hf-workflow/v1` set; a document that passes
//! the family validator (see [`crate::schema`]) is still subject to the
//! engine checks here: acyclicity, boundedness, node-reference integrity,
//! privilege/role rules, digest pinning, and changed-in-flight detection.

use std::collections::BTreeMap;

use crate::canonical::{canonical_bytes, sha256_hex};
use crate::value::{Val, integer, object, string};

/// Closed hf-workflow/v1 node kinds (spec-workflow.md; mirrors the probe).
pub const NODE_KINDS: [&str; 9] = [
    "start",
    "plan",
    "orchestrator",
    "implementer",
    "reviewer",
    "gate",
    "human_approval",
    "merge",
    "terminal",
];

/// Default model-agnostic roles (ADR-0003: never encode a model/provider).
pub const DEFAULT_ROLES: [&str; 3] = ["orchestrator", "implementer", "reviewer"];

/// Authority-bearing parameter names. A workflow node may never carry these;
/// capability/effect/risk/approval axes are daemon-owned (risk-model.md).
const AUTHORITY_PARAM_KEYS: [&str; 10] = [
    "caps",
    "capabilities",
    "risk",
    "effects",
    "policy",
    "policy_hash",
    "approval",
    "authority",
    "production",
    "release",
];

/// Normal review/fix rounds that run automatically (AC4).
pub const NORMAL_REVIEW_ROUNDS: u32 = 3;
/// Separately authorized recovery rounds (AC4).
pub const RECOVERY_ROUNDS: u32 = 1;
/// Doctrine default workflow id (versioned under hf-workflow/v1).
pub const DOCTRINE_WORKFLOW_ID: &str = "fleet-doctrine-1";
/// Pinned sha256 (lowercase hex) over the canonical doctrine document
/// (`schemas/fixtures/workflow/workflow.doctrine.json`; manifest-pinned).
pub const DOCTRINE_DIGEST: &str =
    "88c368277cb8a19bb9e32d6b7e56a8307a3b72eaad2939028efd5dfcc850eb6c";
/// The bundled Doctrine default workflow document (canonical JSON bytes,
/// embedded from the fixture corpus so the repo file stays the one source).
pub const DOCTRINE_DOCUMENT: &str =
    include_str!("../schemas/fixtures/workflow/workflow.doctrine.json");

/// Parse and analyze the bundled Doctrine default workflow (issue-bound:
/// isolated implementation -> exact-head review -> hosted CI -> integration
/// merge -> post-merge verification -> closure; production is separate).
pub fn doctrine_default() -> Result<WorkflowShape, EngineError> {
    let doc = Val::parse_json(DOCTRINE_DOCUMENT).map_err(|message| {
        EngineError::new(
            "engine.doctrine_invalid",
            format!("doctrine parse: {message}"),
        )
    })?;
    let shape = analyze(&doc)?;
    if shape.digest != DOCTRINE_DIGEST {
        return Err(EngineError::new(
            "engine.doctrine_digest",
            "bundled doctrine digest drifted from its pinned value",
        ));
    }
    Ok(shape)
}

/// Typed engine refusal/error codes (stable dotted surface).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineError {
    /// Stable dotted code (`engine.*`).
    pub code: &'static str,
    /// Human-readable reason.
    pub message: String,
}

impl EngineError {
    fn new(code: &'static str, message: impl Into<String>) -> EngineError {
        EngineError {
            code,
            message: message.into(),
        }
    }

    /// `engine.cyclic` — the graph contains a directed cycle.
    pub fn cyclic() -> EngineError {
        EngineError::new("engine.cyclic", "workflow graph contains a cycle")
    }

    /// `engine.unbounded` — not every reachable path terminates or a node is
    /// unreachable from the start node.
    pub fn unbounded() -> EngineError {
        EngineError::new(
            "engine.unbounded",
            "workflow is unbounded: a node is unreachable from start or cannot reach a terminal",
        )
    }

    /// `engine.unknown_node` — an edge endpoint does not exist.
    pub fn unknown_node(node: &str) -> EngineError {
        EngineError::new(
            "engine.unknown_node",
            format!("edge references unknown node {node:?}"),
        )
    }

    /// `engine.privilege` — workflow content attempts to carry authority.
    pub fn privilege(detail: impl Into<String>) -> EngineError {
        EngineError::new("engine.privilege", detail.into())
    }

    /// `engine.hash_mismatch` — the config pin's hash does not match the
    /// canonical digest of the loaded workflow document.
    pub fn hash_mismatch() -> EngineError {
        EngineError::new(
            "engine.hash_mismatch",
            "configured workflow pin hash does not match the document digest",
        )
    }

    /// `engine.changed_in_flight` — an instance's pinned workflow hash no
    /// longer matches the document offered for a transition.
    pub fn changed_in_flight() -> EngineError {
        EngineError::new(
            "engine.changed_in_flight",
            "workflow document changed after the instance pinned its hash",
        )
    }

    /// `engine.closed_role` — a role reference outside the default or
    /// hash-pinned custom set.
    pub fn closed_role(role: &str) -> EngineError {
        EngineError::new(
            "engine.closed_role",
            format!("role {role:?} is not a default role or a hash-pinned custom role"),
        )
    }

    /// `engine.review_exhausted` — review/fix budget exhausted.
    pub fn review_exhausted(detail: &str) -> EngineError {
        EngineError::new("engine.review_exhausted", detail)
    }

    /// `engine.stale_resume` — the presented digest is not the fresh
    /// authorized resume digest.
    pub fn stale_resume() -> EngineError {
        EngineError::new(
            "engine.stale_resume",
            "only a fresh authorized resume digest lifts a pause",
        )
    }

    /// `engine.grant_stale` — the grant's issue/acceptance binding no
    /// longer matches the observed issue revision.
    pub fn grant_stale() -> EngineError {
        EngineError::new(
            "engine.grant_stale",
            "route grant issue/acceptance revision is stale; grant must be re-issued",
        )
    }
}

/// A parsed workflow shape: node id -> kind, plus edges and digests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowShape {
    /// Workflow id (slug).
    pub workflow_id: String,
    /// Node id -> closed kind.
    pub nodes: BTreeMap<String, String>,
    /// Directed edges (from, to), in document order.
    pub edges: Vec<(String, String)>,
    /// Canonical bytes of the document.
    pub canonical: Vec<u8>,
    /// SHA-256 over the canonical bytes (lowercase hex).
    pub digest: String,
}

impl WorkflowShape {
    /// Canonical digest (spec-workflow.md: sha256 over canonical bytes).
    pub fn digest_of(doc: &Val) -> String {
        sha256_hex(&canonical_bytes(doc))
    }

    /// Whether the node id exists.
    pub fn has_node(&self, id: &str) -> bool {
        self.nodes.contains_key(id)
    }

    /// Node kind for an id.
    pub fn kind_of(&self, id: &str) -> Option<&str> {
        self.nodes.get(id).map(String::as_str)
    }

    /// Reachable node ids from `from` (successors over edges).
    pub fn successors(&self, from: &str) -> Vec<&str> {
        self.edges
            .iter()
            .filter(|(f, _)| f == from)
            .map(|(_, to)| to.as_str())
            .collect()
    }

    /// Whether `to` is reachable from `from` following edges.
    pub fn reachable(&self, from: &str, to: &str) -> bool {
        let mut stack = vec![from.to_string()];
        let mut seen = std::collections::BTreeSet::new();
        while let Some(node) = stack.pop() {
            if node == to {
                return true;
            }
            if !seen.insert(node.clone()) {
                continue;
            }
            for successor in self.successors(&node) {
                stack.push(successor.to_string());
            }
        }
        false
    }
}

/// Parse and analyze an `hf-workflow/v1` document (must already validate for
/// [`crate::schema::Family::Workflow`]).
///
/// Engine-level fail-closed rules applied here:
/// - every edge endpoint must name a declared node (`engine.unknown_node`),
/// - the graph must be acyclic (`engine.cyclic`),
/// - the graph must be bounded: exactly one `start` node, every node
///   reachable from it, and every node able to reach a `terminal` node
///   (`engine.unbounded`).
pub fn analyze(doc: &Val) -> Result<WorkflowShape, EngineError> {
    let workflow_id = doc
        .get("workflow_id")
        .and_then(Val::as_str)
        .unwrap_or_default()
        .to_string();
    let mut nodes = BTreeMap::new();
    if let Some(list) = doc.get("nodes").and_then(Val::as_array) {
        for node in list {
            let id = node.get("id").and_then(Val::as_str).unwrap_or_default();
            let kind = node.get("kind").and_then(Val::as_str).unwrap_or_default();
            nodes.insert(id.to_string(), kind.to_string());
        }
    }
    let mut edges = Vec::new();
    if let Some(list) = doc.get("edges").and_then(Val::as_array) {
        for edge in list {
            let from = edge.get("from").and_then(Val::as_str).unwrap_or_default();
            let to = edge.get("to").and_then(Val::as_str).unwrap_or_default();
            if !nodes.contains_key(from) {
                return Err(EngineError::unknown_node(from));
            }
            if !nodes.contains_key(to) {
                return Err(EngineError::unknown_node(to));
            }
            edges.push((from.to_string(), to.to_string()));
        }
    }
    let shape = WorkflowShape {
        workflow_id,
        nodes,
        edges,
        canonical: canonical_bytes(doc),
        digest: WorkflowShape::digest_of(doc),
    };

    // Acyclicity: iterative DFS with an in-progress stack.
    let mut color: BTreeMap<String, u8> = shape.nodes.keys().map(|id| (id.clone(), 0)).collect();
    fn visit(
        node: &str,
        color: &mut BTreeMap<String, u8>,
        edges: &[(String, String)],
    ) -> Result<(), EngineError> {
        color.insert(node.to_string(), 1);
        for (from, to) in edges {
            if from != node {
                continue;
            }
            match color.get(to).copied().unwrap_or(0) {
                1 => return Err(EngineError::cyclic()),
                0 => visit(to, color, edges)?,
                _ => {}
            }
        }
        color.insert(node.to_string(), 2);
        Ok(())
    }
    let ids: Vec<String> = shape.nodes.keys().cloned().collect();
    for id in &ids {
        if color.get(id) == Some(&0) {
            visit(id, &mut color, &shape.edges)?;
        }
    }

    // Boundedness: exactly one start, one terminal reachable from every
    // node, every node reachable from the start.
    let starts: Vec<&String> = shape
        .nodes
        .iter()
        .filter(|(_, kind)| kind.as_str() == "start")
        .map(|(id, _)| id)
        .collect();
    if starts.len() != 1 {
        return Err(EngineError::unbounded());
    }
    let start = starts[0].clone();
    let terminals: Vec<&String> = shape
        .nodes
        .iter()
        .filter(|(_, kind)| kind.as_str() == "terminal")
        .map(|(id, _)| id)
        .collect();
    if terminals.is_empty() {
        return Err(EngineError::unbounded());
    }
    for id in shape.nodes.keys() {
        if !shape.reachable(&start, id) {
            return Err(EngineError::unbounded());
        }
        let has_terminal = terminals.iter().any(|t| shape.reachable(id, t));
        if !has_terminal {
            return Err(EngineError::unbounded());
        }
    }
    Ok(shape)
}

/// Privilege checks over node `params`:
/// - authority-bearing keys are refused (`engine.privilege`),
/// - a `role` reference must be a default role or a hash-pinned custom role
///   from the allowlist (`engine.closed_role`).
pub fn check_node_params(params: Option<&Val>, custom_roles: &[String]) -> Result<(), EngineError> {
    let Some(params) = params else {
        return Ok(());
    };
    let Val::Obj(map) = params else {
        return Ok(());
    };
    for (key, value) in map {
        if AUTHORITY_PARAM_KEYS.contains(&key.as_str()) {
            return Err(EngineError::privilege(format!("node param {key:?}")));
        }
        if key == "role" {
            let Some(role) = value.as_str() else {
                return Err(EngineError::privilege("node role must be a string"));
            };
            if !DEFAULT_ROLES.contains(&role) && !custom_roles.iter().any(|r| r == role) {
                return Err(EngineError::closed_role(role));
            }
        }
    }
    Ok(())
}

/// Privilege scan over every node in the document.
pub fn check_workflow_privileges(doc: &Val, custom_roles: &[String]) -> Result<(), EngineError> {
    if let Some(list) = doc.get("nodes").and_then(Val::as_array) {
        for node in list {
            check_node_params(node.get("params"), custom_roles)?;
        }
    }
    Ok(())
}

/// Pin validation: the configured pin (id + hash) must match the loaded
/// document (spec-workflow.md; AC1 hash-mismatch fail-closed).
pub fn check_pin(doc: &Val, pin_id: &str, pin_hash: &str) -> Result<(), EngineError> {
    let id = doc
        .get("workflow_id")
        .and_then(Val::as_str)
        .unwrap_or_default();
    if id != pin_id {
        return Err(EngineError::new(
            "engine.pin_id_mismatch",
            format!("configured pin selects workflow {pin_id:?} but document is {id:?}"),
        ));
    }
    let digest = WorkflowShape::digest_of(doc);
    if digest != pin_hash {
        return Err(EngineError::hash_mismatch());
    }
    Ok(())
}

/// Changed-in-flight check: an instance pinned `pinned_hash` when it
/// started; a transition may only use a document whose digest still equals
/// that pin (AC1/AC3: upgrades affect only new runs).
pub fn check_changed_in_flight(doc: &Val, pinned_hash: &str) -> Result<(), EngineError> {
    let digest = WorkflowShape::digest_of(doc);
    if digest != pinned_hash {
        return Err(EngineError::changed_in_flight());
    }
    Ok(())
}

/// Review/fix outcome kinds (AC4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewOutcome {
    /// Review passed; the instance may proceed.
    Passed,
    /// Review found issues; a fix round may run.
    FixRequested,
}

/// Where a review outcome routes the instance (AC4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoundVerdict {
    /// Proceed forward (passed, or recovery budget accepted a fix).
    Advance,
    /// Run another normal fix round automatically.
    Fix,
    /// Run the single separately authorized recovery round.
    Recovery,
    /// Budget exhausted; the item enters the human queue.
    HumanQueue,
}

/// Compute the round verdict for a review/fix outcome.
///
/// Rules (AC4): at most `NORMAL_REVIEW_ROUNDS` normal fix rounds run
/// automatically; then one separately authorized recovery round may follow
/// (`recovery_authorized`); exhaustion enters the human queue. `passed`
/// always advances.
pub fn review_round(
    outcome: ReviewOutcome,
    normal_used: u32,
    recovery_used: u32,
    recovery_authorized: bool,
) -> Result<RoundVerdict, EngineError> {
    match outcome {
        ReviewOutcome::Passed => Ok(RoundVerdict::Advance),
        ReviewOutcome::FixRequested => {
            if normal_used < NORMAL_REVIEW_ROUNDS {
                Ok(RoundVerdict::Fix)
            } else if recovery_used < RECOVERY_ROUNDS && recovery_authorized {
                Ok(RoundVerdict::Recovery)
            } else {
                Err(EngineError::review_exhausted(
                    "review/fix rounds exhausted; item enters the human queue",
                ))
            }
        }
    }
}

/// Advisory proposals from an orchestrator/LLM step. Carries evidence and
/// proposals only; it has no transition authority (AC9).
#[derive(Clone, Debug, PartialEq)]
pub struct Advisory {
    /// Optional evidence id (ev_ format).
    pub evidence_id: Option<String>,
    /// Proposal text (advisory only).
    pub proposals: Vec<String>,
    /// Raw advisory document.
    pub doc: Val,
}

/// Accept an orchestrator step's typed output. Any field that attempts to
/// carry policy, capabilities, target, or approval authority is refused
/// (AC9: LLM output / issue text / adapter metadata / repository files can
/// never change policy, capabilities, target, or approval state).
pub fn accept_advisory(doc: &Val) -> Result<Advisory, EngineError> {
    let mut proposals = Vec::new();
    if let Some(list) = doc.get("proposals").and_then(Val::as_array) {
        for item in list {
            if let Some(text) = item.as_str() {
                proposals.push(text.to_string());
            }
        }
    }
    let evidence_id = doc
        .get("evidence_id")
        .and_then(Val::as_str)
        .map(str::to_string);
    // Authority fields are refused even inside advisory steps: the daemon
    // never reads them as decisions.
    let map = match doc {
        Val::Obj(map) => map,
        _ => {
            return Ok(Advisory {
                evidence_id,
                proposals,
                doc: doc.clone(),
            });
        }
    };
    for (key, _) in map {
        if AUTHORITY_PARAM_KEYS.contains(&key.as_str())
            || key.starts_with("target")
            || key == "transition"
        {
            return Err(EngineError::privilege(format!(
                "advisory step field {key:?}"
            )));
        }
    }
    Ok(Advisory {
        evidence_id,
        proposals,
        doc: doc.clone(),
    })
}

/// Route-grant issue/acceptance binding check (AC2): a grant is valid only
/// while the observed issue revision equals the revision the grant bound.
/// A material issue/acceptance edit makes the grant stale; daemon refuses
/// further mutation until a fresh grant is issued.
pub fn grant_binding_valid(
    grant_issue_revision: &str,
    observed_revision: &str,
) -> Result<(), EngineError> {
    if grant_issue_revision != observed_revision {
        return Err(EngineError::grant_stale());
    }
    Ok(())
}

/// Pause/resume digests (AC8). A pause mints a fresh authorized resume
/// digest bound to the instance and the epoch at pause time; a resume is
/// refused unless the presented digest equals the stored fresh digest.
pub fn mint_resume_digest(instance_id: &str, epoch: i64, nonce: &str) -> String {
    let doc = object(vec![
        ("instance_id", string(instance_id)),
        ("epoch", integer(epoch)),
        ("nonce", string(nonce)),
    ]);
    sha256_hex(&canonical_bytes(&doc))
}

/// Authorize a resume: only the fresh stored digest lifts the pause
/// (`engine.stale_resume` otherwise).
pub fn authorize_resume(stored_digest: &str, presented: &str) -> Result<(), EngineError> {
    if stored_digest.is_empty() || stored_digest != presented {
        return Err(EngineError::stale_resume());
    }
    Ok(())
}

/// Terminal-blocker accounting (AC7): blocked items never stop disjoint
/// granted work; terminal blockers stay explicit and counted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockState {
    /// Whether the item is terminally blocked.
    pub blocked: bool,
    /// Number of explicit terminal blockers.
    pub terminal_blockers: u32,
}

impl BlockState {
    /// A new unblocked item.
    pub fn running() -> BlockState {
        BlockState {
            blocked: false,
            terminal_blockers: 0,
        }
    }

    /// Add a terminal blocker (marks the item blocked).
    pub fn add_blocker(self) -> BlockState {
        BlockState {
            blocked: true,
            terminal_blockers: self.terminal_blockers + 1,
        }
    }

    /// Whether a disjoint granted item may keep running (always: blocked is
    /// per-item; a blocked item never stops another instance — AC7).
    pub fn disjoint_work_continues(&self) -> bool {
        true
    }
}

/// One authoritative workflow per repository (AC7/AC10): cross-repository
/// initiatives use linked workflows, never an atomic claim; this engine
/// binds one workflow id per repository scope. Refused when a second
/// workflow id is asserted for the same repository scope.
pub fn check_single_authority(
    existing_workflow_id: Option<&str>,
    proposed_workflow_id: &str,
    scope: &str,
) -> Result<(), EngineError> {
    match existing_workflow_id {
        Some(existing) if existing != proposed_workflow_id => Err(EngineError::new(
            "engine.workflow_conflict",
            format!(
                "scope {scope:?} already runs workflow {existing:?}; one authoritative \
                 workflow per repository (linked workflows, not atomic claims)"
            ),
        )),
        _ => Ok(()),
    }
}

/// Review-gate rules (AC5): an integration merge requires a distinct
/// reviewer — a `reviewer`-kind node reachable before the merge — and a
/// node may never conflate reviewer and implementer identity (a reviewer
/// node declaring role `implementer`, or an implementer declaring role
/// `reviewer`, is refused). Adapter-level session/worktree isolation and
/// optional extra harness/provider/model policy belong to the evidence
/// contract (spec-review-evidence.md; child #8).
pub fn check_review_gate(doc: &Val, shape: &WorkflowShape) -> Result<(), EngineError> {
    for (id, kind) in &shape.nodes {
        if kind != "merge" {
            continue;
        }
        let reviewed = shape
            .nodes
            .keys()
            .any(|rid| shape.kind_of(rid) == Some("reviewer") && shape.reachable(rid, id));
        if !reviewed {
            return Err(EngineError::new(
                "engine.unreviewed_merge",
                format!(
                    "merge node {id:?} has no distinct reviewer reachable before it; \
                     integration merge requires a distinct exact-head reviewer"
                ),
            ));
        }
    }
    if let Some(list) = doc.get("nodes").and_then(Val::as_array) {
        for node in list {
            let kind = node.get("kind").and_then(Val::as_str).unwrap_or_default();
            let role = node
                .get("params")
                .and_then(|params| params.get("role"))
                .and_then(Val::as_str)
                .unwrap_or_default();
            if (kind == "reviewer" && role == "implementer")
                || (kind == "implementer" && role == "reviewer")
            {
                return Err(EngineError::new(
                    "engine.role_collision",
                    format!(
                        "reviewer and implementer must be distinct identities; \
                         node {id:?} conflates role {role:?} with kind {kind:?}",
                        id = node.get("id").and_then(Val::as_str).unwrap_or("(unnamed)"),
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// Follow-up routing rule (AC6): new findings may produce linked
/// proposed/filed follow-ups, but they stay unrouted without a new grant.
/// A follow-up for an issue outside the grant's issue set is refused.
pub fn followup_routed(grant_issue: i64, followup_issue: i64) -> Result<(), EngineError> {
    if followup_issue != grant_issue {
        return Err(EngineError::new(
            "engine.unrouted",
            format!(
                "follow-up issue #{followup_issue} is not covered by the grant (issue #{grant_issue}); \
                 a new grant is required to route it"
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Family, validate_doc};

    fn doc_from(json: &str) -> Val {
        Val::parse_json(json).expect("parse test doc")
    }

    fn valid_spine() -> Val {
        doc_from(
            r#"{"schema":"hf-workflow/v1","workflow_id":"fleet-doctrine-1",
                "nodes":[
                  {"id":"start","kind":"start","params":null},
                  {"id":"impl","kind":"implementer","params":null},
                  {"id":"rev","kind":"reviewer","params":null},
                  {"id":"merge","kind":"merge","params":null},
                  {"id":"term","kind":"terminal","params":null}],
                "edges":[
                  {"from":"start","to":"impl"},
                  {"from":"impl","to":"rev"},
                  {"from":"rev","to":"merge"},
                  {"from":"merge","to":"term"}]}"#,
        )
    }

    #[test]
    fn doctrine_default_is_valid_canonical_and_pinned() {
        let shape = doctrine_default().expect("doctrine default");
        assert_eq!(shape.workflow_id, DOCTRINE_WORKFLOW_ID);
        assert_eq!(shape.digest, DOCTRINE_DIGEST);
        // Canonical round-trip: parse canonical bytes and re-canonicalize.
        let reparsed =
            Val::parse_json(std::str::from_utf8(&shape.canonical).expect("utf8")).expect("parse");
        assert_eq!(canonical_bytes(&reparsed), shape.canonical);
        // The doctrine models the full lifecycle spine.
        assert!(shape.has_node("isolated-implementation"));
        assert!(shape.has_node("exact-head-review"));
        assert!(shape.has_node("hosted-ci"));
        assert!(shape.has_node("integration-merge"));
        assert!(shape.has_node("post-merge-verification"));
        assert!(shape.has_node("closure"));
        // Order: start -> ... -> closure (one authoritative terminal path).
        assert!(shape.reachable("start", "closure"));
        assert_eq!(shape.kind_of("closure"), Some("terminal"));
        // No production/destructive authority is embedded anywhere.
        let doc = Val::parse_json(DOCTRINE_DOCUMENT).expect("doctrine parse");
        check_workflow_privileges(&doc, &[]).expect("no authority fields");
        // The fixture file validates as hf-workflow/v1 (manifest row).
        let bytes = DOCTRINE_DOCUMENT.as_bytes();
        let verdict = crate::schema::validate_bytes(Family::Workflow, bytes);
        assert!(verdict.is_accepted(), "{}", verdict.message());
        // Pinned digest matches the on-disk canonical bytes exactly.
        assert_eq!(sha256_hex(bytes), DOCTRINE_DIGEST);
    }

    #[test]
    fn valid_spine_analyzes_and_digests() {
        let doc = valid_spine();
        assert!(validate_doc(Family::Workflow, &doc).is_accepted());
        let shape = analyze(&doc).expect("analyze");
        assert_eq!(shape.workflow_id, "fleet-doctrine-1");
        assert_eq!(shape.digest.len(), 64);
        assert_eq!(shape.digest, WorkflowShape::digest_of(&doc));
        assert!(shape.has_node("rev"));
        assert!(shape.reachable("start", "term"));
    }

    #[test]
    fn cyclic_dag_fails_closed() {
        let doc = doc_from(
            r#"{"schema":"hf-workflow/v1","workflow_id":"fleet-doctrine-1",
                "nodes":[
                  {"id":"start","kind":"start","params":null},
                  {"id":"a","kind":"plan","params":null},
                  {"id":"b","kind":"gate","params":null},
                  {"id":"term","kind":"terminal","params":null}],
                "edges":[
                  {"from":"start","to":"a"},
                  {"from":"a","to":"b"},
                  {"from":"b","to":"a"},
                  {"from":"b","to":"term"}]}"#,
        );
        let err = analyze(&doc).expect_err("cycle must fail closed");
        assert_eq!(err.code, "engine.cyclic");
    }

    #[test]
    fn unbounded_dag_fails_closed() {
        // Two start nodes -> ambiguous entry.
        let doc = doc_from(
            r#"{"schema":"hf-workflow/v1","workflow_id":"fleet-doctrine-1",
                "nodes":[
                  {"id":"start","kind":"start","params":null},
                  {"id":"start2","kind":"start","params":null},
                  {"id":"term","kind":"terminal","params":null}],
                "edges":[
                  {"from":"start","to":"term"},
                  {"from":"start2","to":"term"}]}"#,
        );
        assert_eq!(
            analyze(&doc).expect_err("unbounded").code,
            "engine.unbounded"
        );

        // A node that cannot reach a terminal.
        let doc = doc_from(
            r#"{"schema":"hf-workflow/v1","workflow_id":"fleet-doctrine-1",
                "nodes":[
                  {"id":"start","kind":"start","params":null},
                  {"id":"dead","kind":"gate","params":null},
                  {"id":"term","kind":"terminal","params":null}],
                "edges":[
                  {"from":"start","to":"dead"},
                  {"from":"start","to":"term"}]}"#,
        );
        assert_eq!(
            analyze(&doc).expect_err("dead end").code,
            "engine.unbounded"
        );
    }

    #[test]
    fn unknown_node_edge_fails_closed() {
        let doc = doc_from(
            r#"{"schema":"hf-workflow/v1","workflow_id":"fleet-doctrine-1",
                "nodes":[
                  {"id":"start","kind":"start","params":null},
                  {"id":"term","kind":"terminal","params":null}],
                "edges":[{"from":"start","to":"ghost"}]}"#,
        );
        let err = analyze(&doc).expect_err("unknown node");
        assert_eq!(err.code, "engine.unknown_node");
    }

    #[test]
    fn privilege_escalation_fails_closed() {
        // A node attempting to declare capabilities/risk in params.
        let doc = doc_from(
            r#"{"schema":"hf-workflow/v1","workflow_id":"fleet-doctrine-1",
                "nodes":[
                  {"id":"start","kind":"start","params":null},
                  {"id":"impl","kind":"implementer","params":{"caps":["merge"],"risk":"read"}},
                  {"id":"term","kind":"terminal","params":null}],
                "edges":[{"from":"start","to":"impl"},{"from":"impl","to":"term"}]}"#,
        );
        let err = check_node_params(
            doc.get("nodes").and_then(Val::as_array).unwrap()[1].get("params"),
            &[],
        )
        .expect_err("privilege");
        assert_eq!(err.code, "engine.privilege");
    }

    #[test]
    fn closed_role_refused_and_pinned_custom_role_accepted() {
        let params = doc_from(r#"{"role":"ghost-oracle"}"#);
        let err = check_node_params(Some(&params), &[]).expect_err("closed role");
        assert_eq!(err.code, "engine.closed_role");
        let custom = vec!["ghost-oracle".to_string()];
        check_node_params(Some(&params), &custom).expect("pinned custom role ok");
        // Default roles are always allowed.
        let params = doc_from(r#"{"role":"reviewer"}"#);
        check_node_params(Some(&params), &[]).expect("default role ok");
    }

    #[test]
    fn pin_hash_mismatch_and_changed_in_flight_fail_closed() {
        let doc = valid_spine();
        let digest = WorkflowShape::digest_of(&doc);
        check_pin(&doc, "fleet-doctrine-1", &digest).expect("pin ok");
        let wrong = "f".repeat(64);
        let err = check_pin(&doc, "fleet-doctrine-1", &wrong).expect_err("hash mismatch");
        assert_eq!(err.code, "engine.hash_mismatch");
        check_changed_in_flight(&doc, &digest).expect("same doc ok");
        let err = check_changed_in_flight(&doc, &wrong).expect_err("changed in flight");
        assert_eq!(err.code, "engine.changed_in_flight");
    }

    #[test]
    fn review_round_policy_is_bounded_and_exhaustion_enters_human_queue() {
        // Passed always advances.
        assert_eq!(
            review_round(ReviewOutcome::Passed, 0, 0, false).expect("passed"),
            RoundVerdict::Advance
        );
        // Three normal fix rounds run automatically.
        for used in 0..NORMAL_REVIEW_ROUNDS {
            assert_eq!(
                review_round(ReviewOutcome::FixRequested, used, 0, false).expect("normal fix"),
                RoundVerdict::Fix
            );
        }
        // Without separate authorization, the fourth fix request is
        // exhausted -> human queue.
        let err = review_round(ReviewOutcome::FixRequested, NORMAL_REVIEW_ROUNDS, 0, false)
            .expect_err("exhausted");
        assert_eq!(err.code, "engine.review_exhausted");
        // With separate authorization, one recovery round may follow.
        assert_eq!(
            review_round(ReviewOutcome::FixRequested, NORMAL_REVIEW_ROUNDS, 0, true)
                .expect("recovery"),
            RoundVerdict::Recovery
        );
        // Recovery used -> exhaustion even when authorized.
        let err = review_round(ReviewOutcome::FixRequested, NORMAL_REVIEW_ROUNDS, 1, true)
            .expect_err("recovery exhausted");
        assert_eq!(err.code, "engine.review_exhausted");
    }

    #[test]
    fn advisory_steps_cannot_carry_transition_authority() {
        let advisory =
            doc_from(r#"{"proposals":["open a follow-up"],"evidence_id":"ev_0123456789abcdef"}"#);
        let accepted = accept_advisory(&advisory).expect("advisory accepted");
        assert_eq!(accepted.proposals.len(), 1);
        assert_eq!(accepted.evidence_id.as_deref(), Some("ev_0123456789abcdef"));

        let escalated = doc_from(r#"{"proposals":[],"transition":"merge","caps":["release"]}"#);
        let err = accept_advisory(&escalated).expect_err("escalation refused");
        assert_eq!(err.code, "engine.privilege");
    }

    #[test]
    fn grant_binding_revision_is_enforced() {
        grant_binding_valid("a".repeat(40).as_str(), "a".repeat(40).as_str()).expect("same");
        let err = grant_binding_valid("a".repeat(40).as_str(), "b".repeat(40).as_str())
            .expect_err("stale");
        assert_eq!(err.code, "engine.grant_stale");
    }

    #[test]
    fn resume_digests_are_fresh_authorized_and_single_use() {
        let digest = mint_resume_digest("inst_1", 3, "pause-1");
        authorize_resume(&digest, &digest).expect("fresh digest ok");
        let err = authorize_resume(&digest, &"0".repeat(64)).expect_err("wrong digest");
        assert_eq!(err.code, "engine.stale_resume");
        // A different pause mints a different digest; the old one is stale.
        let newer = mint_resume_digest("inst_1", 3, "pause-2");
        assert_ne!(digest, newer);
        let err = authorize_resume(&newer, &digest).expect_err("old digest stale");
        assert_eq!(err.code, "engine.stale_resume");
    }

    #[test]
    fn blockers_are_per_item_and_disjoint_work_continues() {
        let blocked = BlockState::running().add_blocker();
        assert!(blocked.blocked);
        assert_eq!(blocked.terminal_blockers, 1);
        assert!(blocked.disjoint_work_continues());
        let other = BlockState::running();
        assert!(!other.blocked);
    }

    #[test]
    fn single_authority_per_repository_scope_is_enforced() {
        check_single_authority(Some("fleet-doctrine-1"), "fleet-doctrine-1", "a/b").expect("same");
        let err = check_single_authority(Some("fleet-doctrine-1"), "other-workflow", "a/b")
            .expect_err("conflict");
        assert_eq!(err.code, "engine.workflow_conflict");
    }

    #[test]
    fn merge_requires_a_distinct_reviewer_before_it() {
        // A merge with no reviewer anywhere fails closed.
        let doc = doc_from(
            r#"{"schema":"hf-workflow/v1","workflow_id":"fleet-doctrine-1",
                "nodes":[
                  {"id":"start","kind":"start","params":null},
                  {"id":"merge","kind":"merge","params":null},
                  {"id":"term","kind":"terminal","params":null}],
                "edges":[
                  {"from":"start","to":"merge"},
                  {"from":"merge","to":"term"}]}"#,
        );
        let shape = analyze(&doc).expect("analyze");
        let err = check_review_gate(&doc, &shape).expect_err("unreviewed merge");
        assert_eq!(err.code, "engine.unreviewed_merge");

        // A reviewer *after* the merge cannot gate it (must be reachable
        // before the merge). Parallel reviewers that cannot reach the merge
        // also do not gate it.
        let doc = doc_from(
            r#"{"schema":"hf-workflow/v1","workflow_id":"fleet-doctrine-1",
                "nodes":[
                  {"id":"start","kind":"start","params":null},
                  {"id":"merge","kind":"merge","params":null},
                  {"id":"rev","kind":"reviewer","params":null},
                  {"id":"term","kind":"terminal","params":null}],
                "edges":[
                  {"from":"start","to":"merge"},
                  {"from":"merge","to":"term"},
                  {"from":"start","to":"rev"},
                  {"from":"rev","to":"term"}]}"#,
        );
        let shape = analyze(&doc).expect("analyze");
        assert_eq!(
            check_review_gate(&doc, &shape)
                .expect_err("late reviewer")
                .code,
            "engine.unreviewed_merge"
        );

        // The doctrine spine gates its merge with exact-head-review.
        let shape = doctrine_default().expect("doctrine");
        let doc = Val::parse_json(DOCTRINE_DOCUMENT).expect("doctrine parse");
        check_review_gate(&doc, &shape).expect("doctrine merge is gated");
    }

    #[test]
    fn reviewer_and_implementer_identities_are_distinct() {
        // A reviewer node claiming the implementer role is refused.
        let doc = doc_from(
            r#"{"schema":"hf-workflow/v1","workflow_id":"fleet-doctrine-1",
                "nodes":[
                  {"id":"start","kind":"start","params":null},
                  {"id":"rev","kind":"reviewer","params":{"role":"implementer"}},
                  {"id":"merge","kind":"merge","params":null},
                  {"id":"term","kind":"terminal","params":null}],
                "edges":[
                  {"from":"start","to":"rev"},
                  {"from":"rev","to":"merge"},
                  {"from":"merge","to":"term"}]}"#,
        );
        let shape = analyze(&doc).expect("analyze");
        let err = check_review_gate(&doc, &shape).expect_err("role collision");
        assert_eq!(err.code, "engine.role_collision");

        // An implementer node claiming the reviewer role is refused too
        // (the gating reviewer must remain a distinct identity).
        let doc = doc_from(
            r#"{"schema":"hf-workflow/v1","workflow_id":"fleet-doctrine-1",
                "nodes":[
                  {"id":"start","kind":"start","params":null},
                  {"id":"impl","kind":"implementer","params":{"role":"reviewer"}},
                  {"id":"rev","kind":"reviewer","params":null},
                  {"id":"merge","kind":"merge","params":null},
                  {"id":"term","kind":"terminal","params":null}],
                "edges":[
                  {"from":"start","to":"impl"},
                  {"from":"impl","to":"rev"},
                  {"from":"rev","to":"merge"},
                  {"from":"merge","to":"term"}]}"#,
        );
        let shape = analyze(&doc).expect("analyze");
        let err = check_review_gate(&doc, &shape).expect_err("role collision");
        assert_eq!(err.code, "engine.role_collision");
    }

    #[test]
    fn followups_are_unrouted_without_a_new_grant() {
        followup_routed(12, 12).expect("same issue ok");
        let err = followup_routed(12, 13).expect_err("new issue needs a grant");
        assert_eq!(err.code, "engine.unrouted");
    }
}
