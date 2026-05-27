use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct XmppConfig {
    pub jid: String,
    pub password: String,
    pub nick: Option<String>,
    pub rooms: Option<Vec<String>>,
}
