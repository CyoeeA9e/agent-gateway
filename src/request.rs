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
        let reply = format!("Echo: {content}");
        req.resp(&reply).await;
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
