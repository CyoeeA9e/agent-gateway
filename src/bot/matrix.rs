use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
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
                message::{
                    MessageType, OriginalSyncRoomMessageEvent,
                    RoomMessageEventContent,
                },
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
    if let Ok(json) = serde_json::to_string_pretty(sessions) {
        let _ = std::fs::write(path, json);
    }
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
        let cfg = GatewayConfig::from_file(&self.config_path)?;

        let matrix_cfg = &cfg.matrix;
        let user_id: OwnedUserId = matrix_cfg.id.parse().context("Invalid Matrix user ID")?;
        let server_name = user_id.server_name().to_string();
        let homeserver_url = format!("https://{server_name}");

        tracing::info!("Allowed users: {:?}", self.allowed);

        let session_file = self.state_dir.join("session.json");
        let crypto_path = self.state_dir.join("matrix_store");
        std::fs::create_dir_all(&crypto_path)?;

        let state_store = matrix_sdk::SqliteStateStore::open(&crypto_path, None)
            .await
            .context("Failed to open state store")?;
        let crypto_store = matrix_sdk::SqliteCryptoStore::open(&crypto_path, None)
            .await
            .context("Failed to open crypto store")?;

        tracing::info!("Connecting to {homeserver_url} as {user_id}");

        let client = Client::builder()
            .homeserver_url(&homeserver_url)
            .store_config(
                matrix_sdk::config::StoreConfig::new("agent-gateway".to_owned())
                    .state_store(state_store)
                    .crypto_store(crypto_store),
            )
            .build()
            .await
            .context("Failed to build Matrix client")?;

        // Restore session from file, or login fresh
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
                .login_username(&user_id, &matrix_cfg.password)
                .initial_device_display_name("agent-gateway")
                .await
                .context("Failed to login")?;
            tracing::info!("Logged in successfully");

            // Save session to file for next restart
            if let Some(session) = client.matrix_auth().session() {
                let json = serde_json::to_string(&session)?;
                std::fs::write(&session_file, json)?;
                tracing::info!("Session saved to {}", session_file.display());
            }
        }

        client
            .encryption()
            .wait_for_e2ee_initialization_tasks()
            .await;
        tracing::info!("Encryption initialized");

        // Check for another active instance on the same account
        let current_device_id = client.device_id().map(|d| d.to_string());
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let devices = client.devices().await?;
        for device in &devices.devices {
            if device.display_name.as_deref() != Some("agent-gateway") {
                continue;
            }
            if Some(device.device_id.to_string()) == current_device_id {
                continue;
            }
            if let Some(ts) = device.last_seen_ts {
                let elapsed_ms = now_ms.saturating_sub(i64::from(ts.0) as u64);
                if elapsed_ms < 60_000 {
                    anyhow::bail!(
                        "Another agent-gateway instance is already active (device {}). \
                         Log out of the other instance first.",
                        device.device_id,
                    );
                }
                tracing::warn!(
                    "Found stale gateway device {} (last seen {}s ago), continuing",
                    device.device_id,
                    elapsed_ms / 1000,
                );
            } else {
                tracing::warn!(
                    "Found gateway device {} with no last_seen, continuing",
                    device.device_id,
                );
            }
        }

        // ---- invite handler ----
        {
            let bot = self.clone();
            client.add_event_handler(
                move |event: StrippedRoomMemberEvent,
                      room: matrix_sdk::room::Room,
                      client: Client| {
                    let bot = bot.clone();
                    async move { on_invite(event, room, client, &bot).await }
                },
            );
        }

        // ---- member leave handler ----
        {
            let bot = self.clone();
            client.add_event_handler(
                move |event: OriginalSyncRoomMemberEvent, room: matrix_sdk::room::Room| {
                    let bot = bot.clone();
                    async move { on_member_change(event, room, &bot).await }
                },
            );
        }

        // ---- room message handler ----
        {
            let bot = self.clone();
            client.add_event_handler(
                move |event: OriginalSyncRoomMessageEvent, room: matrix_sdk::room::Room| {
                    let bot = bot.clone();
                    async move { on_room_message(event, room, &bot).await }
                },
            );
        }

        // ---- encrypted message handler ----
        {
            let bot = self.clone();
            client.add_event_handler(
                move |event: OriginalSyncRoomEncryptedEvent, room: matrix_sdk::room::Room| {
                    let bot = bot.clone();
                    async move { on_encrypted_message(event, room, &bot).await }
                },
            );
        }

        // ---- key stream listener ----
        {
            use futures::StreamExt;
            let key_client = client.clone();
            let bot = self.clone();
            tokio::spawn(async move {
                let Some(mut stream) = key_client.encryption().room_keys_received_stream().await
                else {
                    tracing::warn!("No Olm machine for key stream");
                    return;
                };
                loop {
                    match stream.next().await {
                        Some(Ok(keys)) => {
                            for key_info in keys {
                                let room_id = &key_info.room_id;
                                let room_id_str = room_id.as_str().to_owned();
                                let events = {
                                    let mut p = bot.pending_encrypted.lock().await;
                                    p.remove(&room_id_str).unwrap_or_default()
                                };
                                if !events.is_empty() {
                                    tracing::info!("Processing {} pending events for {room_id_str}", events.len());
                                }
                                for event_id in events {
                                    let room = match key_client.get_room(room_id) {
                                        Some(room) => room,
                                        None => continue,
                                    };
                                    match room.event(&event_id, None).await {
                                        Ok(timeline_event) => {
                                            let raw = timeline_event.raw();
                                            match raw.deserialize() {
                                                Ok(AnySyncTimelineEvent::MessageLike(
                                                    AnySyncMessageLikeEvent::RoomMessage(
                                                        SyncMessageLikeEvent::Original(decrypted),
                                                    ),
                                                )) => {
                                                    if let MessageType::Text(text) =
                                                        &decrypted.content.msgtype
                                                    {
                                                        bot.run_user_prompt(
                                                            text.body.clone(),
                                                            room.clone(),
                                                        )
                                                        .await;
                                                    }
                                                }
                                                _ => tracing::warn!(
                                                    "Pending event {event_id} not a text message"
                                                ),
                                            }
                                        }
                                        Err(e) => {
                                            tracing::warn!(
                                                "Fetching pending event {event_id}: {e}"
                                            );
                                            bot.pending_encrypted
                                                .lock()
                                                .await
                                                .entry(room_id_str.clone())
                                                .or_default()
                                                .push(event_id.clone());
                                        }
                                    }
                                }
                            }
                        }
                        Some(Err(e)) => tracing::warn!("Key stream error: {e}"),
                        None => {
                            tracing::info!("Key stream ended");
                            break;
                        }
                    }
                }
            });
        }

        // Sync loop with graceful shutdown on Ctrl+C
        tracing::info!("Starting sync loop");
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
        // Shutdown: persist sessions and drop agents before stopping ACP
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

impl MatrixBot {
    fn is_allowed(&self, sender: &str) -> bool {
        self.allowed.contains(sender)
    }

    async fn handle_command(&self, body: &str, room_id: &str) -> Result<Option<String>, MatrixError> {
        if !body.starts_with('/') {
            return Ok(None);
        }

        let cmd = body.trim();

        if cmd == "/help" {
            return Ok(Some(
                "Available commands:\n/help — Show this help\n/setpwd <path> — Set working directory\n/reset — Reset the Claude Code session\n/agent <type> — Switch agent (none, claude-code-acp, opencode)"
                    .into(),
            ));
        }

        if cmd == "/reset" {
            let mut sessions = self.sessions.lock().await;
            if let Some(session) = sessions.get_mut(room_id) {
                session.agent_session = None;
                session.clear_agent_session_id();
            }
            persist_sessions(&sessions, &self.sessions_path);
            return Ok(Some("Session reset. A new session will be created on next message.".into()));
        }

        if cmd == "/agent" {
            let sessions = self.sessions.lock().await;
            let current = sessions
                .get(room_id)
                .map(|s| format!("{:?}", s.agent_type))
                .unwrap_or_else(|| "none".into());
            return Ok(Some(format!(
                "Current agent: {current}\nAvailable: none, claude-code-acp, opencode"
            )));
        }

        if let Some(type_str) = cmd.strip_prefix("/agent ") {
            let new_type = match type_str.trim() {
                "opencode" => AgentType::OpenCode,
                "claude-code-acp" => AgentType::ClaudeCodeAcp,
                "none" => AgentType::None,
                _ => {
                    return Ok(Some(
                        "Unknown agent type. Available: none, claude-code-acp, opencode".into(),
                    ));
                }
            };
            let mut sessions = self.sessions.lock().await;
            if let Some(session) = sessions.get_mut(room_id) {
                session.agent_session = None;
                session.clear_agent_session_id();
                session.agent_type = new_type;
            }
            persist_sessions(&sessions, &self.sessions_path);
            return Ok(Some(format!("Switched to {}.", type_str.trim())));
        }

        if cmd == "/setpwd" {
            let sessions = self.sessions.lock().await;
            if let Some(session) = sessions.get(room_id) {
                match session.pwd() {
                    Some(p) => return Ok(Some(format!("Working directory: {}", p.display()))),
                    None => return Ok(Some("No working directory set. Usage: /setpwd <path>".into())),
                }
            }
            return Ok(Some("No working directory set. Usage: /setpwd <path>".into()));
        }

        if let Some(path_str) = cmd.strip_prefix("/setpwd ") {
            let path_str = path_str.trim();
            if path_str.is_empty() {
                return Ok(Some("Usage: /setpwd <path>".into()));
            }

            let p = match std::fs::canonicalize(path_str) {
                Ok(p) if p.is_dir() => p,
                Ok(_) => return Ok(Some(format!("Not a directory: {path_str}"))),
                Err(e) => return Err(MatrixError::Io(e)),
            };

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

            if agent_type != AgentType::None {
                if let Some(sid) = session_id {
                    let mut cc = self.cc.lock().await;
                    let (new_sid, new_session) = cc.create_session(p.clone(), &agent_type, Some(sid)).await?;

                    let mut sessions = self.sessions.lock().await;
                    if let Some(s) = sessions.get_mut(room_id) {
                        s.set_agent_session_id(new_sid);
                        s.agent_session = Some(new_session);
                        persist_sessions(&sessions, &self.sessions_path);
                    }
                }
            }

            return Ok(Some(format!("Working directory set to: {}", p.display())));
        }

        Ok(Some(format!(
            "Unknown command: {cmd}\nType /help for available commands"
        )))
    }

    async fn get_or_create_session(
        &self,
        room: &matrix_sdk::room::Room,
    ) -> Result<Box<dyn AgentSession>, AgentError> {
        let room_id = room.room_id().as_str();
        let mut smap = self.sessions.lock().await;
        match smap.get_mut(room_id).and_then(|s| s.agent_session.take()) {
            Some(a) => Ok(a),
            None => {
                let (pwd, agent_type, session_id) = match smap.get(room_id) {
                    Some(s) => {
                        if s.agent_type == AgentType::None {
                            return Err(AgentError::Acp(
                                "No agent selected. Use /agent <type> to choose an agent.\nAvailable: none, claude-code-acp, opencode".into(),
                            ));
                        }
                        (
                            s.pwd.clone().unwrap_or_else(make_temp_dir),
                            s.agent_type.clone(),
                            s.agent_session_id.clone(),
                        )
                    }
                    None => {
                        let s = Session::new(room_id.to_owned());
                        save_session(&mut smap, &self.sessions_path, s);
                        tracing::info!("New session for room {} (no agent)", room_id);
                        return Err(AgentError::Acp(
                            "No agent selected. Use /agent <type> to choose an agent.\nAvailable: none, claude-code-acp, opencode".into(),
                        ));
                    }
                };
                drop(smap);

                let mut cc = self.cc.lock().await;
                match cc.create_session(pwd, &agent_type, session_id).await {
                    Ok((sid, agent)) => {
                        let mut smap = self.sessions.lock().await;
                        if let Some(entry) = smap.get_mut(room_id) {
                            entry.set_agent_session_id(sid);
                            persist_sessions(&smap, &self.sessions_path);
                        }
                        Ok(agent)
                    }
                    Err(e) => Err(e),
                }
            }
        }
    }

    async fn run_user_prompt(&self, body: String, room: matrix_sdk::room::Room) {
        let agent_session = match self.get_or_create_session(&room).await {
            Ok(a) => a,
            Err(e) => {
                let content = RoomMessageEventContent::text_plain(e.to_string());
                let _ = room.send(content).await;
                return;
            }
        };
        self.run_task(body, room, agent_session).await;
    }

    async fn run_task(
        &self,
        body: String,
        room: matrix_sdk::room::Room,
        mut agent_session: Box<dyn AgentSession>,
    ) {
        let room_id = room.room_id().to_string();
        if let Err(e) = agent_session.send_input(&body) {
            tracing::error!("Failed to send input to Claude Code: {e}");
            let _ = room.typing_notice(false).await;
            let content = RoomMessageEventContent::text_plain(format!("Error: {e}"));
            let _ = room.send(content).await;
            if let Some(s) = self.sessions.lock().await.get_mut(&room_id) {
                s.agent_session = Some(agent_session);
            }
            return;
        }

        let mut full_output = String::new();
        loop {
            if room.typing_notice(true).await.is_err() {
                tracing::info!("Bot no longer in room {room_id}, stopping processing");
                let _ = room.typing_notice(false).await;
                break;
            }

            match agent_session.query_delta().await {
                Ok(Some(AgentDelta::Text {
                    output,
                    done: is_done,
                })) => {
                    if !output.is_empty() {
                        full_output.push_str(&output);
                    }
                    if is_done {
                        let _ = room.typing_notice(false).await;
                        if !full_output.is_empty() {
                            let content = RoomMessageEventContent::text_plain(&full_output);
                            if let Err(e) = room.send(content).await {
                                tracing::error!("Failed to send Claude response: {e}");
                            }
                        }
                        break;
                    }
                }
                Ok(Some(AgentDelta::ToolCall { title, input })) => {
                    // Flush accumulated thought text
                    if !full_output.is_empty() {
                        let content = RoomMessageEventContent::text_plain(&full_output);
                        if let Err(e) = room.send(content).await {
                            tracing::error!("Failed to send thought text: {e}");
                        }
                        full_output.clear();
                    }
                    // Send tool call notification
                    let tool_msg = match input {
                        Some(args) => format!("🔧 {}({})", title, args),
                        None => format!("🔧 {}()", title),
                    };
                    let content = RoomMessageEventContent::text_plain(&tool_msg);
                    if let Err(e) = room.send(content).await {
                        tracing::error!("Failed to send tool call: {e}");
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::error!("Error querying Claude Code: {e}");
                    break;
                }
            }
        }

        if let Some(s) = self.sessions.lock().await.get_mut(&room_id) {
            s.agent_session = Some(agent_session);
        }
    }
}

async fn on_room_message(
    event: OriginalSyncRoomMessageEvent,
    room: matrix_sdk::room::Room,
    bot: &MatrixBot,
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
        text_content.body,
    );

    if text_content.body.starts_with('/') {
        match bot
            .handle_command(&text_content.body, room.room_id().as_str())
            .await
        {
            Ok(Some(reply)) => {
                let content = RoomMessageEventContent::text_plain(&reply);
                if let Err(e) = room.send(content).await {
                    tracing::error!("Failed to send command reply: {e}");
                }
                return;
            }
            Ok(None) => {}
            Err(e) => {
                let content = RoomMessageEventContent::text_plain(format!("Error: {e}"));
                let _ = room.send(content).await;
                return;
            }
        }
    }

    bot.run_user_prompt(text_content.body.clone(), room).await;
}

async fn on_encrypted_message(
    event: OriginalSyncRoomEncryptedEvent,
    _room: matrix_sdk::room::Room,
    bot: &MatrixBot,
) {
    if *_room.own_user_id() == event.sender {
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
        _room.room_id(),
    );

    bot.pending_encrypted
        .lock()
        .await
        .entry(_room.room_id().to_string())
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
    let joined = match client.join_room_by_id(room.room_id()).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("Failed to join room {}: {e}", room.room_id());
            return;
        }
    };

    tracing::info!(
        "Joined room {} ({})",
        joined.room_id(),
        joined.name().unwrap_or_else(|| "unnamed".into()),
    );

    let map = bot.sessions.lock().await;
    if map.contains_key(joined.room_id().as_str()) {
        return;
    }
    drop(map);

    let pwd = make_temp_dir();
    let result = bot
        .cc
        .lock()
        .await
        .create_session(pwd.clone(), &AgentType::ClaudeCodeAcp, None)
        .await;

    let room_id = joined.room_id().to_string();
    let mut map = bot.sessions.lock().await;
    match result {
        Ok((sid, agent)) => {
            let s = Session {
                room_id: room_id.clone(),
                agent_session_id: Some(sid),
                pwd: Some(pwd),
                agent_type: AgentType::ClaudeCodeAcp,
                agent_session: Some(agent),
            };
            tracing::info!("Created ACP session for room {room_id}");
            save_session(&mut map, &bot.sessions_path, s);
        }
        Err(_) => {
            save_session(&mut map, &bot.sessions_path, Session::new(room_id));
        }
    }
}

async fn on_member_change(
    event: OriginalSyncRoomMemberEvent,
    room: matrix_sdk::room::Room,
    bot: &MatrixBot,
) {
    let room_id = room.room_id().to_string();

    if event.state_key.as_str() == bot.bot_id {
        if event.content.membership == MembershipState::Leave
            || event.content.membership == MembershipState::Ban
        {
            tracing::info!("Removed from room {room_id}, cleaning up session");
            let mut map = bot.sessions.lock().await;
            map.remove(&room_id);
            persist_sessions(&map, &bot.sessions_path);
        }
    } else if event.content.membership == MembershipState::Leave
        || event.content.membership == MembershipState::Ban
    {
        if let Ok(members) = room.members(RoomMemberships::JOIN).await {
            if members.len() <= 1 {
                tracing::info!("Only bot left in room {room_id}, leaving");
                let mut map = bot.sessions.lock().await;
                map.remove(&room_id);
                persist_sessions(&map, &bot.sessions_path);
                drop(map);
                if let Err(e) = room.leave().await {
                    tracing::error!("Failed to leave empty room {room_id}: {e}");
                }
            }
        }
    }
}
