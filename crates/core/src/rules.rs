//! The offline decision layer: rules and the confidence cascade.
//!
//! Rules come in two kinds (see `docs/specs/2026-08-10-orchestration.md`,
//! *Decisions 1*). **Data rules** are declarative and loadable from TOML, so
//! bake-back can propose them and the orchestrator can hot-reload them.
//! **Code rules** are Rust functions registered by the caller, for cases the
//! evaluator cannot express. Both produce [`RuleAction`]s with a confidence
//! and are scored by the same [`RuleEngine`].
//!
//! The engine never calls an LLM and never guesses: below the confidence
//! threshold, or on a conflict, it reports the candidates so the caller can
//! escalate to the manager. Everything here is pure and unit-tested.

use serde::{Deserialize, Serialize};

use crate::state::AgentState;

/// The default confidence below which a match is not acted on.
pub const DEFAULT_THRESHOLD: u8 = 80;

/// How long a rendered body may let `{last_output}` grow before truncation.
const MAX_LAST_OUTPUT_CHARS: usize = 2_000;

/// Facts available to rule matching. Built by the orchestrator from the state
/// store and the observer; plain data, no I/O.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleContext {
    pub agent_id: String,
    /// The role, e.g. `dev`, `tester`, `designer`, `manager`.
    pub agent_type: Option<String>,
    /// The agent's current state, when known.
    pub state: Option<AgentState>,
    /// The reason string from the last transition or signal
    /// (e.g. `"process exited 1"`).
    pub reason: Option<String>,
    /// Error exits/markers in the last hour, for rerun-bound rules.
    pub error_count_1h: u32,
    /// Messages currently queued in the agent's inbox.
    pub inbox_depth: u64,
    /// The workflow node this agent is working on, if any.
    pub node: Option<String>,
    /// The most recent output snapshot, available to rules that render a body.
    pub last_output: Option<String>,
}

impl RuleContext {
    #[must_use]
    pub fn new(agent_id: impl Into<String>) -> Self {
        Self { agent_id: agent_id.into(), ..Self::default() }
    }
}

/// The `when` clause of a data rule: every present field must match. Absent
/// fields impose no constraint.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct Condition {
    pub agent_type: Option<String>,
    pub state: Option<AgentState>,
    /// Match when the reason string contains this substring.
    pub reason_contains: Option<String>,
    /// Error exits/markers in the last hour must be at most this.
    pub error_count_lte: Option<u8>,
    /// The inbox must hold at least this many messages.
    pub inbox_depth_gte: Option<u64>,
    /// The agent must be on this workflow node.
    pub node: Option<String>,
}

impl Condition {
    /// Does this context satisfy every constraint the condition expresses?
    #[must_use]
    pub fn matches(&self, ctx: &RuleContext) -> bool {
        if let Some(t) = &self.agent_type
            && ctx.agent_type.as_deref() != Some(t.as_str())
        {
            return false;
        }
        if let Some(s) = self.state
            && ctx.state != Some(s)
        {
            return false;
        }
        if let Some(needle) = &self.reason_contains
            && !ctx.reason.as_deref().is_some_and(|reason| reason.contains(needle))
        {
            return false;
        }
        if let Some(max) = self.error_count_lte
            && ctx.error_count_1h > u32::from(max)
        {
            return false;
        }
        if let Some(min) = self.inbox_depth_gte
            && ctx.inbox_depth < min
        {
            return false;
        }
        if let Some(node) = &self.node
            && ctx.node.as_deref() != Some(node.as_str())
        {
            return false;
        }
        true
    }
}

/// What a matching rule tells the orchestrator to do.
///
/// `to`, `body`, and `question` are templates: `{agent}`, `{node}`, and
/// `{last_output}` are substituted from the context before the action runs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RuleAction {
    /// Post a message to an agent's inbox.
    Post { to: String, body: String },
    /// Delegate the decision to the manager, optionally with a question.
    Escalate { question: Option<String> },
}

impl RuleAction {
    /// Substitute `{agent}`, `{node}`, and `{last_output}` placeholders from
    /// the context. Unknown placeholders are left as-is so a typo is visible
    /// in the rendered message rather than silently blanked.
    #[must_use]
    pub fn render(&self, ctx: &RuleContext) -> RuleAction {
        match self {
            Self::Post { to, body } => {
                RuleAction::Post { to: render_template(to, ctx), body: render_template(body, ctx) }
            }
            Self::Escalate { question } => RuleAction::Escalate {
                question: question.as_deref().map(|q| render_template(q, ctx)),
            },
        }
    }
}

fn render_template(template: &str, ctx: &RuleContext) -> String {
    let out = template
        .replace("{agent}", &ctx.agent_id)
        .replace("{node}", ctx.node.as_deref().unwrap_or_default());
    match &ctx.last_output {
        Some(last) => {
            let trimmed = last.chars().take(MAX_LAST_OUTPUT_CHARS).collect::<String>();
            out.replace("{last_output}", &trimmed)
        }
        None => out.replace("{last_output}", "(no recent output)"),
    }
}

/// One data rule. Confidence 0–100; a match only acts when it clears the
/// engine's threshold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub id: String,
    pub when: Condition,
    pub confidence: u8,
    pub action: RuleAction,
}

impl Rule {
    /// Parse a TOML document of `[[rule]]` entries. Used by the orchestrator
    /// to load the rules file and by bake-back to validate proposed rules.
    ///
    /// # Errors
    /// Returns [`crate::CoreError::MalformedRecord`] if the input is not valid
    /// TOML or a rule violates the schema.
    pub fn parse_toml(input: &str) -> Result<Vec<Self>, crate::CoreError> {
        #[derive(Deserialize)]
        struct RulesFile {
            #[serde(default)]
            rule: Vec<Rule>,
        }
        let file: RulesFile = toml::from_str(input)
            .map_err(|e| crate::CoreError::MalformedRecord(format!("invalid rules TOML: {e}")))?;
        if file.rule.is_empty() {
            return Err(crate::CoreError::MalformedRecord(
                "rules file contains no [[rule]] entries".to_owned(),
            ));
        }
        for rule in &file.rule {
            if rule.confidence > 100 {
                return Err(crate::CoreError::MalformedRecord(format!(
                    "rule {}: confidence must be 0–100",
                    rule.id
                )));
            }
        }
        Ok(file.rule)
    }
}

/// A code rule: a Rust function that may produce an action for a context.
/// Registered with a [`RuleEngine`] and scored like any data rule.
pub trait CodeRule: Send + Sync {
    /// Stable id, used in reports and the decision log.
    fn id(&self) -> &'static str;
    /// Confidence of this rule's judgment, 0–100.
    fn confidence(&self) -> u8;
    /// Return the action to take for `ctx`, or `None` if this rule has no
    /// opinion here.
    fn evaluate(&self, ctx: &RuleContext) -> Option<RuleAction>;
}

/// A matched rule that is being considered, for reporting and escalation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub rule_id: String,
    pub confidence: u8,
    pub action: RuleAction,
}

/// The outcome of running a context through the engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Evaluation {
    /// A single rule cleared the threshold; act on its rendered action.
    Act { decision: RuleAction, rule_id: String, confidence: u8 },
    /// Candidates exist but are below the threshold or conflict — the caller
    /// should delegate to the manager, showing what the rules considered.
    Escalate { candidates: Vec<Candidate> },
    /// Nothing matched. Whether this is worth escalating is the caller's call
    /// (an idle heart-beat is not; a fresh error is).
    NoMatch,
}

/// The offline decision engine. Holds the data rules, any registered code
/// rules, and the confidence threshold.
#[derive(Default)]
pub struct RuleEngine {
    data_rules: Vec<Rule>,
    code_rules: Vec<Box<dyn CodeRule>>,
    threshold: u8,
}

impl RuleEngine {
    #[must_use]
    pub fn new(threshold: u8) -> Self {
        Self { data_rules: Vec::new(), code_rules: Vec::new(), threshold }
    }

    #[must_use]
    pub fn with_rules(rules: Vec<Rule>, threshold: u8) -> Self {
        Self { data_rules: rules, code_rules: Vec::new(), threshold }
    }

    /// Register a code rule. Code rules are consulted after data rules; the
    /// highest-confidence match wins across both kinds.
    pub fn add_code_rule(&mut self, rule: impl CodeRule + 'static) {
        self.code_rules.push(Box::new(rule));
    }

    #[must_use]
    pub fn threshold(&self) -> u8 {
        self.threshold
    }

    /// Replace the data rules wholesale (hot-reload after a rules-file edit).
    pub fn set_rules(&mut self, rules: Vec<Rule>) {
        self.data_rules = rules;
    }

    #[must_use]
    pub fn data_rules(&self) -> &[Rule] {
        &self.data_rules
    }

    /// Run the cascade: collect every matching rule, take the highest
    /// confidence, and decide.
    #[must_use]
    pub fn evaluate(&self, ctx: &RuleContext) -> Evaluation {
        let mut candidates: Vec<Candidate> = Vec::new();
        for rule in &self.data_rules {
            if rule.when.matches(ctx) {
                candidates.push(Candidate {
                    rule_id: rule.id.clone(),
                    confidence: rule.confidence,
                    action: rule.action.render(ctx),
                });
            }
        }
        for code in &self.code_rules {
            if let Some(action) = code.evaluate(ctx) {
                candidates.push(Candidate {
                    rule_id: code.id().to_owned(),
                    confidence: code.confidence(),
                    action: action.render(ctx),
                });
            }
        }
        decide(candidates, self.threshold)
    }
}

/// The pure part of the cascade, separated so it is directly testable.
fn decide(mut candidates: Vec<Candidate>, threshold: u8) -> Evaluation {
    if candidates.is_empty() {
        return Evaluation::NoMatch;
    }
    candidates.sort_by_key(|c| std::cmp::Reverse(c.confidence));
    let best = &candidates[0];
    if best.confidence < threshold {
        return Evaluation::Escalate { candidates };
    }
    // A conflict is only a conflict if the top confidence is *tied*: a strictly
    // higher-confidence rule wins outright.
    let tied: Vec<&Candidate> =
        candidates.iter().filter(|c| c.confidence == best.confidence).collect();
    let distinct_actions = tied.iter().map(|c| &c.action).collect::<std::collections::HashSet<_>>();
    if distinct_actions.len() > 1 {
        return Evaluation::Escalate { candidates };
    }
    Evaluation::Act {
        decision: tied[0].action.clone(),
        rule_id: best.rule_id.clone(),
        confidence: best.confidence,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AgentState;

    fn ctx() -> RuleContext {
        RuleContext::new("tester_01").with_agent_type("tester")
    }

    impl RuleContext {
        fn with_agent_type(mut self, t: &str) -> Self {
            self.agent_type = Some(t.to_owned());
            self
        }
        fn with_state(mut self, s: AgentState) -> Self {
            self.state = Some(s);
            self
        }
    }

    const RULE: &str = r#"
[[rule]]
id = "rerun_crashed_tester_once"
when = { agent_type = "tester", state = "error", reason_contains = "process exited", error_count_lte = 1 }
confidence = 90
action = { kind = "post", to = "{agent}", body = "Your run crashed: {last_output}. Re-run once." }
"#;

    #[test]
    fn parses_the_spec_rule_shape() {
        let rules = Rule::parse_toml(RULE).unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].id, "rerun_crashed_tester_once");
        assert_eq!(rules[0].confidence, 90);
        assert_eq!(
            rules[0].when,
            Condition {
                agent_type: Some("tester".to_owned()),
                state: Some(AgentState::Error),
                reason_contains: Some("process exited".to_owned()),
                error_count_lte: Some(1),
                ..Condition::default()
            }
        );
    }

    #[test]
    fn empty_rules_file_is_rejected() {
        assert!(Rule::parse_toml("").is_err());
        assert!(Rule::parse_toml("[other]").is_err());
    }

    #[test]
    fn confidence_out_of_range_is_rejected() {
        let bad = r#"
[[rule]]
id = "x"
when = {}
confidence = 101
action = { kind = "post", to = "a", body = "b" }
"#;
        assert!(Rule::parse_toml(bad).is_err());
    }

    #[test]
    fn condition_matches_only_when_every_constraint_holds() {
        let cond = Condition {
            agent_type: Some("tester".to_owned()),
            state: Some(AgentState::Error),
            error_count_lte: Some(1),
            ..Condition::default()
        };
        assert!(cond.matches(&ctx().with_state(AgentState::Error)));
        assert!(!cond.matches(&ctx().with_state(AgentState::Idle)));
        assert!(!cond.matches(&ctx().with_state(AgentState::Error).with_agent_type("dev")));
        let counted = ctx().with_state(AgentState::Error);
        let mut counted = counted;
        counted.error_count_1h = 2;
        assert!(!cond.matches(&counted));
    }

    #[test]
    fn reason_substring_must_be_present() {
        let cond = Condition { reason_contains: Some("port".to_owned()), ..Condition::default() };
        let mut c = ctx();
        c.reason = Some("process exited 1".to_owned());
        assert!(!cond.matches(&c));
        c.reason = Some("port collision".to_owned());
        assert!(cond.matches(&c));
    }

    #[test]
    fn placeholders_render_from_context() {
        let action = RuleAction::Post {
            to: "{agent}".to_owned(),
            body: "node {node} crashed: {last_output}".to_owned(),
        };
        let mut c = ctx();
        c.node = Some("test".to_owned());
        c.last_output = Some("kaboom".to_owned());
        let rendered = action.render(&c);
        assert_eq!(
            rendered,
            RuleAction::Post {
                to: "tester_01".to_owned(),
                body: "node test crashed: kaboom".to_owned(),
            }
        );
    }

    #[test]
    fn last_output_is_truncated() {
        let action = RuleAction::Post { to: "x".to_owned(), body: "{last_output}".to_owned() };
        let mut c = ctx();
        c.last_output = Some("y".repeat(MAX_LAST_OUTPUT_CHARS + 100));
        let rendered = action.render(&c);
        match rendered {
            RuleAction::Post { body, .. } => {
                assert_eq!(body.chars().count(), MAX_LAST_OUTPUT_CHARS);
            }
            RuleAction::Escalate { .. } => panic!("expected a post"),
        }
    }

    #[test]
    fn highest_confidence_rule_wins() {
        let rules = Rule::parse_toml(
            r#"
[[rule]]
id = "low"
when = { state = "error" }
confidence = 50
action = { kind = "post", to = "a", body = "low" }

[[rule]]
id = "high"
when = { state = "error" }
confidence = 95
action = { kind = "post", to = "a", body = "high" }
"#,
        )
        .unwrap();
        let engine = RuleEngine::with_rules(rules, DEFAULT_THRESHOLD);
        let eval = engine.evaluate(&ctx().with_state(AgentState::Error));
        match eval {
            Evaluation::Act { rule_id, confidence, .. } => {
                assert_eq!(rule_id, "high");
                assert_eq!(confidence, 95);
            }
            other => panic!("expected an act, got {other:?}"),
        }
    }

    #[test]
    fn below_threshold_escalates_with_candidates() {
        let rules = Rule::parse_toml(
            r#"
[[rule]]
id = "shy"
when = { state = "error" }
confidence = 40
action = { kind = "post", to = "a", body = "shy" }
"#,
        )
        .unwrap();
        let engine = RuleEngine::with_rules(rules, DEFAULT_THRESHOLD);
        match engine.evaluate(&ctx().with_state(AgentState::Error)) {
            Evaluation::Escalate { candidates } => {
                assert_eq!(candidates.len(), 1);
                assert_eq!(candidates[0].rule_id, "shy");
            }
            other => panic!("expected escalation, got {other:?}"),
        }
    }

    #[test]
    fn tied_conflicting_actions_escalate() {
        let rules = Rule::parse_toml(
            r#"
[[rule]]
id = "rerun"
when = { state = "error" }
confidence = 90
action = { kind = "post", to = "a", body = "rerun" }

[[rule]]
id = "ask_human"
when = { state = "error" }
confidence = 90
action = { kind = "escalate", question = "rerun or wait?" }
"#,
        )
        .unwrap();
        let engine = RuleEngine::with_rules(rules, DEFAULT_THRESHOLD);
        match engine.evaluate(&ctx().with_state(AgentState::Error)) {
            Evaluation::Escalate { candidates } => assert_eq!(candidates.len(), 2),
            other => panic!("expected a conflict escalation, got {other:?}"),
        }
    }

    #[test]
    fn identical_tied_actions_do_not_conflict() {
        let rules = Rule::parse_toml(
            r#"
[[rule]]
id = "a"
when = { state = "error" }
confidence = 90
action = { kind = "post", to = "a", body = "same" }

[[rule]]
id = "b"
when = { state = "error" }
confidence = 90
action = { kind = "post", to = "a", body = "same" }
"#,
        )
        .unwrap();
        let engine = RuleEngine::with_rules(rules, DEFAULT_THRESHOLD);
        assert!(matches!(
            engine.evaluate(&ctx().with_state(AgentState::Error)),
            Evaluation::Act { .. }
        ));
    }

    #[test]
    fn no_match_when_conditions_do_not_hold() {
        let rules = Rule::parse_toml(RULE).unwrap();
        let engine = RuleEngine::with_rules(rules, DEFAULT_THRESHOLD);
        assert!(matches!(
            engine.evaluate(&ctx().with_state(AgentState::Idle)),
            Evaluation::NoMatch
        ));
    }

    #[test]
    fn code_rules_participate_in_the_cascade() {
        struct CrashRule;
        impl CodeRule for CrashRule {
            fn id(&self) -> &'static str {
                "code:always_post"
            }
            fn confidence(&self) -> u8 {
                85
            }
            fn evaluate(&self, _ctx: &RuleContext) -> Option<RuleAction> {
                Some(RuleAction::Post { to: "a".to_owned(), body: "from code".to_owned() })
            }
        }
        let mut engine = RuleEngine::with_rules(Vec::new(), DEFAULT_THRESHOLD);
        engine.add_code_rule(CrashRule);
        match engine.evaluate(&ctx()) {
            Evaluation::Act { rule_id, .. } => assert_eq!(rule_id, "code:always_post"),
            other => panic!("expected the code rule to act, got {other:?}"),
        }
    }

    #[test]
    fn escalation_render_keeps_question_templates() {
        let action = RuleAction::Escalate { question: Some("what should {agent} do?".to_owned()) };
        let rendered = action.render(&ctx());
        assert_eq!(
            rendered,
            RuleAction::Escalate { question: Some("what should tester_01 do?".to_owned()) }
        );
    }
}
