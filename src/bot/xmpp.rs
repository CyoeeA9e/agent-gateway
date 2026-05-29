use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::agent::AgentSession;
use crate::bot::Bot;
use crate::request::BotStatus;

use xmpp::{
    ClientBuilder, ClientFeature, ClientType, Event, RoomNick,
    jid::{BareJid, Jid},
    message::send::RawMessageSettings,
    muc::room::JoinRoomSettings,
    parsers::{
        chatstates::ChatState, message::{Lang, MessageType},
        presence::{Presence as PresenceStanza, Type as PresenceType, Show},
    },
};

#[derive(Clone, Serialize, Deserialize)]
struct StoredSession {
    session_id: String,
    agent: String,
    #[serde(default = "default_pwd")]
    pwd: PathBuf,
}

fn default_pwd() -> PathBuf {
    PathBuf::from(".")
}

fn load_sessions(path: &std::path::Path) -> HashMap<String, StoredSession> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_sessions(path: &std::path::Path, sessions: &HashMap<String, StoredSession>) {
    if let Ok(json) = serde_json::to_string_pretty(sessions) {
        let _ = std::fs::write(path, json);
    }
}

pub struct XmppRequest {
    content: String,
    conversation: String,
    is_room: bool,
    client: Arc<Mutex<xmpp::Agent>>,
    status: BotStatus,
}

impl XmppRequest {
    fn new(
        content: String,
        conversation: String,
        is_room: bool,
        client: Arc<Mutex<xmpp::Agent>>,
    ) -> Self {
        XmppRequest {
            content,
            conversation,
            is_room,
            client,
            status: BotStatus::Active,
        }
    }

}

#[async_trait]
impl crate::request::Request for XmppRequest {
    fn get_content(&self) -> &str {
        &self.content
    }

    fn conversation(&self) -> &str {
        &self.conversation
    }

    async fn resp(&self, text: &str) {
        let Ok(jid) = BareJid::from_str(&self.conversation) else {
            return;
        };
        let msg_type = if self.is_room {
            MessageType::Groupchat
        } else {
            MessageType::Chat
        };
        let mut guard = self.client.lock().await;
        guard
            .send_raw_message(RawMessageSettings::new(Jid::from(jid), msg_type, text))
            .await;
    }

    async fn set_status(&mut self, status: BotStatus) {
        let Ok(jid) = BareJid::from_str(&self.conversation) else {
            return;
        };
        self.status = status;
        let state = match self.status {
            BotStatus::Composing => ChatState::Composing,
            BotStatus::Active => ChatState::Active,
        };
        let msg_type = if self.is_room {
            MessageType::Groupchat
        } else {
            MessageType::Chat
        };
        let mut guard = self.client.lock().await;
        guard
            .send_raw_message(
                RawMessageSettings::new(Jid::from(jid), msg_type, "")
                    .with_payload(state),
            )
            .await;
    }
}

pub struct XmppBotBuilder {
    jid: Option<String>,
    password: Option<String>,
    nick: Option<String>,
    rooms: Vec<String>,
    state_dir: Option<PathBuf>,
}

impl XmppBotBuilder {
    pub fn new() -> Self {
        XmppBotBuilder {
            jid: None,
            password: None,
            nick: None,
            rooms: Vec::new(),
            state_dir: None,
        }
    }

    pub fn jid(mut self, jid: String) -> Self {
        self.jid = Some(jid);
        self
    }

    pub fn password(mut self, password: String) -> Self {
        self.password = Some(password);
        self
    }

    pub fn nick(mut self, nick: Option<String>) -> Self {
        self.nick = nick;
        self
    }

    pub fn rooms(mut self, rooms: Option<Vec<String>>) -> Self {
        self.rooms = rooms.unwrap_or_default();
        self
    }

    pub fn state_dir(mut self, dir: PathBuf) -> Self {
        self.state_dir = Some(dir);
        self
    }

    pub fn build(self) -> XmppBot {
        let jid = self.jid.expect("jid is required");
        let password = self.password.expect("password is required");

        let bare_jid = BareJid::from_str(&jid).unwrap_or_else(|_| panic!("Invalid JID: {jid}"));

        let nick_str = self
            .nick
            .or_else(|| bare_jid.node().map(|n| n.to_string()))
            .unwrap_or_else(|| "bot".into());

        let nick = RoomNick::from_str(&nick_str)
            .unwrap_or_else(|_| panic!("Invalid room nick: {nick_str}"));

        let agent = ClientBuilder::new(bare_jid, &password)
            .set_client(ClientType::Bot, "agentbot")
            .set_default_nick(nick)
            .enable_feature(ClientFeature::ContactList)
            .enable_feature(ClientFeature::JoinRooms)
            .build();

        let client = Arc::new(Mutex::new(agent));

        let state_dir = self.state_dir.unwrap_or_else(|| {
            std::env::var("STATE_DIRECTORY")
                .or_else(|_| std::env::var("STATE_DIR"))
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("state"))
        });
        let _ = std::fs::create_dir_all(&state_dir);
        let sessions_path = state_dir.join("sessions.json");
        XmppBot {
            rooms: self.rooms,
            client,
            nick_str,
            sessions: HashMap::new(),
            state_dir,
            sessions_path,
        }
    }
}

pub struct XmppBot {
    rooms: Vec<String>,
    client: Arc<Mutex<xmpp::Agent>>,
    nick_str: String,
    sessions: HashMap<String, Arc<dyn AgentSession>>,
    state_dir: PathBuf,
    sessions_path: PathBuf,
}

impl XmppBot {
    pub fn builder() -> XmppBotBuilder {
        XmppBotBuilder::new()
    }

    async fn get_or_resume_session(&mut self, conv_id: &str) -> Option<Arc<dyn AgentSession>> {
        if let Some(session) = self.sessions.get(conv_id) {
            return Some(session.clone());
        }
        let mut stored = load_sessions(&self.sessions_path);
        let entry = stored.get(conv_id)?;
        let pwd = entry.pwd.clone();
        let agent = entry.agent.clone();
        let session_id = entry.session_id.clone();
        let result: Option<Arc<dyn AgentSession>> = if session_id.is_empty() {
            if agent.is_empty() {
                None
            } else {
                let result = match agent.as_str() {
                    "claude" => crate::agent::claude::create_session(pwd.clone()).await,
                    "opencode" => crate::agent::opencode::create_session(pwd.clone()).await,
                    other => {
                        tracing::warn!("Unknown agent '{other}' for {conv_id}");
                        return None;
                    }
                };
                match result {
                    Ok(session) => {
                        let sid = session.session_id().to_owned();
                        let s: Arc<dyn AgentSession> = Arc::new(session);
                        self.sessions.insert(conv_id.to_owned(), s.clone());
                        stored.insert(
                            conv_id.to_owned(),
                            StoredSession {
                                session_id: sid,
                                agent,
                                pwd,
                            },
                        );
                        save_sessions(&self.sessions_path, &stored);
                        tracing::info!("Created lazy session for {conv_id}");
                        Some(s)
                    }
                    Err(e) => {
                        tracing::warn!("Failed to create session for {conv_id}: {e}");
                        None
                    }
                }
            }
        } else {
            let result = match agent.as_str() {
                "claude" => crate::agent::claude::resume_session(session_id, pwd).await,
                "opencode" => crate::agent::opencode::resume_session(session_id, pwd).await,
                other => {
                    tracing::warn!("Unknown agent '{other}' for {conv_id}");
                    stored.remove(conv_id);
                    save_sessions(&self.sessions_path, &stored);
                    return None;
                }
            };
            match result {
                Ok(session) => {
                    let s: Arc<dyn AgentSession> = Arc::new(session);
                    self.sessions.insert(conv_id.to_owned(), s.clone());
                    tracing::info!("Resumed session for {conv_id}");
                    Some(s)
                }
                Err(e) => {
                    tracing::warn!("Failed to resume session for {conv_id}: {e}");
                    stored.remove(conv_id);
                    save_sessions(&self.sessions_path, &stored);
                    None
                }
            }
        };
        result
    }

    pub async fn listen_msg(&mut self) -> (XmppRequest, Option<Arc<dyn AgentSession>>) {
        loop {
            let events = {
                let mut guard = self.client.lock().await;
                match tokio::time::timeout(Duration::from_millis(200), guard.wait_for_events())
                    .await
                {
                    Ok(events) => events,
                    Err(_) => continue,
                }
            };
            for event in events {
                match event {
                    Event::Online => {
                        tracing::info!("Online.");
                        let mut guard = self.client.lock().await;
                        let mut p = PresenceStanza::new(PresenceType::None);
                        p.show = Some(Show::Chat);
                        p.set_status(Lang::from("en"), "Agent bot ready");
                        let _ = guard.send_stanza(p).await;
                        for room_str in &self.rooms {
                            if let Ok(room_jid) = BareJid::from_str(room_str) {
                                tracing::info!("Joining room {room_str}...");
                                guard.join_room(JoinRoomSettings::new(room_jid)).await;
                            } else {
                                tracing::error!("Invalid room JID: {room_str}");
                            }
                        }
                    }
                    Event::Disconnected(reason) => {
                        tracing::info!("Disconnected: {reason}");
                    }
                    Event::ChatMessage(_id, jid, body, _time_info) => {
                        tracing::info!("DM from {jid}: {body}");
                        let req =
                            XmppRequest::new(body, jid.to_string(), false, self.client.clone());
                        let session = self.get_or_resume_session(&req.conversation).await;
                        return (req, session);
                    }
                    Event::RoomMessage(_id, room_jid, sender_nick, body, _time_info) => {
                        if sender_nick.to_string() == self.nick_str {
                            continue;
                        }
                        if let Some(prompt) = parse_mention(&body, &self.nick_str) {
                            tracing::info!("Mention in {room_jid} from {sender_nick}: {prompt}");
                            let req = XmppRequest::new(
                                prompt,
                                room_jid.to_string(),
                                true,
                                self.client.clone(),
                            );
                            let session = self.get_or_resume_session(&req.conversation).await;
                            return (req, session);
                        }
                    }
                    Event::RoomJoined(jid) => {
                        tracing::info!("Joined room {jid}");
                    }
                    Event::Presence(presence) => match presence.type_ {
                        PresenceType::Subscribe => {
                            tracing::info!("Presence subscribe from {:?}", presence.from);
                            if let Some(from) = presence.from {
                                let mut guard = self.client.lock().await;
                                let _ = guard
                                    .send_stanza(PresenceStanza::subscribed().with_to(from.clone()))
                                    .await;
                                let _ = guard
                                    .send_stanza(
                                        PresenceStanza::available()
                                            .with_to(from)
                                            .with_show(Show::Chat),
                                    )
                                    .await;
                            }
                        }
                        PresenceType::Unsubscribe | PresenceType::Unsubscribed => {
                            tracing::info!("Presence unsubscribe from {:?}", presence.from);
                            if let Some(from) = presence.from {
                                let mut guard = self.client.lock().await;
                                let _ = guard
                                    .send_stanza(
                                        PresenceStanza::new(PresenceType::Unsubscribed)
                                            .with_to(from),
                                    )
                                    .await;
                            }
                        }
                        PresenceType::Probe => {
                            tracing::debug!("Presence probe from {:?}", presence.from);
                            if let Some(from) = presence.from {
                                let mut guard = self.client.lock().await;
                                let _ = guard
                                    .send_stanza(
                                        PresenceStanza::available()
                                            .with_to(from)
                                            .with_show(Show::Chat),
                                    )
                                    .await;
                            }
                        }
                        _ => {
                            tracing::debug!(
                                "Unhandled presence: {:?} ({:?})",
                                presence.type_,
                                presence.from
                            );
                        }
                    },
                    _ => {
                        tracing::debug!("Unhandled event: {event:?}");
                    }
                }
            }
        }
    }

    pub async fn shutdown(&mut self) {
        tracing::info!("Shutting down...");
    }
}

#[async_trait]
impl Bot for XmppBot {
    fn get_pwd(&self, conv_id: &str) -> std::path::PathBuf {
        let stored = load_sessions(&self.sessions_path);
        let pwd = stored.get(conv_id).map(|s| s.pwd.clone()).unwrap_or_else(default_pwd);
        std::fs::canonicalize(&pwd).unwrap_or(pwd)
    }

    async fn set_pwd(&mut self, conv_id: &str, pwd: std::path::PathBuf) {
        let mut stored = load_sessions(&self.sessions_path);
        stored.entry(conv_id.to_owned()).or_insert_with(|| StoredSession {
            session_id: String::new(),
            agent: String::new(),
            pwd: default_pwd(),
        }).pwd = pwd.clone();
        save_sessions(&self.sessions_path, &stored);
    }

    async fn set_agent(&mut self, conv_id: &str, agent: &str) {
        let mut stored = load_sessions(&self.sessions_path);
        stored.entry(conv_id.to_owned()).or_insert_with(|| StoredSession {
            session_id: String::new(),
            agent: String::new(),
            pwd: default_pwd(),
        }).agent = agent.to_owned();
        save_sessions(&self.sessions_path, &stored);
    }

    async fn handle_command<T: crate::request::Request + Sync>(&mut self, req: &T) -> bool {
        crate::command::try_handle(req, req.get_content(), Some(self), req.conversation()).await
    }
}

fn parse_mention(text: &str, nick: &str) -> Option<String> {
    let trimmed = text
        .strip_prefix(&format!("@{nick}"))
        .or_else(|| text.strip_prefix(&format!("{nick}:")))
        .or_else(|| text.strip_prefix(&format!("{nick},")))
        .or_else(|| text.strip_prefix(&format!("{nick} ")))
        .map(|s| s.trim())
        .and_then(|s| {
            if s.is_empty() {
                None
            } else {
                Some(s.to_owned())
            }
        });
    if trimmed.is_some() {
        return trimmed;
    }
    let lower = text.to_ascii_lowercase();
    let lower_nick = nick.to_ascii_lowercase();
    let rest = lower
        .strip_prefix(&format!("@{lower_nick}"))
        .or_else(|| lower.strip_prefix(&format!("{lower_nick}:")))
        .or_else(|| lower.strip_prefix(&format!("{lower_nick},")))
        .or_else(|| lower.strip_prefix(&format!("{lower_nick} ")))?;
    let prefix_len = text.len() - rest.len();
    let trimmed = text[prefix_len..].trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}
