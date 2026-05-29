pub mod xmpp;

use async_trait::async_trait;

use crate::request::Request;

#[async_trait]
pub trait Bot {
    fn get_pwd(&self, conv_id: &str) -> std::path::PathBuf;
    async fn set_pwd(&mut self, conv_id: &str, pwd: std::path::PathBuf);
    async fn set_agent(&mut self, conv_id: &str, agent: &str);
    async fn handle_command<T: Request + Sync>(&mut self, req: &T) -> bool;
}
