use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use futures::StreamExt;
use matrix_sdk::{
    Client, RoomMemberships,
    config::SyncSettings,
    ruma::{
        OwnedEventId, OwnedUserId,
        events::{
            AnySyncMessageLikeEvent, AnySyncTimelineEvent, SyncMessageLikeEvent,
            room::{
                encrypted::OriginalSyncRoomEncryptedEvent,
                member::{MembershipState, OriginalSyncRoomMemberEvent, StrippedRoomMemberEvent},
                message::{MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent},
            },
        },
    },
};
use tokio::sync::Mutex as AsyncMutex;

use serde::{Deserialize, Serialize};

use crate::agent::{AgentDelta, AgentError, AgentRegistry, AgentSession, AgentType};
use crate::config::GatewayConfig;

#[derive(Debug)]
pub enum MatrixError {
    Agent(AgentError),
    Io(std::io::Error),
}

impl std::fmt::Display for MatrixError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MatrixError::Agent(e) => write!(f, "{e}"),
            MatrixError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for MatrixError {}

impl From<AgentError> for MatrixError {
    fn from(e: AgentError) -> Self {
        MatrixError::Agent(e)
    }
}

impl From<std::io::Error> for MatrixError {
    fn from(e: std::io::Error) -> Self {
        MatrixError::Io(e)
    }
}

fn persist_sessions(sessions: &HashMap<String, Session>, path: &Path) {
    let Ok(json) = serde_json::to_string_pretty(sessions) else {
        return;
    };
    let _ = std::fs::write(path, json);
}

fn save_session(sessions: &mut HashMap<String, Session>, path: &Path, session: Session) {
    sessions.insert(session.room_id().to_owned(), session);
    persist_sessions(sessions, path);
}

pub struct MatrixBot {
    cc: AsyncMutex<AgentRegistry>,
    sessions: AsyncMutex<HashMap<String, Session>>,
    sessions_path: PathBuf,
    state_dir: PathBuf,
    config_path: PathBuf,
    pending_encrypted: AsyncMutex<HashMap<String, Vec<OwnedEventId>>>,
    allowed: Arc<HashSet<String>>,
    bot_id: String,
}

impl MatrixBot {
    pub fn new(
        registry: AgentRegistry,
        state_dir: PathBuf,
        config_path: PathBuf,
        sessions: HashMap<String, Session>,
        sessions_path: PathBuf,
    ) -> Arc<Self> {
        let cfg = GatewayConfig::from_file(&config_path).ok();
        let allowed = cfg
            .as_ref()
            .map(|c| Arc::new(c.matrix.allowed_user.iter().cloned().collect()))
            .unwrap_or_default();
        let bot_id = cfg
            .as_ref()
            .map(|c| c.matrix.id.clone())
            .unwrap_or_default();
        Arc::new(MatrixBot {
            cc: AsyncMutex::new(registry),
            sessions: AsyncMutex::new(sessions),
            sessions_path,
            state_dir,
            config_path,
            pending_encrypted: AsyncMutex::new(HashMap::new()),
            allowed,
            bot_id,
        })
    }

    pub async fn run(self: Arc<Self>) -> anyhow::Result<()> {
        let client = self.build_client().await?;
        self.login_or_restore(&client).await?;
        client
            .encryption()
            .wait_for_e2ee_initialization_tasks()
            .await;
        tracing::info!("Encryption initialized");

        self.check_duplicate_instance(&client).await;
        self.register_event_handlers(&client);
        self.spawn_key_stream_listener(&client);

        tracing::info!("Starting sync loop");
        self.run_sync_loop(&client).await;

        self.shutdown().await
    }

    async fn build_client(&self) -> anyhow::Result<Client> {
        let cfg = GatewayConfig::from_file(&self.config_path)?;
        let matrix_cfg = &cfg.matrix;
        let user_id: OwnedUserId = matrix_cfg.id.parse().context("Invalid Matrix user ID")?;
        let server_name = user_id.server_name().to_string();
        let homeserver_url = format!("https://{server_name}");

        tracing::info!("Allowed users: {:?}", self.allowed);

        let crypto_path = self.state_dir.join("matrix_store");
        std::fs::create_dir_all(&crypto_path)?;

        let state_store = matrix_sdk::SqliteStateStore::open(&crypto_path, None)
            .await
            .context("Failed to open state store")?;
        let crypto_store = matrix_sdk::SqliteCryptoStore::open(&crypto_path, None)
            .await
            .context("Failed to open crypto store")?;

        tracing::info!("Connecting to {homeserver_url} as {user_id}");

        Client::builder()
            .homeserver_url(&homeserver_url)
            .store_config(
                matrix_sdk::config::StoreConfig::new("agent-gateway".to_owned())
                    .state_store(state_store)
                    .crypto_store(crypto_store),
            )
            .build()
            .await
            .context("Failed to build Matrix client")
    }

    async fn login_or_restore(&self, client: &Client) -> anyhow::Result<()> {
        let cfg = GatewayConfig::from_file(&self.config_path)?;
        let user_id: OwnedUserId = cfg.matrix.id.parse().context("Invalid Matrix user ID")?;
        let session_file = self.state_dir.join("session.json");

        if session_file.exists() {
            tracing::info!("Restoring session from file...");
            let session_json = std::fs::read_to_string(&session_file)?;
            if let Ok(session) = serde_json::from_str(&session_json)
                && client.matrix_auth().restore_session(session).await.is_ok()
            {
                tracing::info!("Session restored");
            }
        }

        if !client.matrix_auth().logged_in() {
            tracing::info!("Logging in...");
            client
                .matrix_auth()
                .login_username(&user_id, &cfg.matrix.password)
                .initial_device_display_name("agent-gateway")
                .await
                .context("Failed to login")?;
            tracing::info!("Logged in successfully");

            if let Some(session) = client.matrix_auth().session() {
                let json = serde_json::to_string(&session)?;
                std::fs::write(&session_file, json)?;
                tracing::info!("Session saved to {}", session_file.display());
            }
        }

        Ok(())
    }

    async fn check_duplicate_instance(&self, client: &Client) {
        let current_device_id = client.device_id().map(|d| d.to_string());
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let Ok(Ok(devices)) = tokio::time::timeout(Duration::from_secs(15), client.devices()).await
        else {
            tracing::warn!("Skipping duplicate instance check (devices API failed/timed out)");
            return;
        };

        for device in &devices.devices {
            if device.display_name.as_deref() != Some("agent-gateway") {
                continue;
            }
            if Some(device.device_id.to_string()) == current_device_id {
                continue;
            }
            let Some(ts) = device.last_seen_ts else {
                tracing::warn!(
                    "Found gateway device {} with no last_seen, continuing",
                    device.device_id
                );
                continue;
            };
            let elapsed_ms = now_ms.saturating_sub(i64::from(ts.0) as u64);
            if elapsed_ms < 60_000 {
                tracing::error!(
                    "Another agent-gateway instance is active (device {})",
                    device.device_id,
                );
                return;
            }
            tracing::warn!(
                "Found stale gateway device {} (last seen {}s ago), continuing",
                device.device_id,
                elapsed_ms / 1000,
            );
        }
    }

    fn register_event_handlers(self: &Arc<Self>, client: &Client) {
        let bot = self.clone();
        client.add_event_handler(
            move |event: StrippedRoomMemberEvent, room: matrix_sdk::room::Room, client: Client| {
                let bot = bot.clone();
                async move { on_invite(event, room, client, &bot).await }
            },
        );

        let bot = self.clone();
        client.add_event_handler(
            move |event: OriginalSyncRoomMemberEvent, room: matrix_sdk::room::Room| {
                let bot = bot.clone();
                async move { on_member_change(event, room, &bot).await }
            },
        );

        let bot = self.clone();
        client.add_event_handler(
            move |event: OriginalSyncRoomMessageEvent, room: matrix_sdk::room::Room| {
                let bot = bot.clone();
                async move { on_room_message(event, room, bot).await }
            },
        );

        let bot = self.clone();
        client.add_event_handler(
            move |event: OriginalSyncRoomEncryptedEvent, room: matrix_sdk::room::Room| {
                let bot = bot.clone();
                async move { on_encrypted_message(event, room, &bot).await }
            },
        );
    }

    fn spawn_key_stream_listener(self: &Arc<Self>, client: &Client) {
        let key_client = client.clone();
        let bot = self.clone();
        tokio::spawn(async move {
            let Some(mut stream) = key_client.encryption().room_keys_received_stream().await else {
                tracing::warn!("No Olm machine for key stream");
                return;
            };
            while let Some(data) = stream.next().await {
                let Ok(keys) = data else {
                    tracing::warn!("Key stream error");
                    continue;
                };
                for key_info in keys {
                    let room_id = key_info.room_id;
                    let room_id_str = room_id.as_str().to_owned();
                    let events = {
                        let mut p = bot.pending_encrypted.lock().await;
                        p.remove(&room_id_str).unwrap_or_default()
                    };
                    if events.is_empty() {
                        continue;
                    }
                    tracing::info!(
                        "Processing {} pending events for {room_id_str}",
                        events.len()
                    );
                    let Some(room) = key_client.get_room(&room_id) else {
                        continue;
                    };
                    for event_id in events {
                        bot.process_pending_event(&room, &room_id_str, event_id)
                            .await;
                    }
                }
            }
        });
    }

    async fn process_pending_event(
        &self,
        room: &matrix_sdk::room::Room,
        room_id_str: &str,
        event_id: OwnedEventId,
    ) {
        let Ok(timeline_event) = room.event(&event_id, None).await else {
            tracing::warn!("Fetching {event_id}: failed, re-queuing");
            self.requeue_event(room_id_str, event_id).await;
            return;
        };

        let raw = timeline_event.raw();
        match raw.deserialize() {
            Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomEncrypted(_))) => {
                tracing::warn!("Event {event_id} still encrypted, re-queue");
                self.requeue_event(room_id_str, event_id).await;
            }
            Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
                SyncMessageLikeEvent::Original(d),
            ))) => {
                let MessageType::Text(text) = &d.content.msgtype else {
                    return;
                };
                tracing::info!(
                    "Processing decrypted msg from {} in {}",
                    d.sender,
                    room_id_str
                );
                self.run_user_prompt(text.body.clone(), room.clone()).await;
            }
            _ => tracing::warn!("Pending event {event_id} unexpected type"),
        }
    }

    async fn requeue_event(&self, room_id_str: &str, event_id: OwnedEventId) {
        let mut p = self.pending_encrypted.lock().await;
        p.entry(room_id_str.to_owned()).or_default().push(event_id);
    }

    async fn run_sync_loop(&self, client: &Client) {
        let sync_settings = SyncSettings::default();
        loop {
            tokio::select! {
                biased;
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("Received SIGINT, shutting down...");
                    break;
                }
                result = client.sync(sync_settings.clone()) => {
                    if let Err(e) = result {
                        tracing::warn!("Sync error, retrying in 5s: {e}");
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                }
            }
        }
    }

    async fn shutdown(&self) -> anyhow::Result<()> {
        let mut map = self.sessions.lock().await;
        for s in map.values_mut() {
            s.agent_session = None;
        }
        persist_sessions(&map, &self.sessions_path);
        tracing::info!("Room sessions saved");
        drop(map);
        self.cc.lock().await.shutdown().await?;
        tracing::info!("Agent shut down");
        Ok(())
    }
}

fn make_temp_dir() -> PathBuf {
    let mut buf = [0u8; 8];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let _ = f.read_exact(&mut buf);
    }
    let random: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    let path = std::env::temp_dir().join(format!("agent-gateway-{random}"));
    let _ = std::fs::create_dir_all(&path);
    path
}

#[derive(Serialize, Deserialize)]
pub struct Session {
    room_id: String,
    agent_session_id: Option<String>,
    pwd: Option<PathBuf>,
    agent_type: AgentType,
    #[serde(skip)]
    pub agent_session: Option<Box<dyn AgentSession>>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("room_id", &self.room_id)
            .field("agent_session_id", &self.agent_session_id)
            .field("pwd", &self.pwd)
            .field("agent_type", &self.agent_type)
            .field(
                "agent_session",
                &self.agent_session.as_ref().map(|_| "Some(...)"),
            )
            .finish()
    }
}

impl Session {
    pub fn new(room_id: String) -> Self {
        Session {
            room_id,
            agent_session_id: None,
            pwd: Some(make_temp_dir()),
            agent_type: AgentType::default(),
            agent_session: None,
        }
    }

    pub fn room_id(&self) -> &str {
        &self.room_id
    }

    pub fn agent_session_id(&self) -> Option<&str> {
        self.agent_session_id.as_deref()
    }

    pub fn set_agent_session_id(&mut self, id: String) {
        self.agent_session_id = Some(id);
    }

    pub fn clear_agent_session_id(&mut self) {
        self.agent_session_id = None;
    }

    pub fn pwd(&self) -> Option<&PathBuf> {
        self.pwd.as_ref()
    }

    pub fn set_pwd(&mut self, pwd: Option<PathBuf>) {
        self.pwd = pwd;
    }
}

// ── Command handling ──────────────────────────────────────────────────

impl MatrixBot {
    fn is_allowed(&self, sender: &str) -> bool {
        self.allowed.contains(sender)
    }

    fn no_agent_error() -> String {
        "No agent selected. Use /agent <type> to choose an agent.\nAvailable: none, claude-code-acp, opencode".into()
    }

    async fn handle_command(
        &self,
        body: &str,
        room_id: &str,
    ) -> Result<Option<String>, MatrixError> {
        if !body.starts_with('/') {
            return Ok(None);
        }

        let cmd = body.trim();

        if cmd == "/help" {
            return Ok(Some(
                "Available commands:\n/help — Show this help\n\
                 /setpwd <path> — Set working directory\n\
                 /reset — Reset the Claude Code session\n\
                 /agent <type> — Switch agent (none, claude-code-acp, opencode)"
                    .into(),
            ));
        }

        if cmd == "/reset" {
            return Ok(Some(self.cmd_reset(room_id).await));
        }

        if cmd == "/agent" {
            return Ok(Some(self.cmd_show_agent(room_id).await));
        }

        if let Some(type_str) = cmd.strip_prefix("/agent ") {
            return Ok(Some(self.cmd_set_agent(room_id, type_str.trim()).await));
        }

        if cmd == "/setpwd" {
            return Ok(Some(self.cmd_show_pwd(room_id).await));
        }

        if let Some(path_str) = cmd.strip_prefix("/setpwd ") {
            return self.cmd_set_pwd(room_id, path_str.trim()).await.map(Some);
        }

        Ok(Some(format!(
            "Unknown command: {cmd}\nType /help for available commands"
        )))
    }

    async fn cmd_reset(&self, room_id: &str) -> String {
        let mut sessions = self.sessions.lock().await;
        if let Some(session) = sessions.get_mut(room_id) {
            session.agent_session = None;
            session.clear_agent_session_id();
        }
        persist_sessions(&sessions, &self.sessions_path);
        "Session reset. A new session will be created on next message.".into()
    }

    async fn cmd_show_agent(&self, room_id: &str) -> String {
        let sessions = self.sessions.lock().await;
        let current = sessions
            .get(room_id)
            .map(|s| format!("{:?}", s.agent_type))
            .unwrap_or_else(|| "none".into());
        format!("Current agent: {current}\nAvailable: none, claude-code-acp, opencode")
    }

    async fn cmd_set_agent(&self, room_id: &str, type_str: &str) -> String {
        let new_type = match type_str {
            "opencode" => AgentType::OpenCode,
            "claude-code-acp" => AgentType::ClaudeCodeAcp,
            "none" => AgentType::None,
            _ => return "Unknown agent type. Available: none, claude-code-acp, opencode".into(),
        };
        let mut sessions = self.sessions.lock().await;
        if let Some(session) = sessions.get_mut(room_id) {
            session.agent_session = None;
            session.clear_agent_session_id();
            session.agent_type = new_type;
        }
        persist_sessions(&sessions, &self.sessions_path);
        format!("Switched to {type_str}.")
    }

    async fn cmd_show_pwd(&self, room_id: &str) -> String {
        let sessions = self.sessions.lock().await;
        let Some(session) = sessions.get(room_id) else {
            return "No working directory set. Usage: /setpwd <path>".into();
        };
        match session.pwd() {
            Some(p) => format!("Working directory: {}", p.display()),
            None => "No working directory set. Usage: /setpwd <path>".into(),
        }
    }

    async fn cmd_set_pwd(&self, room_id: &str, path_str: &str) -> Result<String, MatrixError> {
        if path_str.is_empty() {
            return Ok("Usage: /setpwd <path>".into());
        }

        let Ok(p) = std::fs::canonicalize(path_str) else {
            return Err(MatrixError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("path not found: {path_str}"),
            )));
        };

        if !p.is_dir() {
            return Ok(format!("Not a directory: {path_str}"));
        }

        let (agent_type, session_id) = {
            let mut sessions = self.sessions.lock().await;
            let session = sessions
                .entry(room_id.to_owned())
                .or_insert_with(|| Session::new(room_id.to_owned()));
            session.set_pwd(Some(p.clone()));
            let agent_type = session.agent_type.clone();
            let session_id = session.agent_session_id().map(|s| s.to_owned());
            persist_sessions(&sessions, &self.sessions_path);
            (agent_type, session_id)
        };

        if agent_type != AgentType::None
            && let Some(sid) = session_id
        {
            let mut cc = self.cc.lock().await;
            let Ok((new_sid, new_session)) =
                cc.create_session(p.clone(), &agent_type, Some(sid)).await
            else {
                return Ok(format!(
                    "Working directory set to: {} (session resume failed)",
                    p.display()
                ));
            };
            let mut sessions = self.sessions.lock().await;
            if let Some(s) = sessions.get_mut(room_id) {
                s.set_agent_session_id(new_sid);
                s.agent_session = Some(new_session);
                persist_sessions(&sessions, &self.sessions_path);
            }
        }

        Ok(format!("Working directory set to: {}", p.display()))
    }
}

// ── Session & prompt handling ─────────────────────────────────────────

impl MatrixBot {
    async fn get_or_create_session(
        &self,
        room: &matrix_sdk::room::Room,
    ) -> Result<Box<dyn AgentSession>, AgentError> {
        let room_id = room.room_id().as_str();
        let mut smap = self.sessions.lock().await;

        if let Some(s) = smap.get_mut(room_id).and_then(|s| s.agent_session.take()) {
            return Ok(s);
        }

        let Some(s) = smap.get(room_id) else {
            let s = Session::new(room_id.to_owned());
            save_session(&mut smap, &self.sessions_path, s);
            tracing::info!("New session for room {} (no agent)", room_id);
            return Err(AgentError::Acp(Self::no_agent_error()));
        };

        if s.agent_type == AgentType::None {
            return Err(AgentError::Acp(Self::no_agent_error()));
        }

        let pwd = s.pwd.clone().unwrap_or_else(make_temp_dir);
        let agent_type = s.agent_type.clone();
        let session_id = s.agent_session_id.clone();
        drop(smap);

        let mut cc = self.cc.lock().await;
        let (sid, agent) = cc.create_session(pwd, &agent_type, session_id).await?;

        let mut smap = self.sessions.lock().await;
        if let Some(entry) = smap.get_mut(room_id) {
            entry.set_agent_session_id(sid);
            persist_sessions(&smap, &self.sessions_path);
        }
        Ok(agent)
    }

    async fn run_user_prompt(&self, body: String, room: matrix_sdk::room::Room) {
        let Ok(agent_session) = self.get_or_create_session(&room).await else {
            return;
        };
        self.run_task(body, room, agent_session).await;
    }

    async fn send_error(&self, room: &matrix_sdk::room::Room, msg: &str) {
        let content = RoomMessageEventContent::text_plain(msg);
        if let Err(e) = room.send(content).await {
            tracing::error!("Failed to send message: {e}");
        }
    }

    async fn stop_typing(&self, room: &matrix_sdk::room::Room) {
        let _ = room.typing_notice(false).await;
    }

    async fn check_typing(&self, room: &matrix_sdk::room::Room) -> bool {
        room.typing_notice(true).await.is_ok()
    }

    async fn run_task(
        &self,
        body: String,
        room: matrix_sdk::room::Room,
        mut agent_session: Box<dyn AgentSession>,
    ) {
        let room_id = room.room_id().to_string();

        if let Err(e) = agent_session.send_input(&body) {
            tracing::error!("Failed to send input to agent: {e}");
            self.stop_typing(&room).await;
            self.send_error(&room, &format!("Error: {e}")).await;
            if let Some(s) = self.sessions.lock().await.get_mut(&room_id) {
                s.agent_session = Some(agent_session);
            }
            return;
        }

        let mut full_output = String::new();
        while let Ok(Some(delta)) = agent_session.query_delta().await {
            if !self.check_typing(&room).await {
                tracing::info!("Bot no longer in room {room_id}, stopping");
                self.stop_typing(&room).await;
                break;
            }

            match delta {
                AgentDelta::Text { output, done } => {
                    if !output.is_empty() {
                        full_output.push_str(&output);
                    }
                    if done {
                        self.stop_typing(&room).await;
                        if !full_output.is_empty() {
                            self.send_error(&room, &full_output).await;
                        }
                        break;
                    }
                }
                AgentDelta::ToolCall { title, input } => {
                    if !full_output.is_empty() {
                        self.send_error(&room, &full_output).await;
                        full_output.clear();
                    }
                    let tool_msg = match input {
                        Some(args) => format!("🔧 {}({})", title, args),
                        None => format!("🔧 {}()", title),
                    };
                    self.send_error(&room, &tool_msg).await;
                }
            }
        }

        if let Some(s) = self.sessions.lock().await.get_mut(&room_id) {
            s.agent_session = Some(agent_session);
        }
    }
}

// ── Event handlers ────────────────────────────────────────────────────

async fn handle_command_or_prompt(
    event: OriginalSyncRoomMessageEvent,
    room: matrix_sdk::room::Room,
    bot: Arc<MatrixBot>,
) {
    if *room.own_user_id() == event.sender {
        return;
    }
    if !bot.is_allowed(event.sender.as_str()) {
        tracing::debug!("Ignoring message from non-allowed user {}", event.sender);
        return;
    }
    let MessageType::Text(text_content) = &event.content.msgtype else {
        return;
    };

    tracing::info!(
        "Message from {} in {}: {}",
        event.sender,
        room.room_id(),
        text_content.body
    );
    let body = text_content.body.clone();

    if !body.starts_with('/') {
        bot.run_user_prompt(body, room).await;
        return;
    }

    match bot.handle_command(&body, room.room_id().as_str()).await {
        Ok(Some(reply)) => {
            let content = RoomMessageEventContent::text_plain(&reply);
            if let Err(e) = room.send(content).await {
                tracing::error!("Failed to send command reply: {e}");
            }
        }
        Err(e) => {
            let content = RoomMessageEventContent::text_plain(format!("Error: {e}"));
            if let Err(e) = room.send(content).await {
                tracing::error!("Failed to send error reply: {e}");
            }
        }
        Ok(None) => {}
    }
}

async fn on_room_message(
    event: OriginalSyncRoomMessageEvent,
    room: matrix_sdk::room::Room,
    bot: Arc<MatrixBot>,
) {
    tokio::spawn(handle_command_or_prompt(event, room, bot));
}

async fn on_encrypted_message(
    event: OriginalSyncRoomEncryptedEvent,
    room: matrix_sdk::room::Room,
    bot: &MatrixBot,
) {
    if *room.own_user_id() == event.sender {
        return;
    }
    if !bot.is_allowed(event.sender.as_str()) {
        tracing::debug!(
            "Ignoring encrypted message from non-allowed user {}",
            event.sender
        );
        return;
    }

    tracing::info!(
        "Encrypted message from {} in {}, queued for key stream",
        event.sender,
        room.room_id()
    );
    bot.pending_encrypted
        .lock()
        .await
        .entry(room.room_id().to_string())
        .or_default()
        .push(event.event_id);
}

async fn on_invite(
    event: StrippedRoomMemberEvent,
    room: matrix_sdk::room::Room,
    client: Client,
    bot: &MatrixBot,
) {
    if event.state_key.as_str() != bot.bot_id {
        return;
    }
    let inviter = event.sender.to_string();
    tracing::info!("Invite from {inviter} to room {}", room.room_id());

    if !bot.is_allowed(&inviter) {
        tracing::info!("Rejecting invite from non-allowed user {inviter}");
        if let Err(e) = room.leave().await {
            tracing::warn!("Failed to reject invite for {}: {e}", room.room_id());
        }
        return;
    }

    tracing::info!("Accepting invite from allowed user {inviter}");
    let Ok(joined) = client.join_room_by_id(room.room_id()).await else {
        tracing::error!("Failed to join room {}", room.room_id());
        return;
    };

    tracing::info!(
        "Joined room {} ({})",
        joined.room_id(),
        joined.name().unwrap_or_else(|| "unnamed".into())
    );

    let map = bot.sessions.lock().await;
    if map.contains_key(joined.room_id().as_str()) {
        return;
    }
    drop(map);

    let room_id = joined.room_id().to_string();
    let mut map = bot.sessions.lock().await;
    save_session(&mut map, &bot.sessions_path, Session::new(room_id));
}

async fn on_member_change(
    event: OriginalSyncRoomMemberEvent,
    room: matrix_sdk::room::Room,
    bot: &MatrixBot,
) {
    let room_id = room.room_id().to_string();

    if event.state_key.as_str() != bot.bot_id {
        // Someone else left/banned — check if only bot remains
        if event.content.membership != MembershipState::Leave
            && event.content.membership != MembershipState::Ban
        {
            return;
        }
        let Ok(members) = room.members(RoomMemberships::JOIN).await else {
            return;
        };
        if members.len() > 1 {
            return;
        }
        tracing::info!("Only bot left in room {room_id}, leaving");
        let mut map = bot.sessions.lock().await;
        map.remove(&room_id);
        persist_sessions(&map, &bot.sessions_path);
        drop(map);
        if let Err(e) = room.leave().await {
            tracing::error!("Failed to leave empty room {room_id}: {e}");
        }
        return;
    }

    // Bot was removed
    if event.content.membership == MembershipState::Leave
        || event.content.membership == MembershipState::Ban
    {
        tracing::info!("Removed from room {room_id}, cleaning up session");
        let mut map = bot.sessions.lock().await;
        map.remove(&room_id);
        persist_sessions(&map, &bot.sessions_path);
    }
}
