pub mod acp;
pub mod claude;

use std::sync::Arc;

use async_trait::async_trait;
use serde_json;

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
    Text {
        output: String,
        done: bool,
    },
    ToolCall {
        title: String,
        input: Option<String>,
    },
}

#[async_trait]
pub trait AgentSession: Send + Sync {
    fn session_id(&self) -> &str;
    async fn send_input(&self, text: &str) -> Result<(), AgentError>;
    async fn query_delta(&self) -> Result<Option<AgentDelta>, AgentError>;
}

#[async_trait]
impl AgentSession for Arc<dyn AgentSession + '_> {
    fn session_id(&self) -> &str {
        (**self).session_id()
    }

    async fn send_input(&self, text: &str) -> Result<(), AgentError> {
        (**self).send_input(text).await
    }

    async fn query_delta(&self) -> Result<Option<AgentDelta>, AgentError> {
        (**self).query_delta().await
    }
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

pub fn format_tool_call_display(title: &str, input: &Option<String>) -> String {
    match input {
        Some(s) if !s.is_empty() => {
            let pairs: Vec<String> = s
                .split(", ")
                .map(|pair| {
                    if let Some((k, v)) = pair.split_once('=') {
                        format!("{k}=\"{v}\"")
                    } else {
                        pair.to_string()
                    }
                })
                .collect();
            format!("{title}({})", pairs.join(", "))
        }
        _ => title.to_string(),
    }
}
