use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Mutex as StdMutex;
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
use async_trait::async_trait;
use tokio::process::{Child, Command as TokioCommand};
use tokio::sync::Mutex;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::agent::{AgentDelta, AgentError, AgentSession, format_tool_input};

pub(crate) struct AcpSession {
    session_id: String,
    session: Mutex<ActiveSession<'static, agent_client_protocol::Agent>>,
    pending_tool_title: StdMutex<Option<String>>,
}

impl AcpSession {
    fn new(session: ActiveSession<'static, agent_client_protocol::Agent>) -> Self {
        let sid = session.session_id().clone().to_string();
        AcpSession {
            session_id: sid,
            session: Mutex::new(session),
            pending_tool_title: StdMutex::new(None),
        }
    }
}

#[async_trait]
impl AgentSession for AcpSession {
    fn session_id(&self) -> &str {
        &self.session_id
    }

    async fn send_input(&self, text: &str) -> Result<(), AgentError> {
        self.session
            .lock()
            .await
            .send_prompt(text)
            .map_err(|e| AgentError::Acp(e.to_string()))
    }

    async fn query_delta(&self) -> Result<Option<AgentDelta>, AgentError> {
        let mut session = self.session.lock().await;
        match tokio::time::timeout(Duration::from_secs(1), session.read_update()).await {
            Ok(Ok(SessionMessage::SessionMessage(dispatch))) => {
                let mut delta = None;
                let _ = MatchDispatch::new(dispatch)
                    .if_notification(async |notif: SessionNotification| {
                        match &notif.update {
                            SessionUpdate::ToolCall(tc) => {
                                tracing::info!(
                                    "ToolCall start: {} | input: {:?}",
                                    tc.title,
                                    format_tool_input(&tc.raw_input),
                                );
                                *self.pending_tool_title.lock().unwrap() = Some(tc.title.clone());
                            }
                            SessionUpdate::ToolCallUpdate(tc) => {
                                let title = tc.fields.title.clone().unwrap_or_default();
                                let has_input = tc
                                    .fields
                                    .raw_input
                                    .as_ref()
                                    .and_then(|v| v.as_object())
                                    .is_some_and(|o| !o.is_empty());
                                if has_input {
                                    let input = format_tool_input(&tc.fields.raw_input);
                                    let tool_name = self
                                        .pending_tool_title
                                        .lock()
                                        .unwrap()
                                        .take()
                                        .unwrap_or(title);
                                    tracing::info!("ToolCall: {tool_name} | input: {input:?}");
                                    delta = Some(AgentDelta::ToolCall {
                                        title: tool_name,
                                        input,
                                    });
                                } else {
                                    self.pending_tool_title.lock().unwrap().take();
                                }
                            }
                            SessionUpdate::AgentMessageChunk(chunk) => {
                                if let ContentBlock::Text(text) = &chunk.content {
                                    delta = Some(AgentDelta::Text {
                                        output: text.text.clone(),
                                        done: false,
                                    });
                                }
                            }
                            _ => {}
                        }
                        Ok(())
                    })
                    .await
                    .otherwise_ignore();

                Ok(delta)
            }
            Ok(Ok(SessionMessage::StopReason(_))) => Ok(Some(AgentDelta::Text {
                output: String::new(),
                done: true,
            })),
            Ok(Ok(_)) => Ok(None),
            Ok(Err(e)) => {
                tracing::warn!("ACP read_update error: {e}");
                Ok(None)
            }
            Err(_) => Ok(None),
        }
    }
}

pub struct AcpBackendBuilder {
    command: Option<String>,
    args: Vec<String>,
    agent_name: Option<String>,
}

impl AcpBackendBuilder {
    pub fn new() -> Self {
        AcpBackendBuilder {
            command: None,
            args: Vec::new(),
            agent_name: None,
        }
    }

    pub fn command(mut self, cmd: impl Into<String>) -> Self {
        self.command = Some(cmd.into());
        self
    }

    pub fn agent_name(mut self, name: impl Into<String>) -> Self {
        self.agent_name = Some(name.into());
        self
    }

    pub async fn build(self) -> Result<AcpBackend, AgentError> {
        let command = self.command.expect("command is required");
        let args = self.args;
        let agent_name = self.agent_name.expect("agent_name is required");

        let mut cmd = TokioCommand::new(&command);
        cmd.args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());

        let mut child = cmd.spawn()?;
        let child_stdin = child
            .stdin
            .take()
            .ok_or_else(|| AgentError::Acp(format!("Failed to open {agent_name} stdin")))?;
        let child_stdout = child
            .stdout
            .take()
            .ok_or_else(|| AgentError::Acp(format!("Failed to open {agent_name} stdout")))?;

        let transport = ByteStreams::new(child_stdin.compat_write(), child_stdout.compat());

        let name_for_init = agent_name.clone();

        let (init_tx, init_rx) = tokio::sync::oneshot::channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        let name_for_error = agent_name.clone();
        tokio::spawn(async move {
            let result = Client
                .builder()
                .on_receive_request(
                    move |request: RequestPermissionRequest,
                          responder: agent_client_protocol::Responder<RequestPermissionResponse>,
                          _conn: ConnectionTo<agent_client_protocol::Agent>| {
                        async move {
                            tracing::info!(
                                "Auto-approving toolcall permission: {} | {:?}",
                                request.tool_call.tool_call_id,
                                request.tool_call.fields.title,
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
                .connect_with(
                    transport,
                    move |connection: ConnectionTo<agent_client_protocol::Agent>| async move {
                        let init_response = connection
                            .send_request(InitializeRequest::new(ProtocolVersion::V1))
                            .block_task()
                            .await?;

                        tracing::info!(
                            "{} ACP capabilities: load_session={}, session_capabilities={:?}, agent_info={:?}",
                            name_for_init,
                            init_response.agent_capabilities.load_session,
                            init_response.agent_capabilities.session_capabilities,
                            init_response.agent_info,
                        );

                        let _ = init_tx.send(connection);
                        let _ = shutdown_rx.await;
                        Ok(())
                    },
                )
                .await;

            if let Err(e) = result {
                tracing::error!("{} ACP connection error: {e}", name_for_error);
            }
        });

        let conn = init_rx
            .await
            .map_err(|_| AgentError::Acp(format!("{agent_name} ACP initialization failed")))?;

        Ok(AcpBackend {
            child,
            conn,
            shutdown_tx: Some(shutdown_tx),
        })
    }
}

pub(crate) struct AcpBackend {
    child: Child,
    conn: ConnectionTo<agent_client_protocol::Agent>,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl AcpBackend {
    pub fn builder() -> AcpBackendBuilder {
        AcpBackendBuilder::new()
    }

    pub(crate) async fn create_session(&mut self, pwd: PathBuf) -> Result<AcpSession, AgentError> {
        let active_session = self
            .conn
            .build_session(&pwd)
            .block_task()
            .start_session()
            .await
            .map_err(|e| AgentError::Acp(e.to_string()))?;

        Ok(AcpSession::new(active_session))
    }

    pub(crate) async fn resume_session(
        &mut self,
        session_id: String,
        pwd: PathBuf,
    ) -> Result<AcpSession, AgentError> {
        let request = ResumeSessionRequest::new(session_id.clone(), pwd);
        let loaded = self
            .conn
            .send_request(request)
            .block_task()
            .await
            .map_err(|e| AgentError::Acp(e.to_string()))?;

        let mut resp = NewSessionResponse::new(session_id.clone());
        resp.modes = loaded.modes;
        resp.meta = loaded.meta;

        let active_session = self
            .conn
            .attach_session(resp, vec![])
            .map_err(|e| AgentError::Acp(e.to_string()))?;

        Ok(AcpSession::new(active_session))
    }

    pub(crate) async fn shutdown(&mut self) -> Result<(), AgentError> {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        self.child.wait().await?;
        Ok(())
    }
}
