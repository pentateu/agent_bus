//! The decision log + bake-back service (C12).
//!
//! Clusters the decision log by signature (normalized: strip ids, keep
//! state+signal+role+node), turns signatures with ≥ `min_occurrences` into
//! persisted `proposal_<ulid>` proposals, and applies/rejects/expires them.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use supervisor_core::bakeback::{cluster, expire, propose, resolve as resolve_proposal};
use supervisor_core::types::{Proposal, ProposalStatus, StoredRule};
use supervisor_core::{now_rfc3339, rules::Rule};
use tokio::sync::Mutex as AsyncMutex;

use crate::state::Fleet;

/// Proposal expiry window (§4.13).
pub const PROPOSAL_TTL_DAYS: u64 = 30;

/// Bake-back service.
pub struct BakebackService {
    fleet: Arc<AsyncMutex<Fleet>>,
    min_occurrences: usize,
    rules_toml: PathBuf,
}

impl BakebackService {
    /// Build the service.
    #[must_use]
    pub fn new(fleet: Arc<AsyncMutex<Fleet>>, min_occurrences: usize, rules_toml: PathBuf) -> Self {
        Self { fleet, min_occurrences, rules_toml }
    }

    /// Cluster the decision log and persist new proposals. Returns the newly
    /// created proposals.
    ///
    /// # Errors
    /// Any projection failure.
    pub async fn preview(&self) -> Result<Vec<Proposal>> {
        let clusters = {
            let fleet = self.fleet.lock().await;
            cluster(fleet.decisions())
        };
        let proposals = propose(&clusters, self.min_occurrences);
        let mut fleet = self.fleet.lock().await;
        let mut created = Vec::new();
        for proposal in &proposals {
            if fleet.proposal(&proposal.id).is_none() {
                fleet.upsert_proposal(proposal)?;
                created.push(proposal.clone());
            }
        }
        Ok(created)
    }

    /// Apply a proposal: validate its rule TOML, merge it into the rule table
    /// and `rules.toml`, and mark the proposal applied. A no-op for an already
    /// resolved proposal (§4.13).
    ///
    /// # Errors
    /// Unknown proposal or a generated rule that does not parse.
    pub async fn apply(&self, id: &str) -> Result<bool> {
        let proposal = {
            let fleet = self.fleet.lock().await;
            fleet.proposal(id).cloned().context("unknown proposal")?
        };
        if proposal.status != ProposalStatus::Pending {
            return Ok(false);
        }
        // Validate the generated rule before merging it.
        let rules =
            Rule::parse_toml(&proposal.rule_toml).context("proposed rule does not parse")?;
        let Some(rule) = rules.first() else {
            anyhow::bail!("proposed rule block is empty");
        };
        let stored = StoredRule {
            id: rule.id.clone(),
            toml: proposal.rule_toml.clone(),
            source: "bakeback".to_owned(),
            confidence: rule.confidence,
            approved: true,
            active: true,
            created_at: now_rfc3339(),
        };
        {
            let mut fleet = self.fleet.lock().await;
            fleet.upsert_rule(&stored)?;
            let resolved = resolve_proposal(&proposal, true);
            fleet.upsert_proposal(&resolved)?;
        }
        self.append_to_rules_toml(&stored.toml)?;
        tracing::info!(id, rule = %stored.id, "bake-back proposal applied");
        Ok(true)
    }

    /// Reject a proposal. A no-op for an already resolved proposal.
    ///
    /// # Errors
    /// Unknown proposal or a projection failure.
    pub async fn reject(&self, id: &str) -> Result<bool> {
        let proposal = {
            let fleet = self.fleet.lock().await;
            fleet.proposal(id).cloned().context("unknown proposal")?
        };
        if proposal.status != ProposalStatus::Pending {
            return Ok(false);
        }
        let mut fleet = self.fleet.lock().await;
        let resolved = resolve_proposal(&proposal, false);
        fleet.upsert_proposal(&resolved)?;
        tracing::info!(id, "bake-back proposal rejected");
        Ok(true)
    }

    /// Expire pending proposals older than the TTL window.
    ///
    /// # Errors
    /// Any projection failure.
    pub async fn expire_old(&self) -> Result<usize> {
        let cutoff = ttl_cutoff();
        let mut fleet = self.fleet.lock().await;
        let proposals: Vec<Proposal> = fleet.proposals().cloned().collect();
        let expired = expire(&proposals, &cutoff);
        let mut changed = 0;
        for proposal in expired {
            if proposal.status == ProposalStatus::Expired {
                fleet.upsert_proposal(&proposal)?;
                changed += 1;
            }
        }
        Ok(changed)
    }

    /// The pending proposals, for `bake-back --preview` output.
    ///
    /// # Errors
    /// Any projection failure.
    pub async fn pending(&self) -> Result<Vec<Proposal>> {
        let fleet = self.fleet.lock().await;
        Ok(fleet.proposals().filter(|p| p.status == ProposalStatus::Pending).cloned().collect())
    }

    /// Append a rule block to `rules.toml` (creating the file if absent).
    ///
    /// # Errors
    /// Any I/O failure.
    fn append_to_rules_toml(&self, block: &str) -> Result<()> {
        use std::io::Write as _;
        if let Some(parent) = self.rules_toml.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.rules_toml)
            .with_context(|| format!("opening {}", self.rules_toml.display()))?;
        file.write_all(block.as_bytes())
            .and_then(|()| file.write_all(b"\n"))
            .with_context(|| format!("appending to {}", self.rules_toml.display()))
    }
}

/// A `created_at` cutoff that marks proposals older than the TTL as expired.
/// RFC 3339 UTC strings are fixed-width, so lexicographic comparison equals
/// chronological comparison.
#[must_use]
fn ttl_cutoff() -> String {
    use chrono::{Duration, Utc};
    let days = i64::try_from(PROPOSAL_TTL_DAYS).unwrap_or(i64::MAX);
    (Utc::now() - Duration::days(days)).format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ttl_cutoff_is_lexicographically_comparable() {
        let cutoff = ttl_cutoff();
        assert_eq!(cutoff.len(), 24);
        assert!(cutoff.starts_with("20"));
        // Newer timestamps sort after the cutoff.
        assert!("2999-01-01T00:00:00.000Z" > cutoff.as_str());
        assert!("2020-01-01T00:00:00.000Z" < cutoff.as_str());
    }
}
