//! Decision log clustering and bake-back (§4.13).
//!
//! Decisions are clustered by a normalized signature (ids stripped; role,
//! state, signal, node kept). Signatures with ≥ `min_occurrences` produce a
//! proposed `[[rule]]` TOML block whose confidence is the observed outcome
//! success rate (floored at 0.6). Proposals persist with a stable
//! `proposal_<ulid>` id and move through `pending → applied|rejected|expired`.
//!
//! All of this is pure: the daemon reads the decision log, calls [`propose`],
//! and persists the results.

use std::collections::BTreeMap;

use crate::error::{CoreError, CoreResult};
use crate::rules::{Action, Situation};
use crate::signal::Signal;
use crate::types::{DecisionRecord, Proposal, ProposalStatus};

/// The minimum success rate a proposed rule carries; never below this.
pub const MIN_PROPOSAL_CONFIDENCE: f64 = 0.6;

/// A group of decisions sharing a signature.
#[derive(Debug, Clone, PartialEq)]
pub struct Cluster {
    pub signature: String,
    pub decisions: Vec<DecisionRecord>,
}

/// Compute the normalized signature of a situation: strip ids (ws, agent,
/// inbox depth, output), keep role + state + signals + node (§4.13).
#[must_use]
pub fn normalized_signature(sit: &Situation) -> String {
    let mut parts = vec![
        format!("role={}", sit.agent_role),
        format!("state={}", serde_json::to_string(&sit.state).unwrap_or_default()),
    ];
    let mut signals: Vec<&'static str> = sit.signals.iter().map(Signal::name).collect();
    signals.sort_unstable();
    signals.dedup();
    if !signals.is_empty() {
        parts.push(format!("signals={}", signals.join(",")));
    }
    if let Some(node) = &sit.node {
        parts.push(format!("node={}/{}", node.graph, node.node));
        parts
            .push(format!("node_state={}", serde_json::to_string(&node.state).unwrap_or_default()));
    }
    parts.join("|")
}

/// Cluster decision rows by signature, preserving first-seen order.
#[must_use]
pub fn cluster(decisions: &[DecisionRecord]) -> Vec<Cluster> {
    let mut by_sig: BTreeMap<&str, Cluster> = BTreeMap::new();
    for d in decisions {
        by_sig.entry(&d.signature).and_modify(|c| c.decisions.push(d.clone())).or_insert_with(
            || Cluster { signature: d.signature.clone(), decisions: vec![d.clone()] },
        );
    }
    by_sig.into_values().collect()
}

/// Generate proposals for clusters meeting `min_occurrences`. Un-proposable
/// clusters (no extractable rule) are skipped.
#[must_use]
pub fn propose(clusters: &[Cluster], min_occurrences: usize) -> Vec<Proposal> {
    let now = crate::time::now_rfc3339();
    let mut out = Vec::new();
    for cluster in clusters {
        if cluster.decisions.len() < min_occurrences {
            continue;
        }
        let Some(first) = cluster.decisions.first() else { continue };
        let confidence = observed_success_rate(&cluster.decisions);
        let Some(rule_toml) = generate_rule_toml(first, confidence) else {
            continue;
        };
        out.push(Proposal {
            id: format!("proposal_{}", crate::time::new_ulid()),
            rule_toml,
            signature: cluster.signature.clone(),
            cluster_size: cluster.decisions.len(),
            confidence,
            status: ProposalStatus::Pending,
            created_at: now.clone(),
            resolved_at: None,
        });
    }
    out
}

/// The observed outcome success rate for a cluster's decisions, floored at
/// [`MIN_PROPOSAL_CONFIDENCE`]. Decisions without a recorded outcome count as
/// non-successes.
#[must_use]
pub fn observed_success_rate(decisions: &[DecisionRecord]) -> f64 {
    if decisions.is_empty() {
        return MIN_PROPOSAL_CONFIDENCE;
    }
    let successes = decisions
        .iter()
        .filter(|d| {
            d.outcome.as_ref().and_then(|o| o.get("success")).and_then(serde_json::Value::as_bool)
                == Some(true)
        })
        .count();
    let total = u32::try_from(decisions.len()).map_or(1.0, f64::from);
    let ok = u32::try_from(successes).map_or(0.0, f64::from);
    let rate = if total > 0.0 { ok / total } else { 0.0 };
    rate.max(MIN_PROPOSAL_CONFIDENCE)
}

/// Build the `[[rule]]` TOML block for a proposal from one representative
/// decision (its situation + action), generalizing the target to `$agent`.
/// The embedded confidence is the CLUSTER's observed success rate, not the
/// single representative decision's (review I-22 — a 0.25-success cluster
/// used to produce a rule that always cleared the 0.8 threshold).
#[must_use]
pub fn generate_rule_toml(decision: &DecisionRecord, cluster_confidence: f64) -> Option<String> {
    let sit: Situation = serde_json::from_value(decision.situation.clone()).ok()?;
    let action: Action = serde_json::from_value(decision.decision.clone()).ok()?;
    let generalized = generalize(&action, &sit);
    build_rule_toml(&sit, &generalized, cluster_confidence).ok()
}

/// Replace the specific agent with `$agent` in an action's `to`/`body` so the
/// proposed rule generalizes to any matching agent.
fn generalize(action: &Action, sit: &Situation) -> Action {
    let swap = |s: &str| s.replace(&sit.agent, "$agent");
    match action {
        Action::Post { to, body } => Action::Post { to: swap(to), body: swap(body) },
        Action::Escalate { reason } => Action::Escalate { reason: swap(reason) },
        Action::FocusPane { ws, agent } => Action::FocusPane { ws: ws.clone(), agent: swap(agent) },
        Action::StartWorkflow { graph, params } => Action::StartWorkflow {
            graph: graph.clone(),
            params: params.iter().map(|(k, v)| (k.clone(), swap(v))).collect(),
        },
        other => other.clone(),
    }
}

/// Render the `when` clause of a proposed rule from the situation's fixed
/// facts: role, state, the last signal, and the node.
fn when_for(sit: &Situation) -> toml::Value {
    let mut table = toml::map::Map::new();
    table.insert("agent_role".to_owned(), toml::Value::String(sit.agent_role.clone()));
    table.insert(
        "state".to_owned(),
        toml::Value::String(
            serde_json::to_string(&sit.state).unwrap_or_default().trim_matches('"').to_owned(),
        ),
    );
    if let Some(name) = sit.last_signal_name() {
        table.insert("signal".to_owned(), toml::Value::String(name.to_owned()));
    }
    if let Some(node) = &sit.node {
        table.insert("node_id".to_owned(), toml::Value::String(node.node.clone()));
    }
    toml::Value::Table(table)
}

fn build_rule_toml(sit: &Situation, action: &Action, confidence: f64) -> CoreResult<String> {
    let action_value: toml::Value =
        toml::Value::try_from(action).map_err(|e| CoreError::InvalidRule {
            id: "bakeback".to_owned(),
            reason: format!("action does not serialize to TOML: {e}"),
        })?;
    let doc = toml::Value::Table(toml::map::Map::from_iter([(
        "rule".to_owned(),
        toml::Value::Array(vec![toml::Value::Table(toml::map::Map::from_iter([
            ("id".to_owned(), toml::Value::String(format!("bakeback_{}", crate::time::new_ulid()))),
            ("when".to_owned(), when_for(sit)),
            ("confidence".to_owned(), toml::Value::Float(confidence)),
            ("action".to_owned(), action_value),
        ]))]),
    )]));
    toml::to_string(&doc).map_err(|e| CoreError::InvalidRule {
        id: "bakeback".to_owned(),
        reason: format!("proposed rule does not serialize: {e}"),
    })
}

/// Mark proposals expired once `created_at` is before `cutoff_ts`. Only
/// `pending` proposals are affected.
#[must_use]
pub fn expire(proposals: &[Proposal], cutoff_ts: &str) -> Vec<Proposal> {
    proposals
        .iter()
        .map(|p| {
            if p.status == ProposalStatus::Pending && p.created_at.as_str() < cutoff_ts {
                let mut p = p.clone();
                p.status = ProposalStatus::Expired;
                p.resolved_at = Some(crate::time::now_rfc3339());
                p
            } else {
                p.clone()
            }
        })
        .collect()
}

/// Resolve a proposal: `apply` marks it applied, `reject` marks it rejected.
/// Already-resolved proposals are returned unchanged (a no-op, per §4.13).
#[must_use]
pub fn resolve(proposal: &Proposal, apply: bool) -> Proposal {
    if proposal.status != ProposalStatus::Pending {
        return proposal.clone();
    }
    let mut p = proposal.clone();
    p.status = if apply { ProposalStatus::Applied } else { ProposalStatus::Rejected };
    p.resolved_at = Some(crate::time::now_rfc3339());
    p
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::float_cmp,
        clippy::match_single_binding,
        clippy::wildcard_in_or_patterns,
        clippy::cast_precision_loss
    )]
    use super::*;
    use crate::rules::{Action, NodeRef};
    use crate::types::{AgentState, NodeState};

    fn situation(
        state: AgentState,
        signals: Vec<&'static str>,
        node: Option<(&str, &str, NodeState)>,
    ) -> Situation {
        Situation {
            ws: "iot".to_owned(),
            agent: "tester_01".to_owned(),
            agent_role: "tester".to_owned(),
            state,
            signals: signals.into_iter().map(signal_for).collect(),
            node: node.map(|(g, n, s)| NodeRef {
                graph: g.to_owned(),
                node: n.to_owned(),
                state: s,
            }),
            last_output: Some("crash log".to_owned()),
            ..Situation::default()
        }
    }

    fn signal_for(name: &'static str) -> Signal {
        match name {
            "step.failed" => {
                Signal::StepFailed { ws: "w".to_owned(), agent: "a".to_owned(), error: None }
            }
            "tool.failed" => Signal::ToolFailed {
                ws: "w".to_owned(),
                agent: "a".to_owned(),
                name: "bash".to_owned(),
            },
            "session.error" => Signal::SessionError { ws: "w".to_owned(), agent: "a".to_owned() },
            _ => Signal::Heartbeat { ws: "w".to_owned() },
        }
    }

    fn decision(sit: &Situation) -> DecisionRecord {
        DecisionRecord {
            id: "dec_1".to_owned(),
            signature: normalized_signature(sit),
            situation: serde_json::to_value(sit).unwrap(),
            decision: serde_json::to_value(Action::Post {
                to: "tester_01".to_owned(),
                body: "re-run once".to_owned(),
            })
            .unwrap(),
            outcome: Some(serde_json::json!({ "success": true })),
            ts: "2026-08-13T00:00:00.000Z".to_owned(),
        }
    }

    #[test]
    fn signatures_strip_ids_and_keep_facts() {
        let a = situation(AgentState::Error, vec!["step.failed"], None);
        let b = situation(AgentState::Error, vec!["step.failed"], None);
        assert_eq!(normalized_signature(&a), normalized_signature(&b));
        let with_agent = situation(AgentState::Error, vec!["step.failed"], None);
        let mut other_agent = with_agent.clone();
        other_agent.agent = "tester_09".to_owned();
        other_agent.ws = "other".to_owned();
        assert_eq!(normalized_signature(&with_agent), normalized_signature(&other_agent));
        let different_state = situation(AgentState::Idle, vec!["step.failed"], None);
        assert_ne!(normalized_signature(&with_agent), normalized_signature(&different_state));
    }

    #[test]
    fn signature_includes_signals_and_node() {
        let a = situation(
            AgentState::Error,
            vec!["step.failed"],
            Some(("bug", "fix", NodeState::Running)),
        );
        let b = situation(AgentState::Error, vec!["step.failed"], None);
        assert_ne!(normalized_signature(&a), normalized_signature(&b));
        let c = situation(AgentState::Error, vec!["tool.failed"], None);
        assert_ne!(normalized_signature(&a), normalized_signature(&c));
    }

    #[test]
    fn cluster_groups_by_signature() {
        let s1 = situation(AgentState::Error, vec!["step.failed"], None);
        let s2 = situation(AgentState::Error, vec!["tool.failed"], None);
        let d1 = decision(&s1);
        let d2 = decision(&s1);
        let d3 = decision(&s2);
        let clusters = cluster(&[d1, d2, d3]);
        assert_eq!(clusters.len(), 2);
        assert_eq!(
            clusters.iter().find(|c| c.signature.contains("step.failed")).unwrap().decisions.len(),
            2
        );
        assert_eq!(
            clusters.iter().find(|c| c.signature.contains("tool.failed")).unwrap().decisions.len(),
            1
        );
    }

    #[test]
    fn propose_respects_min_occurrences() {
        let s = situation(AgentState::Error, vec!["step.failed"], None);
        let decisions = vec![decision(&s), decision(&s)];
        let clusters = cluster(&decisions);
        assert!(propose(&clusters, 3).is_empty(), "2 occurrences < 3 minimum");
        let proposals = propose(&clusters, 2);
        assert_eq!(proposals.len(), 1);
        assert_eq!(proposals[0].cluster_size, 2);
        assert!(proposals[0].id.starts_with("proposal_"));
        assert_eq!(proposals[0].status, ProposalStatus::Pending);
    }

    #[test]
    fn proposed_rule_parses_back_and_generalizes() {
        let s = situation(
            AgentState::Error,
            vec!["step.failed"],
            Some(("bug", "fix", NodeState::Running)),
        );
        let d = decision(&s);
        let toml = generate_rule_toml(&d, 0.7).expect("proposal TOML generated");
        let rules = crate::rules::Rule::parse_toml(&toml).expect("proposed TOML re-parses");
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].when.agent_role, Some(crate::rules::StrCmp::Eq("tester".to_owned())));
        assert_eq!(rules[0].when.state, Some(crate::rules::Cmp::Eq(AgentState::Error)));
        assert!(rules[0].id.starts_with("bakeback_"));
        match &rules[0].action {
            Action::Post { to, .. } => assert_eq!(to, "$agent", "generalized to $agent"),
            other => panic!("expected a post, got {other:?}"),
        }
        assert_eq!(
            rules[0].confidence, 0.7,
            "I-22: the embedded confidence is the cluster's rate, not the single decision's"
        );
    }

    #[test]
    fn success_rate_is_floored_at_min() {
        let s = situation(AgentState::Error, vec!["step.failed"], None);
        let mut d1 = decision(&s);
        d1.outcome = None;
        assert_eq!(observed_success_rate(&[d1]), MIN_PROPOSAL_CONFIDENCE);
        let mut d2 = decision(&s);
        d2.outcome = Some(serde_json::json!({ "success": true }));
        let mut d3 = decision(&s);
        d3.outcome = Some(serde_json::json!({ "success": false }));
        assert!(
            (observed_success_rate(&[d2, d3]) - 0.6).abs() < 1e-9,
            "1/2 success floored to 0.6"
        );
    }

    #[test]
    fn resolve_is_idempotent() {
        let s = situation(AgentState::Error, vec!["step.failed"], None);
        let clusters = cluster(&[decision(&s), decision(&s), decision(&s)]);
        let proposals = propose(&clusters, 3);
        let applied = resolve(&proposals[0], true);
        assert_eq!(applied.status, ProposalStatus::Applied);
        assert!(applied.resolved_at.is_some());
        assert_eq!(resolve(&applied, true), applied, "apply on an applied proposal is a no-op");
        let rejected = resolve(&proposals[0], false);
        assert_eq!(rejected.status, ProposalStatus::Rejected);
    }

    #[test]
    fn expire_marks_old_pending_proposals() {
        let s = situation(AgentState::Error, vec!["step.failed"], None);
        let clusters = cluster(&[decision(&s), decision(&s), decision(&s)]);
        let proposals = propose(&clusters, 3);
        // created_at is "now"; a future cutoff expires them all.
        let expired = expire(&proposals, "2999-01-01T00:00:00.000Z");
        assert_eq!(expired[0].status, ProposalStatus::Expired);
        // A past cutoff expires nothing.
        let kept = expire(&proposals, "2020-01-01T00:00:00.000Z");
        assert_eq!(kept[0].status, ProposalStatus::Pending);
    }
}
