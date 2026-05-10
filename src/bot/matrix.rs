use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
                    MessageType, OriginalSyncRoomMessageEvent, ReplacementMetadata,
                    RoomMessageEventContent,
                },
            },
        },
    },
};

use tokio::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::agent::{AgentDelta, AgentRegistry, AgentSession};
use crate::config::GatewayConfig;

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
    cc: Arc<Mutex<AgentRegistry>>,
    sessions: Arc<Mutex<HashMap<String, Session>>>,
    sessions_path: PathBuf,
    state_dir: PathBuf,
    config_path: PathBuf,
    pending_encrypted: Arc<Mutex<HashMap<String, Vec<OwnedEventId>>>>,
}

impl MatrixBot {
    pub fn new(
        cc: Arc<Mutex<AgentRegistry>>,
        state_dir: PathBuf,
        config_path: PathBuf,
        sessions: HashMap<String, Session>,
        sessions_path: PathBuf,
    ) -> Self {
        Self {
            cc,
            sessions: Arc::new(Mutex::new(sessions)),
            sessions_path,
            state_dir,
            config_path,
            pending_encrypted: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn start(&self) -> anyhow::Result<()> {
        let cfg = GatewayConfig::from_file(&self.config_path)?;

        let matrix_cfg = &cfg.matrix;
        let user_id: OwnedUserId = matrix_cfg.id.parse().context("Invalid Matrix user ID")?;
        let bot_id_str = user_id.to_string();
        let server_name = user_id.server_name().to_string();
        let homeserver_url = format!("https://{server_name}");

        let allowed: Arc<HashSet<String>> =
            Arc::new(matrix_cfg.allowed_user.iter().cloned().collect());
        tracing::info!("Allowed users: {:?}", allowed);

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

        // Check for duplicate gateway instances
        let current_device_id = client.device_id().map(|d| d.to_string());
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let devices = client.devices().await?;
        for device in &devices.devices {
            if device.display_name.as_deref() == Some("agent-gateway")
                && Some(device.device_id.to_string()) != current_device_id
            {
                if let Some(ts) = device.last_seen_ts {
                    let elapsed_ms = now_ms.saturating_sub(i64::from(ts.0) as u64);
                    if elapsed_ms < 60_000 {
                        anyhow::bail!(
                            "Another gateway instance is already running (device {}). \
                             Kill the other instance before starting a new one.",
                            device.device_id,
                        );
                    }
                    tracing::warn!(
                        "Found stale gateway device {} (last seen {}s ago), continuing",
                        device.device_id,
                        elapsed_ms / 1000,
                    );
                } else {
                    anyhow::bail!(
                        "Another gateway instance is running (device {}, no last_seen_ts). \
                         Kill it before starting a new one.",
                        device.device_id,
                    );
                }
            }
        }

        // ---- invite handler ----
        client.add_event_handler({
            let allowed = allowed.clone();
            let bot_id = bot_id_str.clone();
            let sessions = self.sessions.clone();
            let sessions_path = self.sessions_path.clone();
            let cc = self.cc.clone();
            move |event: StrippedRoomMemberEvent, room: matrix_sdk::room::Room, client: Client| {
                let allowed = allowed.clone();
                let bot_id = bot_id.clone();
                let sessions = sessions.clone();
                let sessions_path = sessions_path.clone();
                let cc = cc.clone();
                async move {
                    on_invite(
                        event,
                        room,
                        client,
                        cc,
                        allowed,
                        sessions,
                        sessions_path,
                        bot_id,
                    )
                    .await
                }
            }
        });

        // ---- member leave handler ----
        client.add_event_handler({
            let bot_id = bot_id_str.clone();
            let sessions = self.sessions.clone();
            let sessions_path = self.sessions_path.clone();
            move |event: OriginalSyncRoomMemberEvent, room: matrix_sdk::room::Room| {
                let bot_id = bot_id.clone();
                let sessions = sessions.clone();
                let sessions_path = sessions_path.clone();
                async move {
                    on_member_change(event, room, sessions, sessions_path, bot_id).await
                }
            }
        });

        client.add_event_handler({
            let cc = self.cc.clone();
            let allowed = allowed.clone();
            let sessions = self.sessions.clone();
            let sessions_path = self.sessions_path.clone();
            move |event, room| {
                let cc = cc.clone();
                let allowed = allowed.clone();
                let sessions = sessions.clone();
                let sessions_path = sessions_path.clone();
                on_room_message(event, room, cc, allowed, sessions, sessions_path)
            }
        });

        // ---- encrypted message handler ----
        client.add_event_handler({
            let allowed = allowed.clone();
            let pending = self.pending_encrypted.clone();
            move |event: OriginalSyncRoomEncryptedEvent, room: matrix_sdk::room::Room| {
                let allowed = allowed.clone();
                let pending = pending.clone();
                on_encrypted_message(event, room, allowed, pending)
            }
        });

        // ---- key stream listener ----
        {
            use futures::StreamExt;
            let key_client = client.clone();
            let pending = self.pending_encrypted.clone();
            let cc = self.cc.clone();
            let sessions = self.sessions.clone();
            let sessions_path = self.sessions_path.clone();
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
                                    let mut p = pending.lock().await;
                                    p.remove(&room_id_str).unwrap_or_default()
                                };
                                if events.is_empty() {
                                    continue;
                                }
                                let Some(room) = key_client.get_room(room_id) else {
                                    continue;
                                };
                                for event_id in &events {
                                    match room.event(event_id, None).await {
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
                                                        process_with_claude(
                                                            text.body.clone(),
                                                            room_id_str.clone(),
                                                            room.clone(),
                                                            cc.clone(),
                                                            sessions.clone(),
                                                            sessions_path.clone(),
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
                                            pending
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
        Ok(())
    }

    pub async fn shutdown(&self) -> anyhow::Result<()> {
        let mut map = self.sessions.lock().await;
        // Drop all agent sessions to stop session actors before connection shutdown
        for s in map.values_mut() {
            s.agent_session = None;
        }
        persist_sessions(&map, &self.sessions_path);
        tracing::info!("Room sessions saved");
        drop(map);
        self.cc.lock().await.shutdown().await?;
        tracing::info!("Claude Code shut down");
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
    #[serde(skip)]
    pub agent_session: Option<Box<dyn AgentSession>>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("room_id", &self.room_id)
            .field("agent_session_id", &self.agent_session_id)
            .field("pwd", &self.pwd)
            .field("agent_session", &self.agent_session.as_ref().map(|_| "Some(...)"))
            .finish()
    }
}

impl Session {
    pub fn new(room_id: String) -> Self {
        Session {
            room_id,
            agent_session_id: None,
            pwd: Some(make_temp_dir()),
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

fn is_allowed(sender: &str, allowed: &HashSet<String>) -> bool {
    allowed.contains(sender)
}

fn handle_command(
    body: &str,
    room_id: &str,
    sessions: &mut HashMap<String, Session>,
    sessions_path: &Path,
) -> Option<String> {
    if !body.starts_with('/') {
        return None;
    }

    let cmd = body.trim();

    if cmd == "/help" {
        return Some(
            "Available commands:\n/help — Show this help\n/setpwd <path> — Set working directory for Claude Code\n/reset — Reset the Claude Code session"
                .into(),
        );
    }

    if cmd == "/reset" {
        if let Some(session) = sessions.get_mut(room_id) {
            session.agent_session = None;
            session.clear_agent_session_id();
        }
        persist_sessions(sessions, sessions_path);
        return Some("Session reset. A new session will be created on next message.".into());
    }

    if cmd == "/setpwd" {
        if let Some(session) = sessions.get(room_id) {
            match session.pwd() {
                Some(p) => return Some(format!("Working directory: {}", p.display())),
                None => return Some("No working directory set. Usage: /setpwd <path>".into()),
            }
        }
        return Some("No working directory set. Usage: /setpwd <path>".into());
    }

    if let Some(path_str) = cmd.strip_prefix("/setpwd ") {
        let path_str = path_str.trim();
        if path_str.is_empty() {
            return Some("Usage: /setpwd <path>".into());
        }
        match std::fs::canonicalize(path_str) {
            Ok(p) if p.is_dir() => {
                let session = sessions
                    .entry(room_id.to_owned())
                    .or_insert_with(|| Session::new(room_id.to_owned()));
                session.set_pwd(Some(p.clone()));
                persist_sessions(sessions, sessions_path);
                return Some(format!("Working directory set to: {}", p.display()));
            }
            Ok(_) => {
                return Some(format!("Not a directory: {path_str}"));
            }
            Err(e) => {
                return Some(format!("Invalid path: {path_str} ({e})"));
            }
        }
    }

    Some(format!(
        "Unknown command: {cmd}\nType /help for available commands"
    ))
}

#[allow(clippy::too_many_arguments)]
async fn process_with_claude(
    body: String,
    room_id: String,
    room: matrix_sdk::room::Room,
    cc: Arc<Mutex<AgentRegistry>>,
    sessions: Arc<Mutex<HashMap<String, Session>>>,
    sessions_path: PathBuf,
) {
    let mut agent_session: Box<dyn AgentSession> = {
        let mut smap = sessions.lock().await;
        match smap.get_mut(&room_id).and_then(|s| s.agent_session.take()) {
            Some(a) => a,
            None => {
                let pwd = match smap.get(&room_id) {
                    Some(s) => s.pwd.clone().unwrap_or_else(make_temp_dir),
                    None => {
                        let s = Session::new(room_id.clone());
                        let pwd = s.pwd.clone().unwrap_or_else(make_temp_dir);
                        save_session(&mut smap, &sessions_path, s);
                        tracing::info!(
                            "New session for room {}: pwd={}",
                            room_id,
                            pwd.display()
                        );
                        pwd
                    }
                };
                drop(smap);

                let mut cc = cc.lock().await;
                match cc.create_session(pwd).await {
                    Ok((sid, agent)) => {
                        let mut smap = sessions.lock().await;
                        if let Some(entry) = smap.get_mut(&room_id) {
                            entry.set_agent_session_id(sid);
                            persist_sessions(&smap, &sessions_path);
                        }
                        agent
                    }
                    Err(e) => {
                        tracing::error!("Failed to create ACP session: {e}");
                        let content = RoomMessageEventContent::text_plain(format!(
                            "Error creating session: {e}"
                        ));
                        let _ = room.send(content).await;
                        return;
                    }
                }
            }
        }
    };

    if let Err(e) = agent_session.send_input(&body) {
        tracing::error!("Failed to send input to Claude Code: {e}");
        let _ = room.typing_notice(false).await;
        let content = RoomMessageEventContent::text_plain(format!("Error: {e}"));
        let _ = room.send(content).await;
        if let Some(s) = sessions.lock().await.get_mut(&room_id) {
            s.agent_session = Some(agent_session);
        }
        return;
    }

    const THINKING: &str = "*Thinking*";
    let placeholder = RoomMessageEventContent::text_plain(THINKING);
    let maybe_event_id = match room.send(placeholder).await {
        Ok(r) => Some(r.event_id),
        Err(e) => {
            tracing::warn!("Failed to send placeholder, falling back to one-shot: {e}");
            None
        }
    };

    let mut full_output = String::new();
    let mut last_edit = Instant::now();
    loop {
        if room.typing_notice(true).await.is_err() {
            tracing::info!("Bot no longer in room {room_id}, stopping processing");
            let _ = room.typing_notice(false).await;
            break;
        }

        let deltas = agent_session.query_delta().unwrap_or_else(|e| {
            tracing::error!("Error querying Claude Code: {e}");
            vec![AgentDelta::Text {
                output: String::new(),
                done: true,
            }]
        });

        let mut done = false;
        for d in deltas {
            match d {
                AgentDelta::Text {
                    output,
                    done: is_done,
                } => {
                    if !output.is_empty() {
                        full_output.push_str(&output);
                    }
                    if is_done {
                        done = true;
                    }
                }
                AgentDelta::ToolCall { title } => {
                    let content = RoomMessageEventContent::text_plain(&format!("> **{title}**"));
                    let _ = room.send(content).await;
                }
            }
        }

        if let Some(ref event_id) = maybe_event_id {
            let should_edit =
                done || (!full_output.is_empty() && last_edit.elapsed() >= Duration::from_secs(2));

            if should_edit {
                let display = if done {
                    full_output.clone()
                } else {
                    format!("{}\n{THINKING}", full_output)
                };
                let edit = RoomMessageEventContent::text_plain(&display)
                    .make_replacement(ReplacementMetadata::new(event_id.clone(), None), None);
                if let Err(e) = room.send(edit).await {
                    tracing::warn!("Failed to edit message: {e}");
                }
                last_edit = Instant::now();
            }
        }

        if done {
            let _ = room.typing_notice(false).await;
            if maybe_event_id.is_none() && !full_output.is_empty() {
                let content = RoomMessageEventContent::text_plain(&full_output);
                if let Err(e) = room.send(content).await {
                    tracing::error!("Failed to send Claude response: {e}");
                }
            }
            break;
        }

        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    if let Some(s) = sessions.lock().await.get_mut(&room_id) {
        s.agent_session = Some(agent_session);
    }
}

async fn on_room_message(
    event: OriginalSyncRoomMessageEvent,
    room: matrix_sdk::room::Room,
    cc: Arc<Mutex<AgentRegistry>>,
    allowed: Arc<HashSet<String>>,
    sessions: Arc<Mutex<HashMap<String, Session>>>,
    sessions_path: PathBuf,
) {
    if *room.own_user_id() == event.sender {
        return;
    }

    if !is_allowed(event.sender.as_str(), &allowed) {
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
        let reply = {
            let mut sessions_map = sessions.lock().await;
            handle_command(
                &text_content.body,
                room.room_id().as_str(),
                &mut sessions_map,
                &sessions_path,
            )
        };
        if let Some(reply) = reply {
            let content = RoomMessageEventContent::text_plain(&reply);
            if let Err(e) = room.send(content).await {
                tracing::error!("Failed to send command reply: {e}");
            }
            return;
        }
    }

    let room_id = room.room_id().to_string();
    process_with_claude(
        text_content.body.clone(),
        room_id,
        room,
        cc,
        sessions,
        sessions_path,
    )
    .await;
}

async fn on_encrypted_message(
    event: OriginalSyncRoomEncryptedEvent,
    _room: matrix_sdk::room::Room,
    allowed: Arc<HashSet<String>>,
    pending_encrypted: Arc<Mutex<HashMap<String, Vec<OwnedEventId>>>>,
) {
    if *_room.own_user_id() == event.sender {
        return;
    }

    if !is_allowed(event.sender.as_str(), &allowed) {
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

    pending_encrypted
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
    cc: Arc<Mutex<AgentRegistry>>,
    allowed: Arc<HashSet<String>>,
    sessions: Arc<Mutex<HashMap<String, Session>>>,
    sessions_path: PathBuf,
    bot_id: String,
) {
    if event.state_key.as_str() != bot_id {
        return;
    }
    let inviter = event.sender.to_string();
    tracing::info!("Invite from {inviter} to room {}", room.room_id());

    if !is_allowed(&inviter, &allowed) {
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

    let map = sessions.lock().await;
    if map.contains_key(joined.room_id().as_str()) {
        return;
    }
    drop(map);

    let pwd = make_temp_dir();
    let result = cc.lock().await.create_session(pwd.clone()).await;

    let room_id = joined.room_id().to_string();
    let mut map = sessions.lock().await;
    match result {
        Ok((sid, agent)) => {
            let s = Session {
                room_id: room_id.clone(),
                agent_session_id: Some(sid),
                pwd: Some(pwd),
                agent_session: Some(agent),
            };
            tracing::info!("Created ACP session for room {room_id}");
            save_session(&mut map, &sessions_path, s);
        }
        Err(_) => {
            save_session(&mut map, &sessions_path, Session::new(room_id));
        }
    }
}

async fn on_member_change(
    event: OriginalSyncRoomMemberEvent,
    room: matrix_sdk::room::Room,
    sessions: Arc<Mutex<HashMap<String, Session>>>,
    sessions_path: PathBuf,
    bot_id: String,
) {
    let room_id = room.room_id().to_string();

    if event.state_key.as_str() == bot_id {
        if event.content.membership == MembershipState::Leave
            || event.content.membership == MembershipState::Ban
        {
            tracing::info!("Removed from room {room_id}, cleaning up session");
            let mut map = sessions.lock().await;
            map.remove(&room_id);
            persist_sessions(&map, &sessions_path);
        }
    } else if event.content.membership == MembershipState::Leave
        || event.content.membership == MembershipState::Ban
    {
        if let Ok(members) = room.members(RoomMemberships::JOIN).await {
            if members.len() <= 1 {
                tracing::info!("Only bot left in room {room_id}, leaving");
                let mut map = sessions.lock().await;
                map.remove(&room_id);
                persist_sessions(&map, &sessions_path);
                drop(map);
                if let Err(e) = room.leave().await {
                    tracing::error!("Failed to leave empty room {room_id}: {e}");
                }
            }
        }
    }
}
