use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use agent_client_protocol::{
    ActiveSession, ByteStreams, Client, ConnectionTo, SessionMessage,
    schema::{
        ContentBlock, InitializeRequest, NewSessionResponse, ProtocolVersion,
        RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
        ResumeSessionRequest, SelectedPermissionOutcome, SessionNotification, SessionUpdate,
    },
    util::MatchDispatch,
};
use tokio::process::{Child, Command as TokioCommand};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use super::{AgentSession, AgentDelta, AgentError};
use async_trait::async_trait;

pub struct ClaudeCodeSession {
    session_id: String,
    session: ActiveSession<'static, agent_client_protocol::Agent>,
}

impl ClaudeCodeSession {
    pub fn new(
        session_id: String,
        session: ActiveSession<'static, agent_client_protocol::Agent>,
    ) -> Self {
        ClaudeCodeSession { session_id, session }
    }
}

#[async_trait]
impl AgentSession for ClaudeCodeSession {
    fn id(&self) -> String {
        self.session_id.clone()
    }

    fn send_input(&mut self, text: &str) -> Result<(), AgentError> {
        self.session
            .send_prompt(text)
            .map_err(|e| AgentError::Acp(e.to_string()))
    }

    async fn query_delta(&mut self) -> Result<Option<AgentDelta>, AgentError> {
        match tokio::time::timeout(Duration::from_secs(1), self.session.read_update()).await {
            Ok(Ok(SessionMessage::SessionMessage(dispatch))) => {
                let mut delta = None;
                let _ = MatchDispatch::new(dispatch)
                    .if_notification(async |notif: SessionNotification| {
                        if let SessionUpdate::AgentMessageChunk(chunk) = &notif.update {
                            if let ContentBlock::Text(text) = &chunk.content {
                                delta = Some(AgentDelta::Text {
                                    output: text.text.clone(),
                                    done: false,
                                });
                            }
                        }
                        Ok(())
                    })
                    .await
                    .otherwise_ignore();
                Ok(delta)
            }
            Ok(Ok(SessionMessage::StopReason(_))) => {
                Ok(Some(AgentDelta::Text {
                    output: String::new(),
                    done: true,
                }))
            }
            Ok(Ok(_)) => Ok(None),
            Ok(Err(_)) | Err(_) => Ok(None),
        }
    }
}

pub struct ClaudeCode {
    child: Option<Child>,
    conn: Option<ConnectionTo<agent_client_protocol::Agent>>,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl ClaudeCode {
    pub fn new() -> Self {
        ClaudeCode {
            child: None,
            conn: None,
            shutdown_tx: None,
        }
    }

    pub fn is_running(&self) -> bool {
        self.child.is_some()
    }

    pub async fn ensure_started(&mut self) -> Result<(), AgentError> {
        if self.child.is_some() {
            return Ok(());
        }
        self.start().await
    }

    pub async fn create_session(&mut self, pwd: PathBuf) -> Result<(String, Box<dyn AgentSession>), AgentError> {
        let conn = self.conn.as_ref().ok_or_else(|| AgentError::Acp("not connected".into()))?;

        let active_session = conn
            .build_session(&pwd)
            .block_task()
            .start_session()
            .await
            .map_err(|e| AgentError::Acp(e.to_string()))?;

        let sid = active_session.session_id().clone().to_string();
        let session = ClaudeCodeSession::new(sid.clone(), active_session);
        Ok((sid, Box::new(session)))
    }

    pub async fn resume_session(&mut self, session_id: String, pwd: PathBuf) -> Result<(String, Box<dyn AgentSession>), AgentError> {
        let conn = self.conn.as_ref().ok_or_else(|| AgentError::Acp("not connected".into()))?;

        let request = ResumeSessionRequest::new(session_id.clone(), pwd);
        let loaded = conn
            .send_request(request)
            .block_task()
            .await
            .map_err(|e| AgentError::Acp(e.to_string()))?;

        let mut resp = NewSessionResponse::new(session_id);
        resp.modes = loaded.modes;
        resp.meta = loaded.meta;

        let active_session = conn
            .attach_session(resp, vec![])
            .map_err(|e| AgentError::Acp(e.to_string()))?;

        let sid = active_session.session_id().clone().to_string();
        let session = ClaudeCodeSession::new(sid.clone(), active_session);
        Ok((sid, Box::new(session)))
    }

    async fn finish(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.wait().await;
        }
    }

    pub async fn start(&mut self) -> Result<(), AgentError> {
        let mut cmd = TokioCommand::new("claude-agent-acp");
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());

        let mut child = cmd.spawn()?;
        let child_stdin = child.stdin.take().ok_or_else(|| {
            AgentError::Acp("Failed to open agent stdin".into())
        })?;
        let child_stdout = child.stdout.take().ok_or_else(|| {
            AgentError::Acp("Failed to open agent stdout".into())
        })?;

        let transport = ByteStreams::new(child_stdin.compat_write(), child_stdout.compat());

        let (init_tx, init_rx) = tokio::sync::oneshot::channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        tokio::spawn(async move {
            let result: Result<(), agent_client_protocol::Error> = Client
                .builder()
                .on_receive_request(
                    move |request: RequestPermissionRequest,
                          responder:
                              agent_client_protocol::Responder<
                                  RequestPermissionResponse,
                              >,
                          _connection:
                              ConnectionTo<agent_client_protocol::Agent>| {
                        async move {
                            tracing::debug!(
                                "Auto-approving permission: {}",
                                request.tool_call.tool_call_id
                            );
                            let option_id =
                                request.options.first().map(|opt| opt.option_id.clone());
                            if let Some(id) = option_id {
                                responder.respond(RequestPermissionResponse::new(
                                    RequestPermissionOutcome::Selected(
                                        SelectedPermissionOutcome::new(id),
                                    ),
                                ))
                            } else {
                                responder.respond(RequestPermissionResponse::new(
                                    RequestPermissionOutcome::Cancelled,
                                ))
                            }
                        }
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .connect_with(transport, move |connection: ConnectionTo<agent_client_protocol::Agent>| async move {
                    let init_response = connection
                        .send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;

                    tracing::info!(
                        "Claude Code ACP capabilities: load_session={}, session_capabilities={:?}, agent_info={:?}",
                        init_response.agent_capabilities.load_session,
                        init_response.agent_capabilities.session_capabilities,
                        init_response.agent_info,
                    );

                    let _ = init_tx.send(connection);
                    // Keep connection alive until shutdown
                    let _ = shutdown_rx.await;
                    Ok(())
                })
                .await;

            if let Err(e) = result {
                tracing::error!("ACP connection error: {e}");
            }
        });

        self.child = Some(child);
        self.shutdown_tx = Some(shutdown_tx);

        self.conn = Some(init_rx.await
            .map_err(|_| AgentError::Acp("ACP initialization failed".into()))?);
        Ok(())
    }

    pub async fn shutdown(&mut self) -> Result<(), AgentError> {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        drop(self.conn.take());
        self.finish().await;
        Ok(())
    }
}
