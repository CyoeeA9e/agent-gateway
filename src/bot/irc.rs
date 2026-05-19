use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use native_tls::TlsConnector as NativeTlsConnector;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex as AsyncMutex};
use tokio_native_tls::TlsConnector as TokioTlsConnector;

use serde::{Deserialize, Serialize};

use crate::agent::{AgentDelta, AgentError, AgentRegistry, AgentSession, AgentType};
use crate::config::IrcConfig;

enum MaybeTlsStream {
    Plain(TcpStream),
    Tls(tokio_native_tls::TlsStream<TcpStream>),
}

impl AsyncRead for MaybeTlsStream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            MaybeTlsStream::Plain(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            MaybeTlsStream::Tls(s) => std::pin::Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for MaybeTlsStream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, std::io::Error>> {
        match self.get_mut() {
            MaybeTlsStream::Plain(s) => std::pin::Pin::new(s).poll_write(cx, buf),
            MaybeTlsStream::Tls(s) => std::pin::Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        match self.get_mut() {
            MaybeTlsStream::Plain(s) => std::pin::Pin::new(s).poll_flush(cx),
            MaybeTlsStream::Tls(s) => std::pin::Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        match self.get_mut() {
            MaybeTlsStream::Plain(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            MaybeTlsStream::Tls(s) => std::pin::Pin::new(s).poll_shutdown(cx),
        }
    }
}

fn no_agent_error() -> String {
    "No agent selected. Use !agent <type> to choose an agent.\nAvailable: none, claude-code-acp, opencode".into()
}

fn persist_sessions(sessions: &HashMap<String, IrcSession>, path: &Path) {
    let Ok(json) = serde_json::to_string_pretty(sessions) else {
        return;
    };
    let _ = std::fs::write(path, json);
}

fn make_temp_dir() -> PathBuf {
    let mut buf = [0u8; 8];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let _ = f.read_exact(&mut buf);
    }
    let random: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    let path = std::env::temp_dir().join(format!("agent-gateway-irc-{random}"));
    let _ = std::fs::create_dir_all(&path);
    path
}

fn parse_privmsg(line: &str) -> Option<(String, String, String)> {
    let line = line.strip_prefix(':')?;
    let (prefix, rest) = line.split_once(' ')?;
    let rest = rest.strip_prefix("PRIVMSG ")?;
    let (target, text) = rest.split_once(" :")?;
    let sender = prefix.split('!').next().unwrap_or(prefix).to_owned();
    Some((sender, target.to_owned(), text.to_owned()))
}

#[derive(Serialize, Deserialize)]
struct IrcSession {
    channel: String,
    agent_session_id: Option<String>,
    pwd: Option<PathBuf>,
    agent_type: AgentType,
    #[serde(skip)]
    agent_session: Option<Box<dyn AgentSession>>,
}

impl std::fmt::Debug for IrcSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IrcSession")
            .field("channel", &self.channel)
            .field("agent_session_id", &self.agent_session_id)
            .field("pwd", &self.pwd)
            .field("agent_type", &self.agent_type)
            .field("agent_session", &self.agent_session.as_ref().map(|_| "Some(...)"))
            .finish()
    }
}

impl IrcSession {
    fn new(channel: String) -> Self {
        IrcSession {
            channel,
            agent_session_id: None,
            pwd: Some(make_temp_dir()),
            agent_type: AgentType::None,
            agent_session: None,
        }
    }
}

pub struct IrcBot {
    registry: AsyncMutex<AgentRegistry>,
    config: IrcConfig,
    sessions: AsyncMutex<HashMap<String, IrcSession>>,
    sessions_path: PathBuf,
}

impl IrcBot {
    pub fn new(registry: AgentRegistry, config: IrcConfig, state_dir: PathBuf) -> Arc<Self> {
        let sessions_path = state_dir.join("irc_sessions.json");
        let sessions: HashMap<String, IrcSession> = std::fs::read_to_string(&sessions_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Arc::new(IrcBot {
            registry: AsyncMutex::new(registry),
            config,
            sessions: AsyncMutex::new(sessions),
            sessions_path,
        })
    }

    pub async fn run(self: Arc<Self>) -> anyhow::Result<()> {
        let addr = format!("{}:{}", self.config.server, self.config.port);
        tracing::info!("Connecting to IRC server {addr}");

        let tcp = tokio::time::timeout(
            Duration::from_secs(10),
            TcpStream::connect(&addr),
        )
        .await
        .context("TCP connect timeout")?
        .with_context(|| format!("Failed to connect to {addr}"))?;

        let stream = if self.config.tls {
            let connector = NativeTlsConnector::builder()
                .danger_accept_invalid_certs(true)
                .build()
                .context("Failed to build TLS connector")?;
            let connector = TokioTlsConnector::from(connector);
            let tls_stream = tokio::time::timeout(
                Duration::from_secs(15),
                connector.connect(&self.config.server, tcp),
            )
            .await
            .context("TLS handshake timeout")?
            .context("TLS handshake failed")?;
            MaybeTlsStream::Tls(tls_stream)
        } else {
            MaybeTlsStream::Plain(tcp)
        };

        // Use the stream without split or BufReader
        let (write_tx, mut write_rx) = mpsc::channel::<String>(256);
        let (line_tx, mut line_rx) = mpsc::channel::<String>(256);

        // Single connection handler task
        tokio::spawn(async move {
            let mut stream = stream;
            let mut buf = vec![0u8; 4096];
            let mut partial = Vec::new();

            loop {
                tokio::select! {
                    result = AsyncReadExt::read(&mut stream, &mut buf) => {
                        let n = match result {
                            Ok(0) => break,
                            Ok(n) => n,
                            Err(e) => { tracing::error!("IRC read error: {e}"); break; }
                        };
                        for &b in &buf[..n] {
                            if b == b'\n' {
                                let line = String::from_utf8_lossy(&partial)
                                    .trim_end_matches('\r').to_string();
                                partial.clear();
                                if line_tx.send(line).await.is_err() { break; }
                            } else {
                                partial.push(b);
                            }
                        }
                    }
                    Some(msg) = write_rx.recv() => {
                        if let Err(e) = AsyncWriteExt::write_all(&mut stream, format!("{msg}\r\n").as_bytes()).await {
                            tracing::error!("IRC write error: {e}");
                            break;
                        }
                    }
                }
            }
        });

        if let Some(ref pass) = self.config.password {
            write_tx.send(format!("PASS {pass}")).await.unwrap_or_default();
        }
        let mut current_nick = self.config.nick.clone();
        write_tx.send(format!("NICK {current_nick}")).await.unwrap_or_default();
        write_tx.send(format!("USER {} 0 * :{}", self.config.user, self.config.realname)).await.unwrap_or_default();

        let mut joined = false;
        let mut nick_retries = 0;

        while let Some(line) = line_rx.recv().await {
            tracing::trace!("IRC recv: {line}");

            let line_trimmed = line.trim_start_matches(':');
            if let Some(rest) = line_trimmed.strip_prefix("PING ") {
                let payload = rest.trim_start_matches(':');
                write_tx.send(format!("PONG :{payload}")).await.unwrap_or_default();
                continue;
            }
            if let Some(payload) = line.strip_prefix("PING :") {
                write_tx.send(format!("PONG :{payload}")).await.unwrap_or_default();
                continue;
            }

            if !joined && nick_retries < 5
                && (line.contains(" 432 ") || line.contains(" 433 ") || line.contains(" 436 "))
            {
                nick_retries += 1;
                current_nick = format!("{}_{:02x}", &self.config.nick, nick_retries);
                tracing::warn!("Nick rejected, trying {current_nick}");
                write_tx.send(format!("NICK {current_nick}")).await.unwrap_or_default();
                continue;
            }

            if !joined
                && (line.contains(" 001 ") || line.contains(" 376 ") || line.contains(" 422 "))
            {
                joined = true;
                for channel in &self.config.channels {
                    write_tx.send(format!("JOIN {channel}")).await.unwrap_or_default();
                    let mut sessions = self.sessions.lock().await;
                    sessions.entry(channel.clone()).or_insert_with(|| IrcSession::new(channel.clone()));
                    persist_sessions(&sessions, &self.sessions_path);
                }
                continue;
            }

            let Some((sender, target, text)) = parse_privmsg(&line) else {
                continue;
            };

            if sender.eq_ignore_ascii_case(&self.config.nick) {
                continue;
            }

            if !self.config.allowed_users.is_empty()
                && !self.config.allowed_users.iter().any(|u| sender.eq_ignore_ascii_case(u))
            {
                continue;
            }

            let is_channel = target.starts_with('#');
            let response_target = if is_channel { target.clone() } else { sender.clone() };

            // In channels the bot only responds to @mentions.
            // In private queries every message is for the bot.
            if !is_channel {
                if let Some(cmd) = text.strip_prefix(&self.config.command_prefix) {
                    let response = self.handle_command(cmd, &target).await;
                    if let Some(msg) = response {
                        write_tx.send(format!("PRIVMSG {response_target} :{msg}")).await.unwrap_or_default();
                    }
                    continue;
                }
                let bot = self.clone();
                let write_tx = write_tx.clone();
                tokio::spawn(async move {
                    bot.process_with_agent(text.to_owned(), target.to_owned(), write_tx).await;
                });
                continue;
            }

            // Channel: require @mention
            let mention_body: Option<String> = {
                let nick = &self.config.nick;
                let stripped = text.strip_prefix(&format!("@{nick}"))
                    .or_else(|| text.strip_prefix(&format!("{nick}:")))
                    .or_else(|| text.strip_prefix(&format!("{nick},")))
                    .or_else(|| text.strip_prefix(&format!("{nick} ")))
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_owned());
                if stripped.is_some() { stripped } else {
                    let lower = text.to_ascii_lowercase();
                    let nick_lower = nick.to_ascii_lowercase();
                    let found = lower.strip_prefix(&format!("@{nick_lower}"))
                        .or_else(|| lower.strip_prefix(&format!("{nick_lower}:")))
                        .or_else(|| lower.strip_prefix(&format!("{nick_lower},")));
                    found.and_then(|rest| {
                        let prefix_len = text.len() - rest.len();
                        let trimmed = text[prefix_len..].trim();
                        if trimmed.is_empty() { None } else { Some(trimmed.to_owned()) }
                    })
                }
            };

            if let Some(body) = mention_body {
                let cmd = body.strip_prefix('/');
                if let Some(cmd) = cmd {
                    let response = self.handle_command(cmd, &target).await;
                    if let Some(msg) = response {
                        write_tx.send(format!("PRIVMSG {response_target} :{msg}")).await.unwrap_or_default();
                    }
                } else {
                    let bot = self.clone();
                    let write_tx = write_tx.clone();
                    tokio::spawn(async move {
                        bot.process_with_agent(body, target.to_owned(), write_tx).await;
                    });
                }
            }
        }
        Ok(())
    }
}

// ── Command handling ──────────────────────────────────────────────────

impl IrcBot {
    async fn handle_command(&self, cmd: &str, channel: &str) -> Option<String> {
        let cmd = cmd.trim();

        if cmd == "help" {
            return Some("Available commands:\n/help — Show this help\n\
                 /agent [type] — Show/set agent (none, claude-code-acp, opencode)\n\
                 /reset — Reset the session\n\
                 /setpwd [path] — Show/set working directory".into());
        }

        if cmd == "reset" {
            return Some(self.cmd_reset(channel).await);
        }

        if cmd == "agent" {
            return Some(self.cmd_show_agent(channel).await);
        }

        if let Some(type_str) = cmd.strip_prefix("agent ") {
            return Some(self.cmd_set_agent(channel, type_str.trim()).await);
        }

        if cmd == "setpwd" {
            return Some(self.cmd_show_pwd(channel).await);
        }

        if let Some(path_str) = cmd.strip_prefix("setpwd ") {
            return self.cmd_set_pwd(channel, path_str.trim()).await;
        }

        Some(format!("Unknown command: !{cmd}\nType !help for available commands"))
    }

    async fn cmd_reset(&self, channel: &str) -> String {
        let mut sessions = self.sessions.lock().await;
        if let Some(session) = sessions.get_mut(channel) {
            session.agent_session = None;
            session.agent_session_id = None;
        }
        persist_sessions(&sessions, &self.sessions_path);
        "Session reset. A new session will be created on next message.".into()
    }

    async fn cmd_show_agent(&self, channel: &str) -> String {
        let sessions = self.sessions.lock().await;
        let current = sessions.get(channel).map(|s| format!("{:?}", s.agent_type)).unwrap_or_else(|| "none".into());
        format!("Current agent: {current}\nAvailable: none, claude-code-acp, opencode")
    }

    async fn cmd_set_agent(&self, channel: &str, type_str: &str) -> String {
        let new_type = match type_str {
            "opencode" => AgentType::OpenCode,
            "claude-code-acp" => AgentType::ClaudeCodeAcp,
            "none" => AgentType::None,
            _ => return "Unknown agent type. Available: none, claude-code-acp, opencode".into(),
        };
        let mut sessions = self.sessions.lock().await;
        if let Some(session) = sessions.get_mut(channel) {
            session.agent_session = None;
            session.agent_session_id = None;
            session.agent_type = new_type;
        }
        persist_sessions(&sessions, &self.sessions_path);
        format!("Switched to {type_str}.")
    }

    async fn cmd_show_pwd(&self, channel: &str) -> String {
        let sessions = self.sessions.lock().await;
        let Some(session) = sessions.get(channel) else {
            return "No working directory set. Usage: !setpwd <path>".into();
        };
        match &session.pwd {
            Some(p) => format!("Working directory: {}", p.display()),
            None => "No working directory set. Usage: !setpwd <path>".into(),
        }
    }

    async fn cmd_set_pwd(&self, channel: &str, path_str: &str) -> Option<String> {
        if path_str.is_empty() {
            return Some("Usage: !setpwd <path>".into());
        }
        let Ok(p) = std::fs::canonicalize(path_str) else {
            return Some(format!("Path not found: {path_str}"));
        };
        if !p.is_dir() {
            return Some(format!("Not a directory: {path_str}"));
        }
        let (agent_type, session_id) = {
            let mut sessions = self.sessions.lock().await;
            let session = sessions.entry(channel.to_owned()).or_insert_with(|| IrcSession::new(channel.to_owned()));
            session.pwd = Some(p.clone());
            let agent_type = session.agent_type.clone();
            let session_id = session.agent_session_id.clone();
            persist_sessions(&sessions, &self.sessions_path);
            (agent_type, session_id)
        };
        if agent_type != AgentType::None && let Some(sid) = session_id {
            let mut registry = self.registry.lock().await;
            let Ok((new_sid, new_session)) = registry.create_session(p.clone(), &agent_type, Some(sid)).await else {
                return Some(format!("Working directory set to: {} (session resume failed)", p.display()));
            };
            let mut sessions = self.sessions.lock().await;
            if let Some(s) = sessions.get_mut(channel) {
                s.agent_session_id = Some(new_sid);
                s.agent_session = Some(new_session);
                persist_sessions(&sessions, &self.sessions_path);
            }
        }
        Some(format!("Working directory set to: {}", p.display()))
    }
}

// ── Session & prompt handling ─────────────────────────────────────────

impl IrcBot {
    async fn get_or_create_session(&self, channel: &str) -> Result<Box<dyn AgentSession>, AgentError> {
        let mut smap = self.sessions.lock().await;
        if let Some(s) = smap.get_mut(channel).and_then(|s| s.agent_session.take()) {
            return Ok(s);
        }
        let Some(s) = smap.get(channel) else {
            let s = IrcSession::new(channel.to_owned());
            smap.insert(channel.to_owned(), s);
            persist_sessions(&smap, &self.sessions_path);
            return Err(AgentError::Acp(no_agent_error()));
        };
        if s.agent_type == AgentType::None {
            return Err(AgentError::Acp(no_agent_error()));
        }
        let pwd = s.pwd.clone().unwrap_or_else(make_temp_dir);
        let agent_type = s.agent_type.clone();
        let session_id = s.agent_session_id.clone();
        drop(smap);
        let mut registry = self.registry.lock().await;
        let (sid, agent) = registry.create_session(pwd, &agent_type, session_id).await?;
        let mut smap = self.sessions.lock().await;
        if let Some(entry) = smap.get_mut(channel) {
            entry.agent_session_id = Some(sid);
            persist_sessions(&smap, &self.sessions_path);
        }
        Ok(agent)
    }

    async fn process_with_agent(&self, body: String, channel: String, tx: mpsc::Sender<String>) {
        let agent_session = match self.get_or_create_session(&channel).await {
            Ok(s) => s,
            Err(e) => {
                let _ = tx.send(format!("PRIVMSG {channel} :Error: {e}")).await;
                return;
            }
        };
        let mut agent_session = agent_session;
        if let Err(e) = agent_session.send_input(&body) {
            let _ = tx.send(format!("PRIVMSG {channel} :Error: {e}")).await;
            if let Some(s) = self.sessions.lock().await.get_mut(&channel) {
                s.agent_session = Some(agent_session);
            }
            return;
        }
        let mut full_output = String::new();
        let mut last_tool_title = String::new();
        loop {
            match agent_session.query_delta().await {
                Ok(Some(AgentDelta::Text { output, done })) => {
                    if !output.is_empty() {
                        full_output.push_str(&output);
                    }
                    if done {
                        if !full_output.is_empty() {
                            let _ = tx.send(format!("PRIVMSG {channel} :{full_output}")).await;
                        } else {
                            let _ = tx.send(format!("PRIVMSG {channel} :Task completed")).await;
                        }
                        break;
                    }
                }
                Ok(Some(AgentDelta::ToolCall { title, input })) => {
                    if title.is_empty() && last_tool_title.is_empty() {
                        continue;
                    }
                    let args = input.as_deref().unwrap_or("");
                    if title == last_tool_title && args.is_empty() {
                        continue;
                    }
                    last_tool_title = title.clone();
                    if !full_output.is_empty() {
                        let _ = tx.send(format!("PRIVMSG {channel} :{full_output}")).await;
                        full_output.clear();
                    }
                    let tool_msg = match input {
                        Some(args) if !args.is_empty() => format!("Tool: {}({})", title, args),
                        _ => format!("Tool: {}()", title),
                    };
                    let _ = tx.send(format!("PRIVMSG {channel} :{tool_msg}")).await;
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::error!("Agent query_delta error: {e}");
                    break;
                }
            }
        }
        if let Some(s) = self.sessions.lock().await.get_mut(&channel) {
            s.agent_session = Some(agent_session);
        }
    }
}
