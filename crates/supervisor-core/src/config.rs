//! Config file shapes (§6.1 project-local, §6.2 supervisor root) and their
//! TOML parsing. Pure: the daemon reads files and passes the strings here.

use std::ops::RangeInclusive;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, CoreResult};
use crate::ports::{
    DEFAULT_API_PORT, DEFAULT_PORT_RANGE, DEFAULT_RESERVED_PORTS, DEFAULT_SUPERVISOR_PORT,
};
use crate::rules::DEFAULT_THRESHOLD;
use crate::types::{AgentMode, DriverKind, RosterAgent};

/// How the project's `[server] port` is configured.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum PortSetting {
    Fixed(u16),
    /// `"auto"` → the allocator picks.
    Auto(String),
}

impl<'de> serde::Deserialize<'de> for PortSetting {
    /// Untagged parsing silently turned a quoted `port = "4200"` into `Auto`
    /// (the allocator then picked a random port). Accept an integer, `"auto"`,
    /// or a quoted number; reject anything else loudly.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct PortVisitor;
        impl serde::de::Visitor<'_> for PortVisitor {
            type Value = PortSetting;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an integer port, \"auto\", or a quoted port number")
            }
            fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                u16::try_from(v).map(PortSetting::Fixed).map_err(serde::de::Error::custom)
            }
            fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                u16::try_from(v).map(PortSetting::Fixed).map_err(serde::de::Error::custom)
            }
            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if v == "auto" {
                    Ok(PortSetting::Auto(v.to_owned()))
                } else {
                    v.parse::<u16>().map(PortSetting::Fixed).map_err(|_| {
                        serde::de::Error::custom(format!(
                            "port must be a number or \"auto\", got {v:?}"
                        ))
                    })
                }
            }
        }
        deserializer.deserialize_any(PortVisitor)
    }
}

/// The `[project]` section of a project's `supervisor.toml`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectSection {
    pub name: String,
    pub path: String,
}

/// The `[server]` section.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerSection {
    /// Fixed port or `"auto"`.
    #[serde(default)]
    pub port: Option<PortSetting>,
    /// Default agent/role for sessions created via `POST /session` (never a
    /// `serve` flag — `serve` has no `--agent`).
    pub default_agent: Option<String>,
}

/// The `[workflow]` section of a project config.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectWorkflowSection {
    #[serde(default)]
    pub graphs: Vec<String>,
}

/// The `[ingest]` section of a project config.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectIngestSection {
    #[serde(default)]
    pub github: Option<GithubAdapterConfig>,
}

/// The GitHub-issues adapter config.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubAdapterConfig {
    pub repo: String,
    #[serde(default = "default_poll_secs")]
    pub poll_secs: u64,
}

fn default_poll_secs() -> u64 {
    300
}

/// A project's `supervisor.toml` (§6.1).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    pub project: ProjectSection,
    #[serde(default)]
    pub server: ServerSection,
    #[serde(default)]
    pub agent: Vec<RosterAgent>,
    #[serde(default)]
    pub workflow: ProjectWorkflowSection,
    #[serde(default)]
    pub ingest: ProjectIngestSection,
}

impl ProjectConfig {
    /// Parse project TOML text.
    ///
    /// # Errors
    /// [`CoreError::InvalidConfig`] for schema violations or an empty name.
    pub fn parse(input: &str) -> CoreResult<Self> {
        let cfg: ProjectConfig = toml::from_str(input)
            .map_err(|e| CoreError::InvalidConfig(format!("invalid supervisor.toml: {e}")))?;
        if cfg.project.name.trim().is_empty() {
            return Err(CoreError::InvalidConfig("project.name must not be empty".to_owned()));
        }
        for agent in &cfg.agent {
            if agent.id.trim().is_empty() {
                return Err(CoreError::InvalidConfig(
                    "an [[agent]] id must not be empty".to_owned(),
                ));
            }
            if agent.role.trim().is_empty() {
                return Err(CoreError::InvalidConfig(
                    "an [[agent]] role must not be empty".to_owned(),
                ));
            }
        }
        Ok(cfg)
    }

    /// The configured server port, or `None` for `"auto"` / absent.
    #[must_use]
    pub fn fixed_port(&self) -> Option<u16> {
        match &self.server.port {
            Some(PortSetting::Fixed(p)) => Some(*p),
            Some(PortSetting::Auto(s)) if s == "auto" => None,
            _ => None,
        }
    }
}

/// The `[supervisor]` section of the root config.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupervisorSection {
    #[serde(default = "default_workspace_root")]
    pub workspace_root: PathBuf,
    #[serde(default = "default_port_range")]
    pub port_range: Vec<u16>,
    #[serde(default = "default_reserved_ports")]
    pub reserved_ports: Vec<u16>,
    #[serde(default = "default_api_port")]
    pub api_port: u16,
    #[serde(default = "default_supervisor_ws_port")]
    pub supervisor_workspace_port: u16,
    #[serde(default = "default_open_workspaces")]
    pub open_workspaces_on_start: bool,
    /// F5: start the supervisor workspace (`opencode serve :4199`) at daemon
    /// startup. Default true; `false` lets tests/CI skip opencode.
    #[serde(default = "default_open_supervisor_workspace")]
    pub open_supervisor_workspace: bool,
    #[serde(default = "default_cmux_bin")]
    pub cmux_bin: String,
    #[serde(default = "default_opencode_bin")]
    pub opencode_bin: String,
}

fn default_workspace_root() -> PathBuf {
    PathBuf::from("~/development")
}
fn default_port_range() -> Vec<u16> {
    DEFAULT_PORT_RANGE.clone().collect()
}
fn default_reserved_ports() -> Vec<u16> {
    DEFAULT_RESERVED_PORTS.to_vec()
}
fn default_api_port() -> u16 {
    DEFAULT_API_PORT
}
fn default_supervisor_ws_port() -> u16 {
    DEFAULT_SUPERVISOR_PORT
}
fn default_open_workspaces() -> bool {
    true
}
fn default_open_supervisor_workspace() -> bool {
    true
}
fn default_cmux_bin() -> String {
    "/Applications/cmux.app/Contents/Resources/bin/cmux".to_owned()
}
fn default_opencode_bin() -> String {
    "opencode".to_owned()
}

impl Default for SupervisorSection {
    fn default() -> Self {
        Self {
            workspace_root: default_workspace_root(),
            port_range: default_port_range(),
            reserved_ports: default_reserved_ports(),
            api_port: default_api_port(),
            supervisor_workspace_port: default_supervisor_ws_port(),
            open_workspaces_on_start: default_open_workspaces(),
            open_supervisor_workspace: default_open_supervisor_workspace(),
            cmux_bin: default_cmux_bin(),
            opencode_bin: default_opencode_bin(),
        }
    }
}

/// The `[workflow]` section of the root config.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootWorkflowSection {
    #[serde(default = "default_graphs")]
    pub default_graphs: Vec<String>,
}

fn default_graphs() -> Vec<String> {
    vec!["feature_lifecycle".to_owned(), "bug_flow".to_owned()]
}

impl Default for RootWorkflowSection {
    fn default() -> Self {
        Self { default_graphs: default_graphs() }
    }
}

/// The `[rule]` section of the root config.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootRuleSection {
    #[serde(default = "default_threshold")]
    pub threshold: f64,
    #[serde(default = "default_reload")]
    pub reload: String,
}

fn default_threshold() -> f64 {
    DEFAULT_THRESHOLD
}
fn default_reload() -> String {
    "auto".to_owned()
}

impl Default for RootRuleSection {
    fn default() -> Self {
        Self { threshold: default_threshold(), reload: default_reload() }
    }
}

/// The `[bakeback]` gate knob.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoApprove {
    #[default]
    Never,
    LowRisk,
}

/// The `[bakeback]` section of the root config.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootBakebackSection {
    #[serde(default = "default_min_occurrences")]
    pub min_occurrences: usize,
    #[serde(default)]
    pub auto_approve: AutoApprove,
    /// F6: run preview + expire on a daily timer (default true).
    #[serde(default = "default_auto_preview")]
    pub auto_preview: bool,
}

fn default_min_occurrences() -> usize {
    3
}

fn default_auto_preview() -> bool {
    true
}

impl Default for RootBakebackSection {
    fn default() -> Self {
        Self {
            min_occurrences: default_min_occurrences(),
            auto_approve: AutoApprove::Never,
            auto_preview: default_auto_preview(),
        }
    }
}

/// The `[graceful]` section of the root config.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootGracefulSection {
    #[serde(default = "default_off_timeout")]
    pub off_timeout_secs: u64,
}

fn default_off_timeout() -> u64 {
    120
}

impl Default for RootGracefulSection {
    fn default() -> Self {
        Self { off_timeout_secs: default_off_timeout() }
    }
}

/// The `[ingest]` section of the root config.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootIngestSection {
    #[serde(default = "default_ingest_sources")]
    pub sources: Vec<String>,
}

fn default_ingest_sources() -> Vec<String> {
    vec!["github".to_owned(), "app-feedback".to_owned(), "cli".to_owned()]
}

/// A model price (US$ per million tokens).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPrice {
    pub in_per_mtok: f64,
    pub out_per_mtok: f64,
}

/// The `[usage]` section of the root config (§3.3): model prices for the cost
/// estimates. Unknown models → tokens only, cost `null`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootUsageSection {
    #[serde(default)]
    pub model_prices: std::collections::BTreeMap<String, ModelPrice>,
}

impl RootUsageSection {
    /// Compute an estimated cost in USD cents for a model + token counts.
    /// `None` when the model has no price (shown as "—", never 0).
    #[must_use]
    pub fn cost_cents(
        &self,
        model: &str,
        prompt_tokens: u64,
        completion_tokens: u64,
    ) -> Option<f64> {
        let price = self.model_prices.get(model)?;
        let in_mtok = u64_to_f64(prompt_tokens) / 1_000_000.0;
        let out_mtok = u64_to_f64(completion_tokens) / 1_000_000.0;
        Some((price.in_per_mtok * in_mtok + price.out_per_mtok * out_mtok) * 100.0)
    }
}

/// Tokens are small counts far inside `u64`; the cast cannot lose precision in
/// practice.
#[allow(clippy::cast_precision_loss)]
fn u64_to_f64(v: u64) -> f64 {
    v as f64
}

/// The supervisor root config `~/.supervisor/supervisor.toml` (§6.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[derive(Default)]
pub struct SupervisorConfig {
    #[serde(default)]
    pub supervisor: SupervisorSection,
    #[serde(default)]
    pub workflow: RootWorkflowSection,
    #[serde(default)]
    pub rule: RootRuleSection,
    #[serde(default)]
    pub bakeback: RootBakebackSection,
    #[serde(default)]
    pub graceful: RootGracefulSection,
    #[serde(default)]
    pub ingest: RootIngestSection,
    #[serde(default)]
    pub usage: RootUsageSection,
}

impl SupervisorConfig {
    /// Parse root config TOML text, applying defaults for absent sections.
    ///
    /// # Errors
    /// [`CoreError::InvalidConfig`] when the TOML is malformed.
    pub fn parse(input: &str) -> CoreResult<Self> {
        let cfg: SupervisorConfig = toml::from_str(input)
            .map_err(|e| CoreError::InvalidConfig(format!("invalid supervisor config: {e}")))?;
        cfg.validate()
    }

    fn validate(self) -> CoreResult<Self> {
        let range: RangeInclusive<u16> = RangeInclusive::new(
            *self.supervisor.port_range.first().unwrap_or(&4100),
            *self.supervisor.port_range.last().unwrap_or(&4299),
        );
        if range.start() > range.end() {
            return Err(CoreError::InvalidConfig(
                "port_range must be [low, high] with low <= high".to_owned(),
            ));
        }
        if !self.supervisor.reserved_ports.contains(&self.supervisor.api_port)
            || !self.supervisor.reserved_ports.contains(&self.supervisor.supervisor_workspace_port)
        {
            return Err(CoreError::InvalidConfig(
                "api_port and supervisor_workspace_port must be in reserved_ports".to_owned(),
            ));
        }
        for p in &self.supervisor.reserved_ports {
            if !range.contains(p) {
                return Err(CoreError::InvalidConfig(format!(
                    "reserved port {p} is outside the configured port_range"
                )));
            }
        }
        Ok(self)
    }

    /// The allocator range as an inclusive range.
    #[must_use]
    pub fn port_range(&self) -> RangeInclusive<u16> {
        let first = *self.supervisor.port_range.first().unwrap_or(&4100);
        let last = *self.supervisor.port_range.last().unwrap_or(&4299);
        RangeInclusive::new(first, last)
    }

    /// The reserved set handed to the allocator.
    #[must_use]
    pub fn reserved_ports(&self) -> Vec<u16> {
        self.supervisor.reserved_ports.clone()
    }
}

/// The `[workflow] graphs` for a project, or the root default when absent.
#[must_use]
pub fn graphs_for(project: &ProjectConfig, root: &SupervisorConfig) -> Vec<String> {
    if project.workflow.graphs.is_empty() {
        root.workflow.default_graphs.clone()
    } else {
        project.workflow.graphs.clone()
    }
}

/// Build a default `supervisor.toml` for `supervisor add <path>`, with the
/// given roster.
#[must_use]
pub fn default_project_toml(
    name: &str,
    path: &str,
    roster: &[RosterAgent],
    port: Option<u16>,
) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = write!(
        out,
        "[project]\nname = {}\npath = {}\n\n[server]\nport = {}\ndefault_agent = {}\n\n",
        toml_str(name),
        toml_str(path),
        match port {
            Some(p) => p.to_string(),
            None => "\"auto\"".to_owned(),
        },
        toml_str(roster.first().map_or("dev", |a| a.id.as_str())),
    );
    for agent in roster {
        let _ = write!(
            out,
            "[[agent]]\nid = {}\nrole = {}\n",
            toml_str(&agent.id),
            toml_str(&agent.role)
        );
        if let Some(model) = &agent.model {
            let _ = writeln!(out, "model = {}", toml_str(model));
        }
        if agent.driver != DriverKind::Opencode {
            let _ = writeln!(
                out,
                "driver = {:?}",
                serde_json::to_string(&agent.driver).unwrap_or_default().trim_matches('"')
            );
        }
        if agent.mode != AgentMode::Foreground {
            let _ = writeln!(
                out,
                "mode = {:?}",
                serde_json::to_string(&agent.mode).unwrap_or_default().trim_matches('"')
            );
        }
        out.push('\n');
    }
    out
}

/// Escape a string for TOML inline use (strings here are safe ASCII slugs, but
/// a literal `"` still breaks the file).
fn toml_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
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

    const PROJECT_TOML: &str = r#"
[project]
name = "iot_platform"
path = "~/development/iot_platform"

[server]
port = 4101
default_agent = "dev"

[[agent]]
id = "dev_01"
role = "dev"
model = "anthropic/claude-sonnet-4"

[[agent]]
id = "reviewer_01"
role = "reviewer"
mode = "background"

[workflow]
graphs = ["feature_lifecycle", "bug_flow"]

[ingest]
github = { repo = "acme/iot_platform", poll_secs = 300 }
"#;

    #[test]
    fn parses_the_spec_project_config() {
        let cfg = ProjectConfig::parse(PROJECT_TOML).unwrap();
        assert_eq!(cfg.project.name, "iot_platform");
        assert_eq!(cfg.fixed_port(), Some(4101));
        assert_eq!(cfg.server.default_agent.as_deref(), Some("dev"));
        assert_eq!(cfg.agent.len(), 2);
        assert_eq!(cfg.agent[0].role, "dev");
        assert_eq!(cfg.agent[1].mode, AgentMode::Background);
        assert_eq!(cfg.ingest.github.as_ref().unwrap().repo, "acme/iot_platform");
        assert_eq!(cfg.ingest.github.as_ref().unwrap().poll_secs, 300);
    }

    #[test]
    fn missing_project_sections_default() {
        let cfg = ProjectConfig::parse("[project]\nname = \"x\"\npath = \"/x\"\n").unwrap();
        assert!(cfg.fixed_port().is_none());
        assert!(cfg.agent.is_empty());
        assert_eq!(cfg.ingest.github, None);
    }

    #[test]
    fn empty_name_is_rejected() {
        assert!(ProjectConfig::parse("[project]\nname = \"\"\npath = \"/x\"\n").is_err());
    }

    #[test]
    fn auto_port_setting_parses() {
        let cfg = ProjectConfig::parse(
            "[project]\nname = \"x\"\npath = \"/x\"\n[server]\nport = \"auto\"\n",
        )
        .unwrap();
        assert_eq!(cfg.fixed_port(), None);
        let cfg =
            ProjectConfig::parse("[project]\nname = \"x\"\npath = \"/x\"\n[server]\nport = 4200\n")
                .unwrap();
        assert_eq!(cfg.fixed_port(), Some(4200));
    }

    #[test]
    fn root_config_applies_defaults() {
        let cfg = SupervisorConfig::parse("").unwrap();
        assert_eq!(cfg.supervisor.api_port, DEFAULT_API_PORT);
        assert_eq!(cfg.supervisor.supervisor_workspace_port, DEFAULT_SUPERVISOR_PORT);
        assert!(cfg.supervisor.reserved_ports.contains(&4198));
        assert!(cfg.supervisor.open_workspaces_on_start);
        assert_eq!(cfg.rule.threshold, DEFAULT_THRESHOLD);
        assert_eq!(cfg.bakeback.min_occurrences, 3);
        assert_eq!(cfg.bakeback.auto_approve, AutoApprove::Never);
        assert_eq!(cfg.graceful.off_timeout_secs, 120);
    }

    #[test]
    fn root_config_parses_spec_values() {
        let cfg = SupervisorConfig::parse(
            r"
[supervisor]
reserved_ports = [4198, 4199]
port_range = [4100, 4299]
bakeback_min = 4
",
        );
        // Unknown key `bakeback_min` is rejected by deny_unknown_fields.
        assert!(cfg.is_err());
    }

    #[test]
    fn reserved_ports_must_be_in_range() {
        let cfg = SupervisorConfig::parse(
            "[supervisor]\nport_range = [4000, 4099]\nreserved_ports = [4198, 4199]\n",
        );
        assert!(cfg.is_err(), "reserved ports outside the range are rejected");
    }

    #[test]
    fn reserved_must_include_the_workspace_ports() {
        let cfg = SupervisorConfig::parse(
            "[supervisor]\nreserved_ports = [4198]\napi_port = 4198\nsupervisor_workspace_port = 4199\n",
        );
        assert!(cfg.is_err(), "4199 must be reserved too");
    }

    #[test]
    fn graphs_fall_back_to_root_defaults() {
        let root = SupervisorConfig::default();
        let project = ProjectConfig::default();
        assert_eq!(
            graphs_for(&project, &root),
            vec!["feature_lifecycle".to_owned(), "bug_flow".to_owned()]
        );
    }

    #[test]
    fn default_project_toml_roundtrips() {
        let roster = vec![
            RosterAgent {
                id: "dev_01".to_owned(),
                role: "dev".to_owned(),
                model: Some("anthropic/claude-sonnet-4".to_owned()),
                driver: DriverKind::Opencode,
                mode: AgentMode::Foreground,
            },
            RosterAgent {
                id: "reviewer_01".to_owned(),
                role: "reviewer".to_owned(),
                model: None,
                driver: DriverKind::Opencode,
                mode: AgentMode::Background,
            },
        ];
        let toml = default_project_toml("iot", "/x/iot", &roster, Some(4101));
        let cfg = ProjectConfig::parse(&toml).unwrap();
        assert_eq!(cfg.agent.len(), 2);
        assert_eq!(cfg.fixed_port(), Some(4101));
        assert_eq!(cfg.agent[1].mode, AgentMode::Background);
    }

    #[test]
    fn default_project_toml_defaults_port_to_auto() {
        let toml = default_project_toml("iot", "/x/iot", &[], None);
        let cfg = ProjectConfig::parse(&toml).unwrap();
        assert_eq!(cfg.fixed_port(), None);
    }

    #[test]
    fn port_range_computes_inclusive() {
        let cfg = SupervisorConfig::parse("[supervisor]\nport_range = [4100, 4299]\n").unwrap();
        assert_eq!(cfg.port_range(), 4100..=4299);
    }

    #[test]
    fn usage_cost_is_estimated_or_null() {
        let cfg = SupervisorConfig::parse(
            "[usage]\nmodel_prices = { \"anthropic/claude-sonnet-4\" = { in_per_mtok = 3.0, out_per_mtok = 15.0 } }",
        )
        .unwrap();
        // 1M in + 1M out tokens → $3 + $15 = $18 = 1800 cents.
        let cents =
            cfg.usage.cost_cents("anthropic/claude-sonnet-4", 1_000_000, 1_000_000).unwrap();
        assert!((cents - 1800.0).abs() < 0.001);
        assert_eq!(cfg.usage.cost_cents("unknown/model", 100, 100), None);
        assert_eq!(cfg.usage.model_prices.len(), 1);
    }
}
