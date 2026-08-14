//! The cmux driver (C5-side, future per M9): drives harnesses with no API
//! (Claude Code, Pi, Codex) through their terminal panes (§4.7).
//!
//! `send` → `cmux send` (+ Enter), `read_last_output` → `cmux read-screen`,
//! `status` → a pane heuristic, `read_structured` → always `None` (no
//! structured output over a terminal). The workflow engine sees it through the
//! same [`AgentDriver`] trait; no core changes.

use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use supervisor_core::types::AgentState;

use crate::clients::cmux::{CmuxClient, CmuxHandle};
use crate::clients::driver::{AgentDriver, AgentRef, DriverKind, OutputFormat, SendReceipt};

/// Resolves an agent to its cmux surface handle.
pub type SurfaceResolver = Arc<dyn Fn(&str, &str) -> Option<CmuxHandle> + Send + Sync>;

/// The cmux driver.
pub struct CmuxDriver {
    cmux: Arc<dyn CmuxClient>,
    surface: SurfaceResolver,
}

impl CmuxDriver {
    /// Build a driver over a cmux client + surface resolver.
    #[must_use]
    pub fn new(cmux: Arc<dyn CmuxClient>, surface: SurfaceResolver) -> Self {
        Self { cmux, surface }
    }

    fn surface_for(&self, ws: &str, agent: &str) -> Result<CmuxHandle> {
        (self.surface)(ws, agent).with_context(|| format!("no cmux surface for {ws}/{agent}"))
    }
}

#[async_trait]
impl AgentDriver for CmuxDriver {
    fn kind(&self) -> DriverKind {
        DriverKind::Cmux
    }

    async fn send(
        &self,
        a: &AgentRef,
        msg: &str,
        format: Option<&OutputFormat>,
    ) -> Result<SendReceipt> {
        if format.is_some() {
            // Terminals cannot carry structured output; the layered ACK
            // resolver handles the text path instead.
            tracing::debug!(ws = %a.ws, agent = %a.agent, "structured output ignored for cmux driver");
        }
        let ws = a.ws.clone();
        let surface = self.surface_for(&a.ws, &a.agent)?;
        self.cmux.send_cmd(&ws, &surface, msg).await?;
        Ok(SendReceipt { session_id: a.session_id.clone(), structured_requested: false })
    }

    async fn read_last_output(&self, a: &AgentRef, limit: usize) -> Result<String> {
        let ws = a.ws.clone();
        let surface = self.surface_for(&a.ws, &a.agent)?;
        let screen = self.cmux.read_screen(&ws, &surface).await?;
        Ok(screen
            .lines()
            .rev()
            .take(limit)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n"))
    }

    async fn read_structured(&self, _a: &AgentRef) -> Result<Option<serde_json::Value>> {
        // No structured output over a terminal; the regex ACK fallback covers
        // the contract.
        Ok(None)
    }

    async fn status(&self, a: &AgentRef) -> Result<AgentState> {
        let ws = a.ws.clone();
        let surface = self.surface_for(&a.ws, &a.agent)?;
        let screen = self.cmux.read_screen(&ws, &surface).await?;
        Ok(if screen.trim().is_empty() { AgentState::Idle } else { AgentState::Working })
    }

    async fn abort(&self, a: &AgentRef) -> Result<()> {
        let ws = a.ws.clone();
        let surface = self.surface_for(&a.ws, &a.agent)?;
        self.cmux.send_key(&ws, &surface, "Ctrl-C").await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clients::cmux::CmuxWorkspace;
    use std::path::Path;

    struct FakeCmux {
        screen: std::sync::Mutex<String>,
        sent: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait]
    impl CmuxClient for FakeCmux {
        async fn ping(&self) -> Result<()> {
            Ok(())
        }
        async fn list_workspaces(&self) -> Result<Vec<CmuxWorkspace>> {
            Ok(Vec::new())
        }
        async fn new_workspace(&self, name: &str, _cwd: &Path) -> Result<CmuxHandle> {
            Ok(name.to_owned())
        }
        async fn new_surface(&self, _ws: &CmuxHandle, _wd: &Path) -> Result<CmuxHandle> {
            Ok("surface:1".to_owned())
        }
        async fn send_cmd(&self, _ws: &CmuxHandle, _s: &CmuxHandle, text: &str) -> Result<()> {
            self.sent.lock().unwrap().push(text.to_owned());
            Ok(())
        }
        async fn focus_pane(&self, _ws: &CmuxHandle, _p: &CmuxHandle) -> Result<()> {
            Ok(())
        }
        async fn select_workspace(&self, _ws: &CmuxHandle) -> Result<()> {
            Ok(())
        }
        async fn close_surface(&self, _ws: &CmuxHandle, _s: &CmuxHandle) -> Result<()> {
            Ok(())
        }
        async fn close_workspace(&self, _ws: &CmuxHandle) -> Result<()> {
            Ok(())
        }
        async fn read_screen(&self, _ws: &CmuxHandle, _s: &CmuxHandle) -> Result<String> {
            Ok(self.screen.lock().unwrap().clone())
        }
        async fn send(&self, _ws: &CmuxHandle, _s: &CmuxHandle, _text: &str) -> Result<()> {
            Ok(())
        }
        async fn send_key(&self, _ws: &CmuxHandle, _s: &CmuxHandle, key: &str) -> Result<()> {
            self.sent.lock().unwrap().push(format!("<{key}>"));
            Ok(())
        }
        async fn notify(&self, _ws: &CmuxHandle, _t: &str, _b: &str) -> Result<()> {
            Ok(())
        }
    }

    fn driver(fake: Arc<FakeCmux>) -> CmuxDriver {
        CmuxDriver::new(fake, Arc::new(|_ws: &str, agent: &str| Some(format!("surface:{agent}"))))
    }

    fn agent_ref() -> AgentRef {
        AgentRef {
            ws: "iot".to_owned(),
            agent: "codex_01".to_owned(),
            session_id: "pane".to_owned(),
        }
    }

    #[tokio::test]
    async fn send_types_into_the_pane() {
        let fake = Arc::new(FakeCmux {
            screen: std::sync::Mutex::new(String::new()),
            sent: std::sync::Mutex::new(Vec::new()),
        });
        let driver = driver(fake.clone());
        let receipt = driver.send(&agent_ref(), "do the thing", None).await.unwrap();
        assert!(!receipt.structured_requested, "cmux never requests structured output");
        assert_eq!(fake.sent.lock().unwrap()[0], "do the thing");
    }

    #[tokio::test]
    async fn read_limits_lines() {
        let fake = Arc::new(FakeCmux {
            screen: std::sync::Mutex::new("l1\nl2\nl3\nl4\n".to_owned()),
            sent: std::sync::Mutex::new(Vec::new()),
        });
        let driver = driver(fake.clone());
        assert_eq!(driver.read_last_output(&agent_ref(), 2).await.unwrap(), "l3\nl4");
    }

    #[tokio::test]
    async fn structured_is_never_available() {
        let fake = Arc::new(FakeCmux {
            screen: std::sync::Mutex::new(String::new()),
            sent: std::sync::Mutex::new(Vec::new()),
        });
        let driver = driver(fake.clone());
        assert_eq!(driver.read_structured(&agent_ref()).await.unwrap(), None);
    }

    #[tokio::test]
    async fn status_is_a_pane_heuristic() {
        let fake = Arc::new(FakeCmux {
            screen: std::sync::Mutex::new("thinking...\n".to_owned()),
            sent: std::sync::Mutex::new(Vec::new()),
        });
        let driver = driver(fake.clone());
        assert_eq!(driver.status(&agent_ref()).await.unwrap(), AgentState::Working);
        *fake.screen.lock().unwrap() = String::new();
        assert_eq!(driver.status(&agent_ref()).await.unwrap(), AgentState::Idle);
    }

    #[tokio::test]
    async fn abort_sends_ctrl_c() {
        let fake = Arc::new(FakeCmux {
            screen: std::sync::Mutex::new(String::new()),
            sent: std::sync::Mutex::new(Vec::new()),
        });
        let driver = driver(fake.clone());
        driver.abort(&agent_ref()).await.unwrap();
        assert!(fake.sent.lock().unwrap().contains(&"<Ctrl-C>".to_owned()));
    }
}
