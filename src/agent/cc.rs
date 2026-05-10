use std::path::PathBuf;
use std::process::Stdio;

use agent_client_protocol::{
    ActiveSession, ByteStreams, Client, ConnectionTo,
    schema::{
        ContentBlock, InitializeRequest, ProtocolVersion, RequestPermissionOutcome,
        RequestPermissionRequest, RequestPermissionResponse, SelectedPermissionOutcome,
        SessionNotification, SessionUpdate,
    },
    SessionMessage,
};
use tokio::process::{Child, Command as TokioCommand};
use tokio::sync::mpsc;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use super::{AgentSession, AgentDelta, AgentError};

enum SessionCommand {
    SendPrompt(String),
}

enum AcpCommand {
    CreateSession {
        pwd: PathBuf,
        response_tx: tokio::sync::oneshot::Sender<Result<(String, mpsc::UnboundedSender<SessionCommand>), String>>,
        delta_tx: mpsc::UnboundedSender<AgentDelta>,
    },
    Shutdown,
}

pub struct ClaudeCodeSession {
    session_id: String,
    cmd_tx: mpsc::UnboundedSender<SessionCommand>,
    delta_rx: mpsc::UnboundedReceiver<AgentDelta>,
}

impl AgentSession for ClaudeCodeSession {
    fn id(&self) -> String {
        self.session_id.clone()
    }

    fn send_input(&mut self, text: &str) -> Result<(), AgentError> {
        self.cmd_tx
            .send(SessionCommand::SendPrompt(text.to_owned()))
            .map_err(|_| AgentError::Acp("session channel closed".into()))
    }

    fn query_delta(&mut self) -> Result<Vec<AgentDelta>, AgentError> {
        let mut deltas = Vec::new();
        while let Ok(delta) = self.delta_rx.try_recv() {
            deltas.push(delta);
        }
        Ok(deltas)
    }
}

pub struct ClaudeCode {
    child: Option<Child>,
    cmd_tx: mpsc::UnboundedSender<AcpCommand>,
    data_dir: PathBuf,
}

async fn run_session(
    mut session: ActiveSession<'static, agent_client_protocol::Agent>,
    mut cmd_rx: mpsc::UnboundedReceiver<SessionCommand>,
    delta_tx: mpsc::UnboundedSender<AgentDelta>,
) {
    use agent_client_protocol::util::MatchDispatch;

    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            SessionCommand::SendPrompt(text) => {
                if let Err(e) = session.send_prompt(&text) {
                    tracing::error!("send_prompt error: {e}");
                    continue;
                }

                loop {
                    match session.read_update().await {
                        Ok(SessionMessage::SessionMessage(dispatch)) => {
                            let _ = MatchDispatch::new(dispatch)
                                .if_notification(
                                    async |notif: SessionNotification| {
                        if let SessionUpdate::AgentMessageChunk(chunk) = &notif.update {
                            if let ContentBlock::Text(text) = &chunk.content {
                                let _ = delta_tx.send(AgentDelta::Text {
                                    output: text.text.clone(),
                                    done: false,
                                });
                            }
                        }
                                        Ok(())
                                    },
                                )
                                .await
                                .otherwise_ignore();
                        }
                        Ok(SessionMessage::StopReason(_)) => {
                            let _ = delta_tx.send(AgentDelta::Text {
                                output: String::new(),
                                done: true,
                            });
                            break;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            tracing::error!("read_update error: {e}");
                            break;
                        }
                    }
                }
            }
        }
    }
}

impl ClaudeCode {
    pub fn new(data_dir: PathBuf) -> Self {
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        ClaudeCode {
            child: None,
            cmd_tx,
            data_dir,
        }
    }

    fn create_session_dir(&self, session_id: &str) {
        let dir = self.data_dir.join("session").join(session_id);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!("Failed to create session directory {}: {e}", dir.display());
        } else {
            tracing::info!("Session directory: {}", dir.display());
        }
    }

    pub async fn create_session(&mut self, pwd: PathBuf) -> Result<(String, Box<dyn AgentSession>), AgentError> {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        let (delta_tx, delta_rx) = mpsc::unbounded_channel();

        self.cmd_tx
            .send(AcpCommand::CreateSession { pwd, response_tx, delta_tx })
            .map_err(|_| AgentError::Acp("channel closed".into()))?;

        let (session_id, cmd_tx) = response_rx
            .await
            .map_err(|_| AgentError::Acp("session creation response lost".into()))?
            .map_err(AgentError::Acp)?;

        self.create_session_dir(&session_id);

        let session = ClaudeCodeSession { session_id: session_id.clone(), cmd_tx, delta_rx };
        Ok((session_id, Box::new(session)))
    }

    pub async fn finish(&mut self) {
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

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let (init_tx, init_rx) = tokio::sync::oneshot::channel();

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
                    connection
                        .send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;

                    init_tx.send(()).ok();

                    while let Some(cmd) = cmd_rx.recv().await {
                        match cmd {
                            AcpCommand::CreateSession { pwd, response_tx, delta_tx } => {
                                match connection.build_session(&pwd).block_task().start_session().await {
                                    Ok(session) => {
                                        let sid = session.session_id().clone().to_string();
                                        let (session_cmd_tx, session_cmd_rx) = mpsc::unbounded_channel();

                                        tokio::spawn(run_session(session, session_cmd_rx, delta_tx));

                                        let _ = response_tx.send(Ok((sid, session_cmd_tx)));
                                    }
                                    Err(e) => {
                                        let _ = response_tx.send(Err(e.to_string()));
                                    }
                                }
                            }
                            AcpCommand::Shutdown => break,
                        }
                    }
                    Ok(())
                })
                .await;

            if let Err(e) = result {
                tracing::error!("ACP connection error: {e}");

            }
        });

        self.cmd_tx = cmd_tx;
        self.child = Some(child);

        match init_rx.await {
            Ok(_) => Ok(()),
            Err(_) => Err(AgentError::Acp("ACP initialization failed".into())),
        }
    }

    pub async fn shutdown(&mut self) -> Result<(), AgentError> {
        let _ = self.cmd_tx.send(AcpCommand::Shutdown);
        self.finish().await;
        Ok(())
    }
}
