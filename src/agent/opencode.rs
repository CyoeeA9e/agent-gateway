use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{Mutex, OnceCell};

use crate::agent::AgentError;
use crate::agent::acp::{AcpBackend, AcpSession};

pub(crate) struct OpenCodeAcp {
    backend: Arc<Mutex<AcpBackend>>,
}

impl Clone for OpenCodeAcp {
    fn clone(&self) -> Self {
        OpenCodeAcp {
            backend: self.backend.clone(),
        }
    }
}

impl OpenCodeAcp {
    async fn new() -> Result<Self, AgentError> {
        let backend = AcpBackend::builder()
            .command("opencode")
            .args(vec!["acp".to_string()])
            .agent_name("opencode")
            .build()
            .await?;
        Ok(OpenCodeAcp {
            backend: Arc::new(Mutex::new(backend)),
        })
    }
}

static OPENCODE: OnceCell<OpenCodeAcp> = OnceCell::const_new();

async fn get_opencode() -> Result<&'static OpenCodeAcp, AgentError> {
    OPENCODE
        .get_or_try_init(|| async { OpenCodeAcp::new().await })
        .await
}

pub async fn create_session(pwd: PathBuf) -> Result<AcpSession, AgentError> {
    let opencode = get_opencode().await?;
    let mut backend = opencode.backend.lock().await;
    backend.create_session(pwd).await
}

pub async fn resume_session(session_id: String, pwd: PathBuf) -> Result<AcpSession, AgentError> {
    let opencode = get_opencode().await?;
    let mut backend = opencode.backend.lock().await;
    backend.resume_session(session_id, pwd).await
}
