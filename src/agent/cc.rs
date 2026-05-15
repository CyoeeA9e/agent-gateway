use std::path::PathBuf;

use super::acp::AcpBackend;
use super::{AgentError, AgentSession};

pub struct ClaudeCode(AcpBackend);

impl ClaudeCode {
    pub fn new() -> Self {
        ClaudeCode(AcpBackend::new())
    }

    pub async fn ensure_started(&mut self) -> Result<(), AgentError> {
        self.0
            .ensure_started("claude-agent-acp", &[], "Claude Code")
            .await
    }

    pub async fn create_session(
        &mut self,
        pwd: PathBuf,
    ) -> Result<(String, Box<dyn AgentSession>), AgentError> {
        self.0.create_session(pwd).await
    }

    pub async fn resume_session(
        &mut self,
        session_id: String,
        pwd: PathBuf,
    ) -> Result<(String, Box<dyn AgentSession>), AgentError> {
        self.0.resume_session(session_id, pwd).await
    }

    pub async fn shutdown(&mut self) -> Result<(), AgentError> {
        self.0.shutdown().await
    }
}
