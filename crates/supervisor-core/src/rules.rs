//! The offline decision layer (C9): rules and the confidence cascade.
//!
//! Rules come in two kinds. **Data rules** are declarative TOML
//! (`rules.toml` / the `rule` table), so bake-back can propose them and the
//! supervisor can hot-reload them. **Code rules** are Rust functions registered
//! by the caller. Both produce an [`Action`] with a confidence and are scored
//! by the same [`RuleEngine`].
//!
//! The cascade: score every matching rule → the highest confidence ≥ threshold
//! wins (data beats code on ties); below threshold or a tied conflict →
//! escalate to the manager; nothing matched → escalate (an uncovered
//! situation is never silently ignored).
//!
//! Everything here is pure and unit-tested. The [`CounterStore`] is the
//! read-only counter source for rules like `times_errored_in_1h`; it is fed by
//! the daemon (signals + journal replay) and never mutated by a rule.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::{Duration, Instant};

use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize};

use crate::error::{CoreError, CoreResult};
use crate::signal::Signal;
use crate::types::{AgentId, AgentState, NodeState};

/// The default confidence below which a match is not acted on.
pub const DEFAULT_THRESHOLD: f64 = 0.8;

/// How long a rendered body may let `{last_output}` grow before truncation.
const MAX_LAST_OUTPUT_CHARS: usize = 2_000;

/// The facts a rule sees. Built by the daemon from the state store, the
/// observer, and the triggering signal; plain data, no I/O.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Situation {
    pub ws: String,
    pub agent: AgentId,
    pub agent_role: String,
    #[serde(default)]
    pub state: AgentState,
    /// 1.0 when observed; lower when inferred.
    #[serde(default = "one_f64")]
    pub state_confidence: f64,
    /// Why the situation arose, when a failure triggered it (`"exit"`, ...).
    pub reason: Option<String>,
    /// Signals observed around the situation, most recent last.
    #[serde(default)]
    pub signals: Vec<Signal>,
    /// The workflow node this agent is on, if any.
    pub node: Option<NodeRef>,
    /// Messages currently queued in the agent's inbox.
    #[serde(default)]
    pub inbox_depth: usize,
    /// The most recent output snapshot, available to rules that render a body.
    pub last_output: Option<String>,
}

fn one_f64() -> f64 {
    1.0
}

/// A workflow node reference, for `node_id` / `node.state` rule keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeRef {
    pub graph: String,
    pub node: String,
    pub state: NodeState,
}

impl Situation {
    /// The name of the most recent signal, for the `signal` rule key.
    #[must_use]
    pub fn last_signal_name(&self) -> Option<&'static str> {
        self.signals.last().map(Signal::name)
    }
}

/// What a matching rule tells the supervisor to do (§4.10).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    /// Post a message to an agent's inbox. `$agent` / `{last_output}` /
    /// `{node}` placeholders are rendered before delivery.
    Post { to: AgentId, body: String },
    /// Auto-answer a tool-permission prompt.
    RespondPermission { permission_id: String, allow: bool },
    /// Move the agent to a state explicitly.
    Transition { to: AgentState },
    /// Kick off a workflow with parameters.
    StartWorkflow { graph: String, params: BTreeMap<String, String> },
    /// Focus the agent's pane.
    FocusPane { ws: String, agent: AgentId },
    /// Explicitly do nothing.
    Noop,
    /// Hand the decision to the manager (C11), with a reason.
    Escalate { reason: String },
}

impl Action {
    /// Substitute `$agent`, `{agent}`, `{node}`, and `{last_output}`
    /// placeholders from the situation. Unknown placeholders are left as-is so
    /// a typo is visible in the rendered message rather than silently blanked.
    #[must_use]
    pub fn render(&self, sit: &Situation) -> Self {
        let render_text = |t: &str| {
            let out = t
                .replace("$agent", &sit.agent)
                .replace("{agent}", &sit.agent)
                .replace("{node}", sit.node.as_ref().map_or("", |n| n.node.as_str()));
            match &sit.last_output {
                Some(last) => {
                    let trimmed = last.chars().take(MAX_LAST_OUTPUT_CHARS).collect::<String>();
                    out.replace("{last_output}", &trimmed)
                }
                None => out.replace("{last_output}", "(no recent output)"),
            }
        };
        match self {
            Self::Post { to, body } => Self::Post { to: render_text(to), body: render_text(body) },
            Self::Escalate { reason } => Self::Escalate { reason: render_text(reason) },
            Self::StartWorkflow { graph, params } => {
                let params = params
                    .iter()
                    .map(|(k, v)| (k.clone(), render_text(v)))
                    .collect::<BTreeMap<_, _>>();
                Self::StartWorkflow { graph: graph.clone(), params }
            }
            Self::FocusPane { ws, agent } => {
                Self::FocusPane { ws: ws.clone(), agent: agent.clone() }
            }
            Self::RespondPermission { permission_id, allow } => {
                Self::RespondPermission { permission_id: permission_id.clone(), allow: *allow }
            }
            Self::Transition { to } => Self::Transition { to: *to },
            Self::Noop => Self::Noop,
        }
    }
}

/// A comparison operator against a typed value, parsed from TOML.
///
/// Scalar forms (`state = "error"`) mean equality; a table form names the
/// operator: `times_errored_in_1h = { lte = 1 }`. `Serialize` emits the same
/// canonical forms so a parsed rule round-trips back to TOML.
#[derive(Debug, Clone, PartialEq)]
pub enum Cmp<T> {
    Eq(T),
    NotEq(T),
    In(Vec<T>),
    Lt(T),
    Lte(T),
    Gte(T),
    Gt(T),
}

impl<T: PartialEq + PartialOrd> Cmp<T> {
    #[must_use]
    pub fn matches(&self, value: &T) -> bool {
        match self {
            Self::Eq(v) => value == v,
            Self::NotEq(v) => value != v,
            Self::In(vs) => vs.contains(value),
            Self::Lt(v) => value < v,
            Self::Lte(v) => value <= v,
            Self::Gte(v) => value >= v,
            Self::Gt(v) => value > v,
        }
    }
}

impl<T: Serialize> Serialize for Cmp<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Eq(v) => v.serialize(serializer),
            Self::NotEq(v) => single_key_map(serializer, "!=", v),
            Self::In(vs) => single_key_map(serializer, "in", vs),
            Self::Lt(v) => single_key_map(serializer, "<", v),
            Self::Lte(v) => single_key_map(serializer, "<=", v),
            Self::Gte(v) => single_key_map(serializer, ">=", v),
            Self::Gt(v) => single_key_map(serializer, ">", v),
        }
    }
}

fn single_key_map<S, T>(serializer: S, key: &str, value: &T) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
    T: Serialize + ?Sized,
{
    let mut map = serializer.serialize_map(Some(1))?;
    map.serialize_entry(key, value)?;
    map.end()
}

/// String comparisons, which also allow `contains`.
#[derive(Debug, Clone, PartialEq)]
pub enum StrCmp {
    Eq(String),
    NotEq(String),
    In(Vec<String>),
    Contains(String),
}

impl StrCmp {
    #[must_use]
    pub fn matches(&self, value: &str) -> bool {
        match self {
            Self::Eq(v) => value == v,
            Self::NotEq(v) => value != v,
            Self::In(vs) => vs.iter().any(|v| v == value),
            Self::Contains(needle) => value.contains(needle.as_str()),
        }
    }
}

impl Serialize for StrCmp {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Eq(v) => v.serialize(serializer),
            Self::NotEq(v) => single_key_map(serializer, "!=", v),
            Self::In(vs) => single_key_map(serializer, "in", vs),
            Self::Contains(v) => single_key_map(serializer, "contains", v),
        }
    }
}

/// The `when` clause of a data rule: every present field must match. Absent
/// fields impose no constraint; unknown keys disable the rule (logged on load).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Condition {
    /// `agent.role` (or the spec example's `agent.type`).
    pub agent_role: Option<StrCmp>,
    pub state: Option<Cmp<AgentState>>,
    /// `state.confidence`, under the flat key `state_confidence`.
    pub state_confidence: Option<Cmp<f64>>,
    /// Matches the situation's `reason` string exactly (use `contains` for a
    /// substring test).
    pub reason: Option<StrCmp>,
    /// Counter lookups, e.g. `times_errored_in_1h`.
    pub counters: BTreeMap<String, Cmp<u32>>,
    pub node_id: Option<StrCmp>,
    pub node_state: Option<Cmp<NodeState>>,
    /// Matches the most recent signal's name, e.g. `step.failed`.
    pub signal: Option<StrCmp>,
    /// Keys the loader did not recognize. The rule never matches while any are
    /// present; the daemon logs them at load.
    pub unknown_keys: Vec<String>,
}

impl Serialize for Condition {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut map = serializer.serialize_map(None)?;
        if let Some(cmp) = &self.agent_role {
            map.serialize_entry("agent_role", cmp)?;
        }
        if let Some(cmp) = &self.state {
            map.serialize_entry("state", cmp)?;
        }
        if let Some(cmp) = &self.state_confidence {
            map.serialize_entry("state_confidence", cmp)?;
        }
        if let Some(cmp) = &self.reason {
            map.serialize_entry("reason", cmp)?;
        }
        if let Some(cmp) = &self.node_id {
            map.serialize_entry("node_id", cmp)?;
        }
        if let Some(cmp) = &self.node_state {
            map.serialize_entry("node_state", cmp)?;
        }
        if let Some(cmp) = &self.signal {
            map.serialize_entry("signal", cmp)?;
        }
        for (key, cmp) in &self.counters {
            map.serialize_entry(key, cmp)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for Condition {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = toml::Value::deserialize(deserializer)?;
        Ok(Self::from_toml(&value))
    }
}

impl Condition {
    /// Does this situation satisfy every constraint the condition expresses?
    /// A condition with unknown keys never matches.
    #[must_use]
    pub fn matches(&self, sit: &Situation) -> bool {
        if !self.unknown_keys.is_empty() {
            return false;
        }
        if let Some(cmp) = &self.agent_role
            && !cmp.matches(&sit.agent_role)
        {
            return false;
        }
        if let Some(cmp) = &self.state
            && !cmp.matches(&sit.state)
        {
            return false;
        }
        if let Some(cmp) = &self.state_confidence
            && !cmp.matches(&sit.state_confidence)
        {
            return false;
        }
        if let Some(cmp) = &self.reason
            && !sit.reason.as_deref().is_some_and(|r| cmp.matches(r))
        {
            return false;
        }
        if let Some(node) = &self.node_id
            && !sit.node.as_ref().is_some_and(|n| node.matches(&n.node))
        {
            return false;
        }
        if let Some(state) = &self.node_state
            && !sit.node.as_ref().is_some_and(|n| state.matches(&n.state))
        {
            return false;
        }
        if let Some(sig) = &self.signal
            && !sit.last_signal_name().is_some_and(|name| sig.matches(name))
        {
            return false;
        }
        // Counter checks are evaluated against the engine's counter store (see
        // `RuleEngine::evaluate`), not the situation — they are not part of
        // this pure `matches`.
        true
    }

    /// Parse a `when` value from TOML. Dotted keys (`agent.type`) arrive as
    /// nested tables and are flattened; a table value names an operator.
    #[must_use]
    pub fn from_toml(value: &toml::Value) -> Self {
        let mut cond = Condition::default();
        let Some(table) = value.as_table() else {
            cond.unknown_keys.push("<not a table>".to_owned());
            return cond;
        };
        for (key, val) in table {
            match key.as_str() {
                "agent.role" | "agent" => match val {
                    toml::Value::Table(agent) => {
                        // I-23: a typo'd nested key (e.g. `agent = { r0le = ... }`)
                        // must disable the rule, not silently match everything.
                        for (k, v) in agent {
                            match k.as_str() {
                                "role" | "type" => {
                                    // I-23 residual: a wrong-typed value
                                    // (`role = 123`) must disable the rule,
                                    // not silently match everything.
                                    if let Some(cmp) = parse_str_cmp(v) {
                                        cond.agent_role = Some(cmp);
                                    } else {
                                        cond.unknown_keys.push(format!("agent.{k}"));
                                    }
                                }
                                other => cond.unknown_keys.push(format!("agent.{other}")),
                            }
                        }
                    }
                    _ => cond.agent_role = parse_str_cmp(val),
                },
                "agent.type" | "agent_role" => cond.agent_role = parse_str_cmp(val),
                "state" => cond.state = parse_enum_cmp::<AgentState>(val),
                "state.confidence" | "state_confidence" => {
                    cond.state_confidence = parse_num_cmp(val);
                }
                "reason" => cond.reason = parse_str_cmp(val),
                "node_id" => cond.node_id = parse_str_cmp(val),
                "node.state" | "node_state" => cond.node_state = parse_enum_cmp::<NodeState>(val),
                "signal" => cond.signal = parse_str_cmp(val),
                "times_errored_in_1h" => {
                    if let Some(cmp) = parse_u32_cmp(val) {
                        cond.counters.insert(key.clone(), cmp);
                    }
                }
                _ => cond.unknown_keys.push(key.clone()),
            }
        }
        cond
    }

    #[must_use]
    pub fn is_disabled(&self) -> bool {
        !self.unknown_keys.is_empty()
    }
}

/// One data rule (§4.10). Confidence 0–1; a match only acts when it clears the
/// engine's threshold.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub id: String,
    pub when: Condition,
    pub confidence: f64,
    pub action: Action,
}

impl Rule {
    /// Parse a TOML document of `[[rule]]` entries.
    ///
    /// # Errors
    /// Returns [`CoreError::InvalidRule`] if the input is not valid TOML, a
    /// rule violates the schema, or a confidence is outside 0–1.
    pub fn parse_toml(input: &str) -> CoreResult<Vec<Self>> {
        #[derive(Deserialize)]
        struct RulesFile {
            #[serde(default)]
            rule: Vec<Rule>,
        }
        let file: RulesFile = toml::from_str(input).map_err(|e| CoreError::InvalidRule {
            id: "<file>".to_owned(),
            reason: format!("invalid rules TOML: {e}"),
        })?;
        for rule in &file.rule {
            if !(0.0..=1.0).contains(&rule.confidence) {
                return Err(CoreError::InvalidRule {
                    id: rule.id.clone(),
                    reason: "confidence must be in 0.0–1.0".to_owned(),
                });
            }
        }
        Ok(file.rule)
    }
}

/// A code rule: a Rust function that may produce a decision for a situation.
pub trait CodeRule: Send + Sync {
    /// Stable id, used in reports and the decision log.
    fn id(&self) -> &'static str;
    /// Confidence of this rule's judgment, 0–1.
    fn confidence(&self) -> f64;
    /// Return the decision to take for `sit`, or `None` if this rule has no
    /// opinion here.
    fn evaluate(&self, sit: &Situation) -> Option<Decision>;
}

/// A decision with its rule's confidence (§4.10).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Decision {
    pub action: Action,
    pub confidence: f64,
}

/// A matched rule under consideration, for reporting and escalation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Candidate {
    pub rule_id: String,
    pub confidence: f64,
    pub action: Action,
}

/// The outcome of running a situation through the engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Evaluation {
    /// A single rule cleared the threshold; act on its rendered action.
    Act { decision: Decision, rule_id: String },
    /// Candidates exist but are below threshold, conflict, or nothing matched —
    /// the caller should escalate to the manager. Empty candidates mean an
    /// uncovered situation.
    Escalate { candidates: Vec<Candidate> },
}

/// What failed events count toward, for rules like `times_errored_in_1h`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Error,
    ToolFailed,
    StepFailed,
}

impl EventKind {
    /// The counter-store key a signal feeds.
    #[must_use]
    pub fn of(signal: &Signal) -> Option<Self> {
        match signal {
            Signal::SessionError { .. } | Signal::StepFailed { .. } => Some(Self::Error),
            Signal::ToolFailed { .. } => Some(Self::ToolFailed),
            _ => None,
        }
    }
}

/// The rolling-window event counter that rules read (`count(agent, kind,
/// window)`). Signals are not journaled (§4.18), so this store is rebuilt on
/// start from journaled `agent.state → error` transitions; counts for
/// non-journaled kinds start at zero and fill in as signals arrive.
#[derive(Debug, Default)]
pub struct CounterStore {
    /// `(ws, agent, kind)` -> event timestamps (newest last).
    events: HashMap<(String, String, EventKind), VecDeque<Instant>>,
}

impl CounterStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that an event of `kind` happened to an agent at `at`.
    pub fn record(&mut self, ws: &str, agent: &str, kind: EventKind, at: Instant) {
        self.events.entry((ws.to_owned(), agent.to_owned(), kind)).or_default().push_back(at);
    }

    /// Drop events older than `before` from every bucket.
    pub fn prune(&mut self, before: Instant) {
        self.events.retain(|_, queue| {
            queue.retain(|at| *at >= before);
            !queue.is_empty()
        });
    }

    /// How many events of `kind` for an agent fall within `window` ending now.
    #[must_use]
    pub fn count(
        &self,
        ws: &str,
        agent: &str,
        kind: EventKind,
        window: Duration,
        now: Instant,
    ) -> usize {
        let cutoff = now.checked_sub(window).unwrap_or(now);
        self.events
            .get(&(ws.to_owned(), agent.to_owned(), kind))
            .map_or(0, |queue| queue.iter().filter(|at| **at >= cutoff).count())
    }

    /// Rebuild the `Error` counts from journaled `agent.state → error`
    /// transitions (the journal records every `agent.state` change).
    pub fn rebuild_error_counts(
        &mut self,
        entries: impl IntoIterator<Item = (String, String, Instant)>,
    ) {
        for (ws, agent, at) in entries {
            self.record(&ws, &agent, EventKind::Error, at);
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

/// The offline decision engine. Holds the data rules, any registered code
/// rules, the confidence threshold, and the counter store.
pub struct RuleEngine {
    rules: Vec<Rule>,
    code_rules: Vec<Box<dyn CodeRule>>,
    threshold: f64,
    counters: CounterStore,
}

impl RuleEngine {
    #[must_use]
    pub fn new(threshold: f64) -> Self {
        Self { rules: Vec::new(), code_rules: Vec::new(), threshold, counters: CounterStore::new() }
    }

    #[must_use]
    pub fn with_rules(rules: Vec<Rule>, threshold: f64) -> Self {
        Self { rules, code_rules: Vec::new(), threshold, counters: CounterStore::new() }
    }

    /// Register a code rule. Code rules are consulted after data rules; the
    /// highest-confidence match wins across both kinds, with data beating code
    /// on ties.
    pub fn add_code_rule(&mut self, rule: impl CodeRule + 'static) {
        self.code_rules.push(Box::new(rule));
    }

    /// Replace the data rules wholesale (hot-reload after a rules-file edit).
    pub fn set_rules(&mut self, rules: Vec<Rule>) {
        self.rules = rules;
    }

    #[must_use]
    pub fn threshold(&self) -> f64 {
        self.threshold
    }

    #[must_use]
    pub fn data_rules(&self) -> &[Rule] {
        &self.rules
    }

    #[must_use]
    pub fn counters(&self) -> &CounterStore {
        &self.counters
    }

    #[must_use]
    pub fn counters_mut(&mut self) -> &mut CounterStore {
        &mut self.counters
    }

    /// Run the cascade: collect every matching rule (data + code), take the
    /// highest confidence, and decide.
    #[must_use]
    pub fn evaluate(&self, sit: &Situation) -> Evaluation {
        let count = |ws: &str, agent: &str, kind: EventKind| {
            self.counters.count(ws, agent, kind, Duration::from_hours(1), Instant::now())
        };

        let mut candidates: Vec<Candidate> = Vec::new();
        for rule in &self.rules {
            if rule.when.is_disabled() {
                continue;
            }
            if rule.when.matches(sit) && counters_hold(rule, sit, &count) {
                candidates.push(Candidate {
                    rule_id: rule.id.clone(),
                    confidence: rule.confidence,
                    action: rule.action.render(sit),
                });
            }
        }
        for code in &self.code_rules {
            if let Some(decision) = code.evaluate(sit) {
                candidates.push(Candidate {
                    rule_id: code.id().to_owned(),
                    confidence: decision.confidence,
                    action: decision.action.render(sit),
                });
            }
        }
        decide(candidates, self.threshold)
    }
}

/// The counter constraints of a rule, evaluated against a lookup closure.
fn counters_hold(
    rule: &Rule,
    sit: &Situation,
    count: &dyn Fn(&str, &str, EventKind) -> usize,
) -> bool {
    let mut counts = BTreeMap::new();
    for key in rule.when.counters.keys() {
        let value = match key.as_str() {
            "times_errored_in_1h" => {
                u32::try_from(count(&sit.ws, &sit.agent, EventKind::Error)).unwrap_or(u32::MAX)
            }
            _ => continue,
        };
        counts.insert(key.clone(), value);
    }
    rule.when
        .counters
        .iter()
        .all(|(key, cmp)| counts.get(key).is_some_and(|value| cmp.matches(value)))
}

/// The pure part of the cascade, separated so it is directly testable.
///
/// `data` beats `code` on equal confidence, so a data rule can always override
/// an identical-armed code rule. A tied top confidence with *different*
/// actions is a conflict → escalate.
fn decide(mut candidates: Vec<Candidate>, threshold: f64) -> Evaluation {
    if candidates.is_empty() {
        return Evaluation::Escalate { candidates: Vec::new() };
    }
    // Sort by confidence desc; on ties, data rules (source = data, kind rank 0)
    // come first. Candidates from data rules are tracked via a parallel sort
    // key: rules are all `data` here, code rules come from the trait. We encode
    // precedence by ordering confidence desc, then keeping insertion order is
    // insufficient, so we rank explicitly below.
    candidates.sort_by(|a, b| {
        b.confidence.total_cmp(&a.confidence).then_with(|| data_rank(a).cmp(&data_rank(b)))
    });
    let best = &candidates[0];
    if best.confidence < threshold {
        return Evaluation::Escalate { candidates };
    }
    let tied: Vec<&Candidate> =
        candidates.iter().filter(|c| floats_eq(c.confidence, best.confidence)).collect();
    let distinct_actions = tied.iter().map(|c| &c.action).collect::<std::collections::HashSet<_>>();
    if distinct_actions.len() > 1 {
        return Evaluation::Escalate { candidates };
    }
    Evaluation::Act {
        decision: Decision { action: tied[0].action.clone(), confidence: best.confidence },
        rule_id: best.rule_id.clone(),
    }
}

/// Exact-enough float equality for confidence tie-breaking.
fn floats_eq(a: f64, b: f64) -> bool {
    (a - b).abs() < f64::EPSILON
}

/// Precedence rank for tie-breaking: 0 = data rule, 1 = code rule.
fn data_rank(candidate: &Candidate) -> u8 {
    u8::from(candidate.rule_id.starts_with("code:"))
}

// --- TOML comparison parsing ----------------------------------------------

fn parse_str_cmp(value: &toml::Value) -> Option<StrCmp> {
    if value.is_table() {
        let op = operator_key(value)?;
        match op.as_str() {
            "!=" | "ne" => Some(StrCmp::NotEq(toml_to_scalar(value)?)),
            "in" => Some(StrCmp::In(toml_to_string_vec(value)?)),
            "contains" => Some(StrCmp::Contains(toml_to_scalar(value)?)),
            _ => None,
        }
    } else {
        Some(StrCmp::Eq(toml_to_scalar(value)?))
    }
}

fn parse_num_cmp(value: &toml::Value) -> Option<Cmp<f64>> {
    if value.is_table() {
        let op = operator_key(value)?;
        let scalar = toml_to_number(value)?;
        Some(match op.as_str() {
            "!=" | "ne" => Cmp::NotEq(scalar),
            "lte" | "<=" => Cmp::Lte(scalar),
            "gte" | ">=" => Cmp::Gte(scalar),
            "lt" | "<" => Cmp::Lt(scalar),
            "gt" | ">" => Cmp::Gt(scalar),
            "in" => Cmp::In(
                value
                    .as_table()
                    .and_then(|t| t.get("in"))?
                    .as_array()?
                    .iter()
                    .filter_map(toml_to_number)
                    .collect::<Vec<f64>>(),
            ),
            _ => return None,
        })
    } else {
        Some(Cmp::Eq(toml_to_number(value)?))
    }
}

/// Parse a counter comparison (`u32`). Counters come from TOML integers and
/// are small counts, so the float→u32 casts below cannot truncate in practice.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn parse_u32_cmp(value: &toml::Value) -> Option<Cmp<u32>> {
    let num = parse_num_cmp(value)?;
    Some(match num {
        Cmp::Eq(v) => Cmp::Eq(v as u32),
        Cmp::NotEq(v) => Cmp::NotEq(v as u32),
        Cmp::In(vs) => Cmp::In(vs.into_iter().map(|v| v as u32).collect()),
        Cmp::Lt(v) => Cmp::Lt(v as u32),
        Cmp::Lte(v) => Cmp::Lte(v as u32),
        Cmp::Gte(v) => Cmp::Gte(v as u32),
        Cmp::Gt(v) => Cmp::Gt(v as u32),
    })
}

fn parse_enum_cmp<T>(value: &toml::Value) -> Option<Cmp<T>>
where
    T: for<'de> Deserialize<'de> + PartialEq + PartialOrd,
{
    if value.is_table() {
        let op = operator_key(value)?;
        let scalar = toml_to_scalar(value)?;
        let parsed = parse_enum(&scalar)?;
        Some(match op.as_str() {
            "!=" | "ne" => Cmp::NotEq(parsed),
            "lte" | "<=" => Cmp::Lte(parsed),
            "gte" | ">=" => Cmp::Gte(parsed),
            "lt" | "<" => Cmp::Lt(parsed),
            "gt" | ">" => Cmp::Gt(parsed),
            "in" => Cmp::In(
                value
                    .as_table()
                    .and_then(|t| t.get("in"))?
                    .as_array()?
                    .iter()
                    .filter_map(toml_to_scalar)
                    .filter_map(|s| parse_enum(&s))
                    .collect::<Vec<T>>(),
            ),
            _ => return None,
        })
    } else {
        Some(Cmp::Eq(parse_enum(&toml_to_scalar(value)?)?))
    }
}

fn parse_enum<T>(text: &str) -> Option<T>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_str::<T>(&serde_json::Value::String(text.to_owned()).to_string()).ok()
}

/// For a scalar value, `None`. For a table, the single operator key it holds
/// (e.g. `lte`). A table that is not an operator form also yields `None`.
/// For an operator table (`{ lte = 1 }`), the single operator key it holds.
/// `None` for scalars, multi-key tables, and tables whose single key is not an
/// operator.
fn operator_key(value: &toml::Value) -> Option<String> {
    let toml::Value::Table(t) = value else { return None };
    if t.len() != 1 {
        return None;
    }
    let key = t.keys().next()?.clone();
    is_operator(&key).then_some(key)
}

fn is_operator(key: &str) -> bool {
    matches!(key, "=" | "!=" | "in" | "lte" | "gte" | "lt" | "gt" | "contains" | "eq" | "ne")
}

fn toml_to_scalar(value: &toml::Value) -> Option<String> {
    match value {
        toml::Value::String(s) => Some(s.clone()),
        toml::Value::Integer(i) => Some(i.to_string()),
        toml::Value::Float(f) => Some(f.to_string()),
        toml::Value::Boolean(b) => Some(b.to_string()),
        toml::Value::Table(t) => {
            let op = operator_key(value)?;
            if op == "in" {
                return None;
            }
            toml_to_scalar(t.values().next()?)
        }
        _ => None,
    }
}

/// `toml::Value` integer to a float for confidence-style comparisons. Counts
/// and confidences stay far inside `u64`, so the cast cannot lose precision in
/// practice.
#[allow(clippy::cast_precision_loss)]
fn toml_to_number(value: &toml::Value) -> Option<f64> {
    match value {
        toml::Value::Integer(i) => Some(u64::try_from(*i).map_or(0.0, |v| v as f64)),
        toml::Value::Float(f) => Some(*f),
        toml::Value::Table(t) => {
            let op = operator_key(value)?;
            if op == "in" {
                return None;
            }
            toml_to_number(t.values().next()?)
        }
        _ => None,
    }
}

fn toml_to_string_vec(value: &toml::Value) -> Option<Vec<String>> {
    let arr = value.as_table()?.get("in")?.as_array()?;
    arr.iter().map(toml_to_scalar).collect()
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::float_cmp,
        clippy::match_wildcard_for_single_variants,
        clippy::wildcard_in_or_patterns,
        clippy::cast_precision_loss
    )]
    use super::*;
    use crate::signal::Signal;

    fn sit() -> Situation {
        Situation {
            ws: "iot".to_owned(),
            agent: "tester_01".to_owned(),
            agent_role: "tester".to_owned(),
            state: AgentState::Error,
            reason: Some("exit".to_owned()),
            last_output: Some("crash log".to_owned()),
            signals: vec![Signal::StepFailed {
                ws: "iot".to_owned(),
                agent: "tester_01".to_owned(),
                error: Some("exit 1".to_owned()),
            }],
            ..Situation::default()
        }
    }

    #[test]
    fn parses_the_spec_rule_shape() {
        // The spec example spans two lines; inline tables must be single-line,
        // so the equivalent single-line form is parsed here.
        let rules = Rule::parse_toml(
            r#"
[[rule]]
id = "rerun_crashed_tester_once"
when = { agent.type = "tester", state = "error", reason = "exit", times_errored_in_1h = { lte = 1 } }
confidence = 0.9
action = { kind = "post", to = "$agent", body = "Your last run crashed. Re-run once." }
"#,
        )
        .unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].id, "rerun_crashed_tester_once");
        assert_eq!(rules[0].confidence, 0.9);
        assert_eq!(rules[0].when.agent_role, Some(StrCmp::Eq("tester".to_owned())));
        assert_eq!(rules[0].when.state, Some(Cmp::Eq(AgentState::Error)));
        assert_eq!(rules[0].when.reason, Some(StrCmp::Eq("exit".to_owned())));
        assert!(rules[0].when.counters.contains_key("times_errored_in_1h"));
    }

    #[test]
    fn confidence_out_of_range_is_rejected() {
        let bad = r#"
[[rule]]
id = "x"
when = {}
confidence = 1.1
action = { kind = "post", to = "a", body = "b" }
"#;
        assert!(Rule::parse_toml(bad).is_err());
    }

    #[test]
    fn condition_matches_only_when_every_constraint_holds() {
        let when: toml::Value = toml::from_str(
            "when = { agent.role = \"tester\", state = \"error\", reason = \"exit\" }",
        )
        .unwrap();
        let cond = Condition::from_toml(&when["when"]);
        assert!(cond.matches(&sit()));
        let idle = Situation { state: AgentState::Idle, ..sit() };
        assert!(!cond.matches(&idle));
        let other_role = Situation { agent_role: "dev".to_owned(), ..sit() };
        assert!(!cond.matches(&other_role));
        let other_reason = Situation { reason: Some("port".to_owned()), ..sit() };
        assert!(!cond.matches(&other_reason));
        let partial_reason = Situation { reason: Some("exit 1".to_owned()), ..sit() };
        assert!(
            !cond.matches(&partial_reason),
            "reason equality is exact; use contains for substrings"
        );
    }

    #[test]
    fn unknown_keys_disable_the_rule() {
        let when: toml::Value =
            toml::from_str("when = { bogus_key = 1, state = \"error\" }").unwrap();
        let cond = Condition::from_toml(&when["when"]);
        assert_eq!(cond.unknown_keys, vec!["bogus_key".to_owned()]);
        assert!(cond.is_disabled());
        assert!(!cond.matches(&sit()));
    }

    #[test]
    fn typo_in_nested_agent_table_disables_the_rule() {
        // I-23: `when = { agent = { r0le = "tester" } }` must not match
        // everything — the unknown nested key disables the rule.
        let when: toml::Value =
            toml::from_str(r#"when = { agent = { r0le = "tester" } }"#).unwrap();
        let cond = Condition::from_toml(&when["when"]);
        assert!(cond.is_disabled(), "typo'd nested key must disable the rule");
        assert!(!cond.matches(&sit()));
        assert!(cond.unknown_keys.iter().any(|k| k.contains("r0le")));
    }

    #[test]
    fn operators_match() {
        assert!(Cmp::Lte(1).matches(&1));
        assert!(Cmp::Lte(1).matches(&0));
        assert!(!Cmp::Lte(1).matches(&2));
        assert!(Cmp::In(vec![1, 2, 3]).matches(&2));
        assert!(!Cmp::In(vec![1, 2, 3]).matches(&9));
        assert!(StrCmp::Contains("port".to_owned()).matches("port collision"));
        assert!(!StrCmp::Contains("port".to_owned()).matches("no collision"));
        assert!(Cmp::Gte(0.8).matches(&0.9));
    }

    #[test]
    fn table_operator_parsing() {
        let when: toml::Value =
            toml::from_str("when = { times_errored_in_1h = { lte = 1 } }").unwrap();
        let c = Condition::from_toml(&when["when"]);
        let cmp = c.counters.get("times_errored_in_1h").unwrap();
        assert!(cmp.matches(&0));
        assert!(cmp.matches(&1));
        assert!(!cmp.matches(&2));
    }

    #[test]
    fn signal_field_matches_the_most_recent_signal() {
        let when: toml::Value = toml::from_str("when = { signal = \"step.failed\" }").unwrap();
        let cond = Condition::from_toml(&when["when"]);
        assert!(cond.matches(&sit()));
        let no_signal = Situation { signals: Vec::new(), ..sit() };
        assert!(!cond.matches(&no_signal));
    }

    #[test]
    fn placeholders_render_from_context() {
        let action = Action::Post {
            to: "$agent".to_owned(),
            body: "node {node} crashed: {last_output}".to_owned(),
        };
        let sit = Situation {
            node: Some(NodeRef {
                graph: "bug".to_owned(),
                node: "fix".to_owned(),
                state: NodeState::Running,
            }),
            ..sit()
        };
        let rendered = action.render(&sit);
        assert_eq!(
            rendered,
            Action::Post {
                to: "tester_01".to_owned(),
                body: "node fix crashed: crash log".to_owned(),
            }
        );
    }

    #[test]
    fn last_output_is_truncated() {
        let action = Action::Post { to: "x".to_owned(), body: "{last_output}".to_owned() };
        let sit = Situation { last_output: Some("y".repeat(MAX_LAST_OUTPUT_CHARS + 100)), ..sit() };
        match action.render(&sit) {
            Action::Post { body, .. } => {
                assert_eq!(body.chars().count(), MAX_LAST_OUTPUT_CHARS);
            }
            other => panic!("expected a post, got {other:?}"),
        }
    }

    #[test]
    fn highest_confidence_rule_wins() {
        let engine = RuleEngine::with_rules(
            Rule::parse_toml(
                r#"
[[rule]]
id = "low"
when = { state = "error" }
confidence = 0.5
action = { kind = "post", to = "a", body = "low" }

[[rule]]
id = "high"
when = { state = "error" }
confidence = 0.95
action = { kind = "post", to = "a", body = "high" }
"#,
            )
            .unwrap(),
            DEFAULT_THRESHOLD,
        );
        match engine.evaluate(&sit()) {
            Evaluation::Act { rule_id, decision, .. } => {
                assert_eq!(rule_id, "high");
                assert_eq!(decision.confidence, 0.95);
            }
            other => panic!("expected an act, got {other:?}"),
        }
    }

    #[test]
    fn below_threshold_escalates_with_candidates() {
        let engine = RuleEngine::with_rules(
            Rule::parse_toml(
                r#"
[[rule]]
id = "shy"
when = { state = "error" }
confidence = 0.4
action = { kind = "post", to = "a", body = "shy" }
"#,
            )
            .unwrap(),
            DEFAULT_THRESHOLD,
        );
        match engine.evaluate(&sit()) {
            Evaluation::Escalate { candidates } => {
                assert_eq!(candidates.len(), 1);
                assert_eq!(candidates[0].rule_id, "shy");
            }
            other => panic!("expected escalation, got {other:?}"),
        }
    }

    #[test]
    fn tied_conflicting_actions_escalate() {
        let engine = RuleEngine::with_rules(
            Rule::parse_toml(
                r#"
[[rule]]
id = "rerun"
when = { state = "error" }
confidence = 0.9
action = { kind = "post", to = "a", body = "rerun" }

[[rule]]
id = "ask_human"
when = { state = "error" }
confidence = 0.9
action = { kind = "escalate", reason = "rerun or wait?" }
"#,
            )
            .unwrap(),
            DEFAULT_THRESHOLD,
        );
        match engine.evaluate(&sit()) {
            Evaluation::Escalate { candidates } => assert_eq!(candidates.len(), 2),
            other => panic!("expected a conflict escalation, got {other:?}"),
        }
    }

    #[test]
    fn uncovered_situation_escalates_empty() {
        let engine = RuleEngine::new(DEFAULT_THRESHOLD);
        assert!(matches!(
            engine.evaluate(&sit()),
            Evaluation::Escalate { candidates } if candidates.is_empty()
        ));
    }

    #[test]
    fn data_beats_code_on_ties() {
        struct CodeRuleAlways;
        impl CodeRule for CodeRuleAlways {
            fn id(&self) -> &'static str {
                "code:always"
            }
            fn confidence(&self) -> f64 {
                0.9
            }
            fn evaluate(&self, _sit: &Situation) -> Option<Decision> {
                Some(Decision {
                    action: Action::Post { to: "a".to_owned(), body: "same body".to_owned() },
                    confidence: 0.9,
                })
            }
        }
        let mut engine = RuleEngine::with_rules(
            Rule::parse_toml(
                r#"
[[rule]]
id = "data_rule"
when = { state = "error" }
confidence = 0.9
action = { kind = "post", to = "a", body = "same body" }
"#,
            )
            .unwrap(),
            DEFAULT_THRESHOLD,
        );
        engine.add_code_rule(CodeRuleAlways);
        match engine.evaluate(&sit()) {
            Evaluation::Act { rule_id, .. } => assert_eq!(rule_id, "data_rule"),
            other => panic!("expected the data rule to win the tie, got {other:?}"),
        }
    }

    #[test]
    fn identical_tied_actions_do_not_conflict() {
        let engine = RuleEngine::with_rules(
            Rule::parse_toml(
                r#"
[[rule]]
id = "a"
when = { state = "error" }
confidence = 0.9
action = { kind = "post", to = "a", body = "same" }

[[rule]]
id = "b"
when = { state = "error" }
confidence = 0.9
action = { kind = "post", to = "a", body = "same" }
"#,
            )
            .unwrap(),
            DEFAULT_THRESHOLD,
        );
        assert!(matches!(engine.evaluate(&sit()), Evaluation::Act { .. }));
    }

    #[test]
    fn counter_store_counts_and_prunes() {
        let mut store = CounterStore::new();
        let t0 = Instant::now();
        store.record("iot", "tester_01", EventKind::Error, t0);
        store.record("iot", "tester_01", EventKind::Error, t0 + Duration::from_mins(1));
        store.record("iot", "dev_01", EventKind::Error, t0);
        let now = t0 + Duration::from_mins(2);
        assert_eq!(
            store.count("iot", "tester_01", EventKind::Error, Duration::from_hours(1), now),
            2
        );
        assert_eq!(store.count("iot", "dev_01", EventKind::Error, Duration::from_hours(1), now), 1);
        assert_eq!(store.count("iot", "ghost", EventKind::Error, Duration::from_hours(1), now), 0);
        store.prune(now.checked_sub(Duration::from_secs(90)).unwrap());
        assert_eq!(
            store.count("iot", "tester_01", EventKind::Error, Duration::from_hours(1), now),
            1,
            "the older event was pruned"
        );
    }

    #[test]
    fn counter_store_rebuilds_error_counts_from_journal() {
        let mut store = CounterStore::new();
        let t0 = Instant::now();
        store.rebuild_error_counts(vec![
            ("iot".to_owned(), "dev_01".to_owned(), t0),
            ("iot".to_owned(), "dev_01".to_owned(), t0 + Duration::from_secs(5)),
        ]);
        assert_eq!(
            store.count(
                "iot",
                "dev_01",
                EventKind::Error,
                Duration::from_hours(1),
                t0 + Duration::from_secs(10)
            ),
            2
        );
        assert_eq!(
            store.count("iot", "tester_01", EventKind::Error, Duration::from_hours(1), t0),
            0
        );
    }

    #[test]
    fn counter_rule_gates_on_the_store() {
        let mut engine = RuleEngine::with_rules(
            Rule::parse_toml(
                r#"
[[rule]]
id = "rerun_once"
when = { agent.role = "tester", state = "error", times_errored_in_1h = { lte = 1 } }
confidence = 0.9
action = { kind = "post", to = "$agent", body = "re-run once" }
"#,
            )
            .unwrap(),
            DEFAULT_THRESHOLD,
        );
        let situation = sit();
        // 2 errors in the window → rule must not fire.
        engine.counters_mut().record("iot", "tester_01", EventKind::Error, Instant::now());
        engine.counters_mut().record("iot", "tester_01", EventKind::Error, Instant::now());
        assert!(matches!(engine.evaluate(&situation), Evaluation::Escalate { .. }));
        // Pruning everything leaves the count at zero, so the rule fires.
        engine.counters_mut().prune(Instant::now() + Duration::from_secs(1));
        assert!(matches!(engine.evaluate(&situation), Evaluation::Act { .. }));
    }

    #[test]
    fn action_kinds_roundtrip_through_toml() {
        let rules = Rule::parse_toml(
            r#"
[[rule]]
id = "perm"
when = {}
confidence = 0.9
action = { kind = "respond_permission", permission_id = "p_1", allow = true }
"#,
        )
        .unwrap();
        assert_eq!(
            rules[0].action,
            Action::RespondPermission { permission_id: "p_1".to_owned(), allow: true }
        );
    }

    #[test]
    fn escalate_action_renders_placeholders() {
        let action = Action::Escalate { reason: "{agent} crashed: {last_output}".to_owned() };
        let rendered = action.render(&sit());
        assert_eq!(
            rendered,
            Action::Escalate { reason: "tester_01 crashed: crash log".to_owned() }
        );
    }
}
