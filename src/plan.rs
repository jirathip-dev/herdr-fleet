//! Deterministic read-only plan rendering (`hf-plan/v1`).
//!
//! A plan is the complete, typed description of intended effects for one
//! issue-bound unit of work (docs/contracts/spec-plans.md). This read-only
//! slice renders plans without any daemon: it never applies, journals, or
//! grants anything. Rendering is fully deterministic — the same inputs
//! always produce the same canonical bytes and digest — so plans are
//! reproducible evidence.
//!
//! Determinism rules (documented, stable surface):
//! - `plan_id` is content-addressed: the first 16 hex of the SHA-256 over
//!   the canonical document with a placeholder id, formatted as
//!   `hf_plan_<16hex>`.
//! - `state_epoch` is 0 (no daemon state exists in this slice).
//! - The step spine is the built-in `fleet-doctrine-1` doctrine workflow
//!   (the same eight steps the #3 `plan.valid.json` fixture shows). A
//!   configured workflow pin with a different id is refused (the workflow
//!   engine is a later slice); when no pin is configured the `workflow_hash`
//!   is derived from the spine definition itself.

use crate::canonical::{canonical_bytes, sha256_hex};
use crate::config::{Repository, WorkflowPin};
use crate::value::{Val, integer, null, object, string};

/// The built-in doctrine workflow id known to this read-only slice.
pub const DOCTRINE_WORKFLOW_ID: &str = "fleet-doctrine-1";
/// Placeholder id used for the content-addressed plan id derivation.
const PLAN_ID_PLACEHOLDER: &str = "hf_plan_0000000000000000";

/// One rendered plan step (id, closed-set kind, optional params object).
#[derive(Clone, Debug, PartialEq)]
pub struct PlanStep {
    /// Step id (slug, e.g. `p1`).
    pub id: String,
    /// Closed-set step kind.
    pub kind: String,
    /// Typed params or none.
    pub params: Option<Val>,
}

/// Why rendering refused a plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanRefusal {
    /// A configured workflow pin names a workflow this slice cannot render.
    UnsupportedWorkflow(String),
}

/// Inputs for one deterministic plan render.
#[derive(Clone)]
pub struct PlanInput<'a> {
    /// Configured repository (identity comes from config, never free-form).
    pub repository: &'a Repository,
    /// Issue number (positive integer from validated argv).
    pub issue_number: u64,
    /// Exact acceptance revision (40-hex).
    pub revision: String,
    /// Configured workflow pin, if any.
    pub workflow: Option<&'a WorkflowPin>,
}

/// One deterministic rendered plan.
#[derive(Clone, Debug)]
pub struct RenderedPlan {
    /// The `hf-plan/v1` document value.
    pub doc: Val,
    /// Canonical JSON bytes of the document.
    pub canonical: Vec<u8>,
    /// SHA-256 over the canonical bytes (lowercase hex).
    pub digest: String,
    /// Content-addressed plan id.
    pub plan_id: String,
    /// Workflow id bound by the plan.
    pub workflow_id: String,
    /// 64-hex workflow hash bound by the plan.
    pub workflow_hash: String,
}

/// Render the doctrine step spine for one issue.
fn doctrine_steps(repository: &Repository, issue_number: u64) -> Vec<PlanStep> {
    let branch = repository
        .branch
        .clone()
        .unwrap_or_else(|| "staging".to_string());
    vec![
        PlanStep {
            id: "p1".to_string(),
            kind: "checkout".to_string(),
            params: Some(object(vec![("ref", string(&branch))])),
        },
        PlanStep {
            id: "p2".to_string(),
            kind: "worktree_create".to_string(),
            params: Some(object(vec![(
                "scope",
                string(&format!("issues/{issue_number}")),
            )])),
        },
        PlanStep {
            id: "p3".to_string(),
            kind: "harness_start".to_string(),
            params: None,
        },
        PlanStep {
            id: "p4".to_string(),
            kind: "prompt".to_string(),
            params: None,
        },
        PlanStep {
            id: "p5".to_string(),
            kind: "collect_outcome".to_string(),
            params: None,
        },
        PlanStep {
            id: "p6".to_string(),
            kind: "review_evidence".to_string(),
            params: None,
        },
        PlanStep {
            id: "p7".to_string(),
            kind: "merge".to_string(),
            params: None,
        },
        PlanStep {
            id: "p8".to_string(),
            kind: "cleanup".to_string(),
            params: None,
        },
    ]
}

/// The workflow hash for the built-in doctrine workflow: derived from the
/// canonical serialization of the spine definition (id + step ids/kinds)
/// when no config pin supplies one.
fn doctrine_workflow_hash(steps: &[PlanStep]) -> String {
    let definition = object(vec![
        ("workflow_id", string(DOCTRINE_WORKFLOW_ID)),
        (
            "steps",
            Val::Arr(
                steps
                    .iter()
                    .map(|step| {
                        object(vec![("id", string(&step.id)), ("kind", string(&step.kind))])
                    })
                    .collect(),
            ),
        ),
    ]);
    sha256_hex(&canonical_bytes(&definition))
}

fn steps_value(steps: &[PlanStep]) -> Val {
    Val::Arr(
        steps
            .iter()
            .map(|step| {
                object(vec![
                    ("id", string(&step.id)),
                    ("kind", string(&step.kind)),
                    ("params", step.params.clone().unwrap_or_else(null)),
                ])
            })
            .collect(),
    )
}

/// Build the plan document with a concrete plan id.
fn build_doc(
    repository: &Repository,
    issue_number: u64,
    revision: &str,
    workflow_id: &str,
    workflow_hash: &str,
    plan_id: &str,
    steps: &[PlanStep],
) -> Val {
    object(vec![
        ("schema", string("hf-plan/v1")),
        ("plan_id", string(plan_id)),
        ("workflow_id", string(workflow_id)),
        ("workflow_hash", string(workflow_hash)),
        ("state_epoch", integer(0)),
        ("repository", string(&repository.identity())),
        (
            "issue",
            object(vec![
                ("number", integer(issue_number as i64)),
                ("revision", string(revision)),
            ]),
        ),
        ("steps", steps_value(steps)),
    ])
}

/// Render a deterministic plan. Refuses when a configured workflow pin names
/// a workflow the built-in doctrine spine cannot represent.
pub fn render_plan(input: &PlanInput<'_>) -> Result<RenderedPlan, PlanRefusal> {
    let workflow_id = DOCTRINE_WORKFLOW_ID.to_string();
    let steps = doctrine_steps(input.repository, input.issue_number);
    let workflow_hash = match input.workflow {
        None => doctrine_workflow_hash(&steps),
        Some(pin) if pin.id == DOCTRINE_WORKFLOW_ID => pin.hash.clone(),
        Some(pin) => return Err(PlanRefusal::UnsupportedWorkflow(pin.id.clone())),
    };

    let seeded = build_doc(
        input.repository,
        input.issue_number,
        &input.revision,
        &workflow_id,
        &workflow_hash,
        PLAN_ID_PLACEHOLDER,
        &steps,
    );
    let seed_digest = sha256_hex(&canonical_bytes(&seeded));
    let plan_id = format!("hf_plan_{}", &seed_digest[..16]);

    let doc = build_doc(
        input.repository,
        input.issue_number,
        &input.revision,
        &workflow_id,
        &workflow_hash,
        &plan_id,
        &steps,
    );
    let canonical = canonical_bytes(&doc);
    let digest = sha256_hex(&canonical);
    Ok(RenderedPlan {
        doc,
        canonical,
        digest,
        plan_id,
        workflow_id,
        workflow_hash,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Family, Verdict, validate_doc};

    fn repository() -> Repository {
        Repository {
            key: "widgets".to_string(),
            owner: "example-org".to_string(),
            name: "widgets".to_string(),
            origin: "https://github.com/example-org/widgets".to_string(),
            branch: Some("staging".to_string()),
            enabled: true,
        }
    }

    fn pin(id: &str) -> WorkflowPin {
        WorkflowPin {
            key: "bundle".to_string(),
            id: id.to_string(),
            hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string(),
        }
    }

    const REVISION: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn renders_a_valid_deterministic_plan() {
        let input = PlanInput {
            repository: &repository(),
            issue_number: 123,
            revision: REVISION.to_string(),
            workflow: None,
        };
        let plan = render_plan(&input).expect("render");
        let verdict = validate_doc(Family::Plan, &plan.doc);
        assert!(verdict.is_accepted(), "{}", verdict.message());
        assert_eq!(plan.workflow_id, "fleet-doctrine-1");
        assert!(plan.plan_id.starts_with("hf_plan_"));
        assert_eq!(plan.plan_id.len(), "hf_plan_".len() + 16);
        assert_eq!(plan.digest.len(), 64);
        assert_eq!(plan.digest, sha256_hex(&plan.canonical));
        // Canonical round-trip: parsing the canonical bytes must reproduce
        // the document byte-for-byte when re-canonicalized.
        let reparsed =
            Val::parse_json(std::str::from_utf8(&plan.canonical).expect("utf8")).expect("parse");
        assert_eq!(canonical_bytes(&reparsed), plan.canonical);
    }

    #[test]
    fn rendering_is_repeatable_and_sensitive_to_every_binding() {
        let base = PlanInput {
            repository: &repository(),
            issue_number: 123,
            revision: REVISION.to_string(),
            workflow: None,
        };
        let first = render_plan(&base).expect("render");
        let again = render_plan(&base).expect("render");
        assert_eq!(first.digest, again.digest, "same inputs, same digest");
        assert_eq!(first.canonical, again.canonical, "same inputs, same bytes");

        let different_issue = PlanInput {
            issue_number: 124,
            ..base.clone()
        };
        assert_ne!(
            first.digest,
            render_plan(&different_issue).expect("render").digest
        );

        let different_revision = PlanInput {
            revision: format!("{}1", &REVISION[..39]),
            ..base.clone()
        };
        assert_ne!(
            first.digest,
            render_plan(&different_revision).expect("render").digest
        );

        let mut other_repo = repository();
        other_repo.name = "gadgets".to_string();
        let different_repo = PlanInput {
            repository: &other_repo,
            ..base.clone()
        };
        assert_ne!(
            first.digest,
            render_plan(&different_repo).expect("render").digest
        );

        let pinned = PlanInput {
            workflow: Some(&pin("fleet-doctrine-1")),
            ..base.clone()
        };
        let pinned_plan = render_plan(&pinned).expect("render");
        assert_ne!(
            first.digest, pinned_plan.digest,
            "pin changes the bound hash"
        );
        assert_eq!(
            pinned_plan.workflow_hash,
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        );
    }

    #[test]
    fn unsupported_pinned_workflow_is_refused() {
        let input = PlanInput {
            repository: &repository(),
            issue_number: 123,
            revision: REVISION.to_string(),
            workflow: Some(&pin("some-other-engine")),
        };
        let refusal = render_plan(&input).expect_err("must refuse");
        assert_eq!(
            refusal,
            PlanRefusal::UnsupportedWorkflow("some-other-engine".to_string())
        );
    }

    #[test]
    fn configured_branch_flows_into_checkout_params() {
        let mut repo = repository();
        repo.branch = Some("integration".to_string());
        let input = PlanInput {
            repository: &repo,
            issue_number: 7,
            revision: REVISION.to_string(),
            workflow: None,
        };
        let plan = render_plan(&input).expect("render");
        let steps = plan.doc.get("steps").expect("steps");
        let Val::Arr(step_list) = steps else {
            panic!("steps array")
        };
        let checkout = &step_list[0];
        assert_eq!(checkout.get("kind").and_then(Val::as_str), Some("checkout"));
        let params = checkout.get("params").expect("params");
        assert_eq!(params.get("ref").and_then(Val::as_str), Some("integration"));
    }

    #[test]
    fn plan_validates_with_every_family_rule() {
        let input = PlanInput {
            repository: &repository(),
            issue_number: 123,
            revision: REVISION.to_string(),
            workflow: None,
        };
        let plan = render_plan(&input).expect("render");
        let verdict: Verdict = validate_doc(Family::Plan, &plan.doc);
        assert!(verdict.is_accepted());
    }
}
