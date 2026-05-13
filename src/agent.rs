pub mod cc;
pub mod opencode;

use async_trait::async_trait;
use cc::ClaudeCode;
use opencode::OpenCodeAgent;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug)]
pub enum AgentError {
    Io(std::io::Error),
    Acp(String),
}

impl std::fmt::Display for AgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentError::Io(e) => write!(f, "IO error: {e}"),
            AgentError::Acp(e) => write!(f, "ACP error: {e}"),
        }
    }
}

impl std::error::Error for AgentError {}

impl From<std::io::Error> for AgentError {
    fn from(e: std::io::Error) -> Self {
        AgentError::Io(e)
    }
}

impl From<agent_client_protocol::Error> for AgentError {
    fn from(e: agent_client_protocol::Error) -> Self {
        AgentError::Acp(e.to_string())
    }
}

#[derive(Debug)]
pub enum AgentDelta {
    Text { output: String, done: bool },
    ToolCall { title: String, input: Option<String> },
}

pub fn format_tool_input(raw: &Option<serde_json::Value>) -> Option<String> {
    match raw {
        Some(serde_json::Value::Object(map)) => {
            let pairs: Vec<String> = map
                .iter()
                .map(|(k, v)| match v {
                    serde_json::Value::String(s) => format!("{k}={s}"),
                    _ => format!("{k}={v}"),
                })
                .collect();
            Some(pairs.join(", "))
        }
        Some(val) => Some(val.to_string()),
        None => None,
    }
}

#[async_trait]
pub trait AgentSession: Send {
    fn send_input(&mut self, text: &str) -> Result<(), AgentError>;
    async fn query_delta(&mut self) -> Result<Option<AgentDelta>, AgentError>;
    fn id(&self) -> String;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum AgentType {
    #[default]
    None,
    ClaudeCodeAcp,
    OpenCode,
}

pub struct AgentRegistry {
    cc: Option<ClaudeCode>,
    opencode: Option<OpenCodeAgent>,
}

impl AgentRegistry {
    pub fn new() -> Self {
        AgentRegistry {
            cc: None,
            opencode: None,
        }
    }

    pub async fn create_session(
        &mut self,
        pwd: PathBuf,
        agent_type: &AgentType,
        session_id: Option<String>,
    ) -> Result<(String, Box<dyn AgentSession>), AgentError> {
        match agent_type {
            AgentType::None => unreachable!("None is handled by get_or_create_session"),
            AgentType::ClaudeCodeAcp => {
                let cc = self.cc.get_or_insert_with(|| ClaudeCode::new());
                cc.ensure_started().await?;
                if let Some(sid) = session_id {
                    match cc.resume_session(sid, pwd.clone()).await {
                        Ok(result) => return Ok(result),
                        Err(e) => tracing::warn!("Failed to resume session, creating new: {e}"),
                    }
                }
                cc.create_session(pwd).await
            }
            AgentType::OpenCode => {
                let oc = self.opencode.get_or_insert_with(|| OpenCodeAgent::new());
                oc.ensure_started().await?;
                if let Some(sid) = session_id {
                    match oc.resume_session(sid, pwd.clone()).await {
                        Ok(result) => return Ok(result),
                        Err(e) => tracing::warn!("Failed to resume session, creating new: {e}"),
                    }
                }
                oc.create_session(pwd).await
            }
        }
    }

    pub async fn shutdown(&mut self) -> Result<(), AgentError> {
        if let Some(cc) = &mut self.cc {
            cc.shutdown().await?;
        }
        if let Some(oc) = &mut self.opencode {
            oc.shutdown().await?;
        }
        Ok(())
    }
}
