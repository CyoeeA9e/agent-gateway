use std::time::Duration;

use async_trait::async_trait;

use crate::agent::{AgentDelta, AgentSession, format_tool_call_display};

pub enum BotStatus {
    Composing,
    Active,
}

#[async_trait]
pub trait Request: Send + Sync + 'static {
    fn get_content(&self) -> &str;
    fn conversation(&self) -> &str;
    async fn resp(&self, text: &str);
    async fn set_status(&mut self, status: BotStatus);
}

pub async fn handle_request<T: Request, S: AgentSession>(mut req: T, session: Option<S>) {
    let content = req.get_content().to_string();

    req.set_status(BotStatus::Composing).await;

    let Some(session) = session else {
        req.resp("No agent configured. Use /bot new <agent> to set one.").await;
        req.set_status(BotStatus::Active).await;
        return;
    };

    if let Err(e) = session.send_input(&content).await {
        req.resp(&format!("Error: {e}")).await;
        req.set_status(BotStatus::Active).await;
        return;
    }

    let mut full_output = String::new();
    let result = loop {
        req.set_status(BotStatus::Composing).await;
        match session.query_delta().await {
            Ok(Some(AgentDelta::Text { output, done })) => {
                if !output.is_empty() {
                    full_output.push_str(&output);
                }
                if done {
                    break Ok(if full_output.is_empty() {
                        "Task completed".to_owned()
                    } else {
                        full_output.clone()
                    });
                }
            }
            Ok(Some(AgentDelta::ToolCall { title, input })) => {
                if !full_output.is_empty() {
                    req.resp(&full_output).await;
                    full_output.clear();
                }
                let msg = format_tool_call_display(&title, &input);
                req.resp(&msg).await;
            }
            Ok(None) => {}
            Err(e) => {
                break Err(format!("Error: {e}"));
            }
        }
    };
    match result {
        Ok(msg) => req.resp(&msg).await,
        Err(e) => req.resp(&e).await,
    }
    req.set_status(BotStatus::Active).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AgentDelta, AgentError, AgentSession, format_tool_call_display};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex as StdMutex};

    struct MockSession {
        deltas: StdMutex<VecDeque<AgentDelta>>,
    }

    impl MockSession {
        fn new(deltas: Vec<AgentDelta>) -> Self {
            MockSession {
                deltas: StdMutex::new(deltas.into()),
            }
        }
    }

    #[async_trait]
    impl AgentSession for MockSession {
        fn session_id(&self) -> &str {
            "mock-session"
        }
        async fn send_input(&self, _text: &str) -> Result<(), AgentError> {
            Ok(())
        }
        async fn query_delta(&self) -> Result<Option<AgentDelta>, AgentError> {
            Ok(self.deltas.lock().unwrap().pop_front())
        }
    }

    struct MockRequest {
        content: String,
        responses: Arc<StdMutex<Vec<String>>>,
    }

    impl MockRequest {
        fn new(content: &str) -> (Self, Arc<StdMutex<Vec<String>>>) {
            let responses = Arc::new(StdMutex::new(Vec::new()));
            let req = MockRequest {
                content: content.to_string(),
                responses: responses.clone(),
            };
            (req, responses)
        }
    }

    #[async_trait]
    impl Request for MockRequest {
        fn get_content(&self) -> &str {
            &self.content
        }
        fn conversation(&self) -> &str {
            "mock-conv"
        }
        async fn resp(&self, text: &str) {
            self.responses.lock().unwrap().push(text.to_string());
        }
        async fn set_status(&mut self, _status: BotStatus) {}
    }

    #[tokio::test]
    async fn test_claude_two_step_toolcall() {
        // Claude: ToolCall(title, input) → Text chunks → Text(done)
        // handle_request sends toolcall via resp immediately, then final text at the end
        let (req, resp) = MockRequest::new("list files");
        let session = MockSession::new(vec![
            AgentDelta::ToolCall {
                title: "Terminal".into(),
                input: Some("command=ls -la".into()),
            },
            AgentDelta::Text {
                output: "file1.txt\nfile2.txt".into(),
                done: false,
            },
            AgentDelta::Text {
                output: String::new(),
                done: true,
            },
        ]);
        handle_request(req, Some(session)).await;
        let msgs = resp.lock().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0], r#"Terminal(command="ls -la")"#);
        assert_eq!(msgs[1], "file1.txt\nfile2.txt");
    }

    #[tokio::test]
    async fn test_opencode_toolcall_with_raw_input() {
        // OpenCode: ToolCall with raw_input already present
        let (req, resp) = MockRequest::new("read cargo");
        let session = MockSession::new(vec![
            AgentDelta::ToolCall {
                title: "Read File".into(),
                input: Some("file_path=/workspace/Cargo.toml".into()),
            },
            AgentDelta::Text {
                output: "[package]\nname = \"agentbot\"".into(),
                done: false,
            },
            AgentDelta::Text {
                output: String::new(),
                done: true,
            },
        ]);
        handle_request(req, Some(session)).await;
        let msgs = resp.lock().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0], r#"Read File(file_path="/workspace/Cargo.toml")"#);
        assert_eq!(msgs[1], "[package]\nname = \"agentbot\"");
    }

    #[tokio::test]
    async fn test_opencode_toolcall_title_only() {
        // OpenCode: ToolCall with title only, no input
        let (req, resp) = MockRequest::new("think");
        let session = MockSession::new(vec![
            AgentDelta::ToolCall {
                title: "Thinking".into(),
                input: None,
            },
            AgentDelta::Text {
                output: "Done thinking".into(),
                done: true,
            },
        ]);
        handle_request(req, Some(session)).await;
        let msgs = resp.lock().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0], "Thinking");
        assert_eq!(msgs[1], "Done thinking");
    }

    #[tokio::test]
    async fn test_toolcall_flushes_pending_text() {
        // Text accumulated before toolcall should be flushed
        let (req, resp) = MockRequest::new("do stuff");
        let session = MockSession::new(vec![
            AgentDelta::Text {
                output: "Let me ".into(),
                done: false,
            },
            AgentDelta::Text {
                output: "check that.".into(),
                done: false,
            },
            AgentDelta::ToolCall {
                title: "Terminal".into(),
                input: Some("command=ls".into()),
            },
            AgentDelta::Text {
                output: "All done".into(),
                done: true,
            },
        ]);
        handle_request(req, Some(session)).await;
        let msgs = resp.lock().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0], "Let me check that.");
        assert_eq!(msgs[1], r#"Terminal(command="ls")"#);
        assert_eq!(msgs[2], "All done");
    }

    #[tokio::test]
    async fn test_no_session_shows_error() {
        let (req, resp) = MockRequest::new("hello bot");
        handle_request::<MockRequest, MockSession>(req, None).await;
        let msgs = resp.lock().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0], "No agent configured. Use /bot new <agent> to set one.");
    }

    #[tokio::test]
    async fn test_multiple_toolcalls() {
        // Multiple toolcalls in sequence
        let (req, resp) = MockRequest::new("do it");
        let session = MockSession::new(vec![
            AgentDelta::ToolCall {
                title: "Read File".into(),
                input: Some("file_path=a.txt".into()),
            },
            AgentDelta::ToolCall {
                title: "Terminal".into(),
                input: Some("command=cat a.txt".into()),
            },
            AgentDelta::Text {
                output: "ok".into(),
                done: true,
            },
        ]);
        handle_request(req, Some(session)).await;
        let msgs = resp.lock().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0], r#"Read File(file_path="a.txt")"#);
        assert_eq!(msgs[1], r#"Terminal(command="cat a.txt")"#);
        assert_eq!(msgs[2], "ok");
    }

    #[test]
    fn test_format_tool_input_object() {
        let val = serde_json::json!({"command": "ls -la", "description": "list files"});
        let result = crate::agent::format_tool_input(&Some(val));
        assert!(result.is_some());
        let s = result.unwrap();
        assert!(s.contains("command=ls -la"));
        assert!(s.contains("description=list files"));
    }

    #[test]
    fn test_format_tool_input_none() {
        assert!(crate::agent::format_tool_input(&None).is_none());
    }

    #[test]
    fn test_format_tool_input_string() {
        let val = serde_json::json!("just a string");
        let result = crate::agent::format_tool_input(&Some(val));
        // serde_json::Value::String.to_string() includes JSON quotes
        assert_eq!(result.unwrap(), "\"just a string\"");
    }

    #[test]
    fn test_format_tool_call_display_with_input() {
        let result = format_tool_call_display(
            "Terminal",
            &Some(r#"command=ls -la, cwd=/home"#.to_string()),
        );
        assert_eq!(result, r#"Terminal(command="ls -la", cwd="/home")"#);
    }

    #[test]
    fn test_format_tool_call_display_no_input() {
        let result = format_tool_call_display("Thinking", &None);
        assert_eq!(result, "Thinking");
    }

    #[test]
    fn test_format_tool_call_display_empty_input() {
        let result = format_tool_call_display("Thinking", &Some(String::new()));
        assert_eq!(result, "Thinking");
    }
}
