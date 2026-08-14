//! The default workflow graphs, shipped verbatim and installable via
//! `supervisor dag apply` (§4.11). `feature_lifecycle` is the authoritative
//! rendering from the detailed design.

use crate::dag::Workflow;

/// The default `feature → prod` lifecycle graph (authoritative, from §4.11).
pub const FEATURE_LIFECYCLE_JSON: &str = r#"{
  "id": "feature_lifecycle",
  "name": "Default feature → prod",
  "nodes": [
    { "id": "brainstorm", "role": "designer", "timeout_secs": 3600, "depends_on": [],
      "start_template": "Research and brainstorm {feature} per {spec}. Finish by returning the ACK JSON contract with task_id=brainstorm.",
      "done_when": { "ack": "brainstorm" }, "on_error": "delegate" },
    { "id": "high_level_design", "role": "designer", "timeout_secs": 3600, "depends_on": ["brainstorm"],
      "start_template": "Write the high-level design for {feature}. Finish by returning the ACK JSON contract with task_id=high_level_design.",
      "done_when": { "ack": "high_level_design" }, "on_error": "delegate" },
    { "id": "hl_agent_review", "role": "reviewer", "timeout_secs": 3600, "depends_on": ["high_level_design"],
      "start_template": "Review the high-level design: verify every claim and completeness; ask 'is there a better solution?' per option. Finish with the ACK JSON contract, task_id=hl_agent_review.",
      "done_when": { "ack": "hl_agent_review" }, "on_error": "delegate" },
    { "id": "hl_human_gate", "role": "designer", "depends_on": ["hl_agent_review"],
      "gate": "plannotator",
      "start_template": "Submit the high-level design via submit_plan for human review. When the human approves or sends feedback, finish with the ACK JSON contract, task_id=hl_human_gate, status reflecting approval; include feedback in summary; set needs_revision=none|small|big per the feedback.",
      "done_when": { "ack": "hl_human_gate", "approved": true }, "on_error": "delegate",
      "loop_back": { "on": "needs_revision",
                     "small": "hl_human_gate",
                     "big": "high_level_design" } },
    { "id": "detailed_design", "role": "designer", "timeout_secs": 3600, "depends_on": ["hl_human_gate"],
      "start_template": "Write the detailed design for {feature} from the approved high-level design. Finish with the ACK JSON contract, task_id=detailed_design.",
      "done_when": { "ack": "detailed_design" }, "on_error": "delegate" },
    { "id": "dd_agent_review", "role": "reviewer", "timeout_secs": 3600, "depends_on": ["detailed_design"],
      "start_template": "Review the detailed design in detail; request human input only if a real decision is needed. Finish with the ACK JSON contract, task_id=dd_agent_review.",
      "done_when": { "ack": "dd_agent_review" }, "on_error": "delegate" },
    { "id": "dev", "role": "dev", "timeout_secs": 3600, "depends_on": ["dd_agent_review"],
      "start_template": "Implement {feature} from the approved detailed design; go through code review cycles, meet standards, unit+integration tests. Finish with the ACK JSON contract, task_id=dev.",
      "done_when": { "ack": "dev" }, "on_error": { "rerun": { "max": 2 } } },
    { "id": "tester_prep", "role": "tester", "timeout_secs": 3600, "depends_on": ["dd_agent_review"],
      "start_template": "Prepare UI automation setup and scripts for {feature} in parallel with dev. Finish with the ACK JSON contract, task_id=tester_prep.",
      "done_when": { "ack": "tester_prep" }, "on_error": "delegate" },
    { "id": "ui_e2e", "role": "tester", "timeout_secs": 3600, "depends_on": ["dev", "tester_prep"],
      "start_template": "Run end-to-end UI tests on {feature} (web + mobile, human-like). Finish with the ACK JSON contract, task_id=ui_e2e.",
      "done_when": { "ack": "ui_e2e" }, "on_error": "delegate" },
    { "id": "docs", "role": "memory-keeper", "timeout_secs": 3600, "depends_on": ["dev", "ui_e2e"],
      "start_template": "Update docs for {feature}. Finish with the ACK JSON contract, task_id=docs.",
      "done_when": { "ack": "docs" }, "on_error": "delegate" },
    { "id": "deploy_dev", "role": "dev", "timeout_secs": 3600, "depends_on": ["ui_e2e", "docs"], "mode": "background",
      "start_template": "Deploy {feature} to the dev env per this project's deploy rules. Finish with the ACK JSON contract, task_id=deploy_dev.",
      "done_when": { "ack": "deploy_dev" }, "on_error": "delegate" },
    { "id": "verify_dev", "role": "tester", "timeout_secs": 3600, "depends_on": ["deploy_dev"],
      "start_template": "Verify {feature} in the dev env. Finish with the ACK JSON contract, task_id=verify_dev.",
      "done_when": { "ack": "verify_dev" }, "on_error": "delegate" },
    { "id": "promote_prod", "role": "dev", "timeout_secs": 3600, "depends_on": ["verify_dev"], "mode": "background",
      "start_template": "Promote {feature} to prod per this project's deploy rules. Finish with the ACK JSON contract, task_id=promote_prod.",
      "done_when": { "ack": "promote_prod" }, "on_error": "delegate" }
  ]
}"#;

/// The bug flow: intake → reproduce → fix → verify.
pub const BUG_FLOW_JSON: &str = r#"{
  "id": "bug_flow",
  "name": "Default bug → fix → verify",
  "nodes": [
    { "id": "reproduce", "role": "tester", "timeout_secs": 3600, "depends_on": [],
      "start_template": "Reproduce bug {bug} from the report; record exact steps and the failing behavior. Finish with the ACK JSON contract, task_id=reproduce.",
      "done_when": { "ack": "reproduce" }, "on_error": "delegate" },
    { "id": "diagnose", "role": "dev", "timeout_secs": 3600, "depends_on": ["reproduce"],
      "start_template": "Diagnose bug {bug} using the reproduction steps; identify the root cause and propose a minimal fix. Finish with the ACK JSON contract, task_id=diagnose.",
      "done_when": { "ack": "diagnose" }, "on_error": { "rerun": { "max": 1 } } },
    { "id": "fix", "role": "dev", "timeout_secs": 3600, "depends_on": ["diagnose"],
      "start_template": "Implement and test the fix for bug {bug}; add a regression test. Finish with the ACK JSON contract, task_id=fix.",
      "done_when": { "ack": "fix" }, "on_error": { "rerun": { "max": 2 } } },
    { "id": "verify", "role": "tester", "timeout_secs": 3600, "depends_on": ["fix"],
      "start_template": "Verify the fix for bug {bug} against the reproduction steps; confirm the regression test passes. Finish with the ACK JSON contract, task_id=verify.",
      "done_when": { "ack": "verify" }, "on_error": "delegate" }
  ]
}"#;

/// Parse a default graph by id.
///
/// # Errors
/// Returns a [`crate::CoreError`] if the embedded graph is invalid (a bug in
/// the crate, not in user data).
pub fn default_graph(id: &str) -> crate::CoreResult<Workflow> {
    match id {
        "feature_lifecycle" => Workflow::parse_json(FEATURE_LIFECYCLE_JSON),
        "bug_flow" => Workflow::parse_json(BUG_FLOW_JSON),
        other => Err(crate::error::CoreError::InvalidGraph {
            id: other.to_owned(),
            reason: "unknown default graph".to_owned(),
        }),
    }
}

/// The ids of the shipped default graphs.
#[must_use]
pub fn default_graph_ids() -> &'static [&'static str] {
    &["feature_lifecycle", "bug_flow"]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::NodeState;

    #[test]
    fn feature_lifecycle_parses() {
        let wf = default_graph("feature_lifecycle").unwrap();
        assert_eq!(wf.graph().id, "feature_lifecycle");
        assert_eq!(wf.nodes().len(), 13);
        assert_eq!(wf.state("brainstorm"), Some(NodeState::Ready));
        assert_eq!(wf.state("high_level_design"), Some(NodeState::Pending));
        // The human gate carries the approved criterion and a loop_back.
        let gate = wf.node("hl_human_gate").unwrap();
        assert_eq!(gate.gate.as_deref(), Some("plannotator"));
        assert!(gate.done_when.is_human_gate());
        assert_eq!(gate.loop_back.as_ref().unwrap().small, "hl_human_gate");
        assert_eq!(gate.loop_back.as_ref().unwrap().big, "high_level_design");
        // Background deploy/promote nodes.
        assert_eq!(wf.node("deploy_dev").unwrap().mode, crate::types::AgentMode::Background);
        assert_eq!(wf.node("promote_prod").unwrap().mode, crate::types::AgentMode::Background);
        // dev has a rerun bound.
        assert_eq!(wf.node("dev").unwrap().on_error.max_reruns(), 2);
    }

    #[test]
    fn bug_flow_parses() {
        let wf = default_graph("bug_flow").unwrap();
        assert_eq!(wf.nodes().len(), 4);
        assert_eq!(wf.state("reproduce"), Some(NodeState::Ready));
        assert_eq!(wf.state("verify"), Some(NodeState::Pending));
    }

    #[test]
    fn every_agent_node_has_a_timeout_but_the_human_gate_does_not() {
        // Real-world catch (2026-08-14): the graphs shipped without per-node
        // timeouts, so an agent that drifted from the ACK contract hung its
        // node in `running` forever. Every non-gate node must bound its work.
        for id in default_graph_ids() {
            let wf = default_graph(id).unwrap();
            for node in wf.nodes() {
                if node.gate.is_some() {
                    assert_eq!(
                        node.timeout_secs, None,
                        "a human-paced gate node must not carry an agent timeout ({id}/{})",
                        node.id
                    );
                } else {
                    assert!(
                        node.timeout_secs.is_some(),
                        "agent node {id}/{} must set timeout_secs",
                        node.id
                    );
                }
            }
        }
    }

    #[test]
    fn unknown_default_graph_errors() {
        assert!(default_graph("nope").is_err());
    }

    #[test]
    fn feature_lifecycle_runs_end_to_end() {
        let mut wf = default_graph("feature_lifecycle").unwrap();
        // Advance the whole chain with plain ack()s, treating the human gate
        // as approved, then assert completion.
        let ack = |task: &str, approved: bool| crate::ack::Ack {
            task_id: task.to_owned(),
            status: crate::types::AckStatus::Done,
            summary: None,
            approved: Some(approved),
            needs_revision: None,
        };
        let mut safety = 0;
        while !wf.is_complete() && safety < 100 {
            safety += 1;
            let node = match wf.ready().first() {
                Some(n) => (*n).clone(),
                None => break,
            };
            let task = node.done_when.ack.clone().expect("node ack task");
            wf.start(&node.id).unwrap();
            let _ = wf.apply_ack(&ack(&task, true));
        }
        assert!(wf.is_complete(), "feature_lifecycle completes when gates approve");
    }

    #[test]
    fn feature_lifecycle_stalls_until_gate_approves() {
        let mut wf = default_graph("feature_lifecycle").unwrap();
        let reject = crate::ack::Ack {
            task_id: "hl_human_gate".to_owned(),
            status: crate::types::AckStatus::Done,
            summary: Some("rework".to_owned()),
            approved: Some(false),
            needs_revision: Some(crate::types::Revision::Small),
        };
        // Drive everything up to the gate, then reject once.
        let mut safety = 0;
        while safety < 100 {
            safety += 1;
            let node = match wf.ready().first() {
                Some(n) => (*n).clone(),
                None => break,
            };
            if node.id == "hl_human_gate" {
                wf.start(&node.id).unwrap();
                let _ = wf.apply_ack(&reject);
                break;
            }
            let task = node.done_when.ack.clone().expect("node ack task");
            wf.start(&node.id).unwrap();
            let _ = wf.apply_ack(&ack_plain(&task));
        }
        assert_eq!(
            wf.state("hl_human_gate"),
            Some(NodeState::Ready),
            "gate re-readied after small revision"
        );
        assert_eq!(wf.state("detailed_design"), Some(NodeState::Pending), "downstream holds");
    }

    fn ack_plain(task: &str) -> crate::ack::Ack {
        crate::ack::Ack {
            task_id: task.to_owned(),
            status: crate::types::AckStatus::Done,
            summary: None,
            approved: None,
            needs_revision: None,
        }
    }
}
