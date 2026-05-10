pub mod cc;

use std::path::PathBuf;
use cc::ClaudeCode;

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
    ToolCall { title: String },
}

pub trait AgentSession: Send {
    fn send_input(&mut self, text: &str) -> Result<(), AgentError>;
    fn query_delta(&mut self) -> Result<Vec<AgentDelta>, AgentError>;
    fn id(&self) -> String;
}

pub struct AgentRegistry {
    cc: ClaudeCode,
}

impl AgentRegistry {
    pub fn new(data_dir: PathBuf) -> Self {
        AgentRegistry {
            cc: ClaudeCode::new(data_dir),
        }
    }

    pub async fn start(&mut self) -> Result<(), AgentError> {
        self.cc.start().await
    }

    pub async fn create_session(&mut self, pwd: PathBuf) -> Result<(String, Box<dyn AgentSession>), AgentError> {
        self.cc.create_session(pwd).await
    }

    pub async fn shutdown(&mut self) -> Result<(), AgentError> {
        self.cc.shutdown().await
    }
}
