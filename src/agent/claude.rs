use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{Mutex, OnceCell};

use crate::agent::AgentError;
use crate::agent::acp::{AcpBackend, AcpSession};

pub(crate) struct ClaudeCodeAcp {
    backend: Arc<Mutex<AcpBackend>>,
}

impl Clone for ClaudeCodeAcp {
    fn clone(&self) -> Self {
        ClaudeCodeAcp {
            backend: self.backend.clone(),
        }
    }
}

impl ClaudeCodeAcp {
    async fn new() -> Result<Self, AgentError> {
        let backend = AcpBackend::builder()
            .command("claude-agent-acp")
            .agent_name("Claude Code")
            .build()
            .await?;
        Ok(ClaudeCodeAcp {
            backend: Arc::new(Mutex::new(backend)),
        })
    }

}

static CLAUDE: OnceCell<ClaudeCodeAcp> = OnceCell::const_new();

async fn get_claude() -> Result<&'static ClaudeCodeAcp, AgentError> {
    CLAUDE
        .get_or_try_init(|| async { ClaudeCodeAcp::new().await })
        .await
}

pub async fn create_session(pwd: PathBuf) -> Result<AcpSession, AgentError> {
    let claude = get_claude().await?;
    let mut backend = claude.backend.lock().await;
    backend.create_session(pwd).await
}

pub async fn resume_session(session_id: String, pwd: PathBuf) -> Result<AcpSession, AgentError> {
    let claude = get_claude().await?;
    let mut backend = claude.backend.lock().await;
    backend.resume_session(session_id, pwd).await
}
