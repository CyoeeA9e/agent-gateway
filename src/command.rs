use clap::Parser;

use crate::bot::Bot;
use crate::request::Request;

fn strip_prefix(content: &str) -> Option<&str> {
    if content == "/bot" {
        return Some("help");
    }
    content.strip_prefix("/bot ")
}

#[derive(Parser, Debug)]
#[command(disable_help_subcommand = true)]
pub enum Command {
    /// Show available commands
    Help,
    /// Reset the current agent session
    Reset,
    /// Start a new agent session
    New { agent: Option<String> },
    /// Show or set the working directory for the current session
    Pwd { path: Option<String> },
}

fn help_text() -> String {
    let mut text = String::from("Usage: /bot <COMMAND>\n\nCommands:\n");
    for variant in ["help", "reset", "new <agent>", "pwd <path>"] {
        let desc = match variant {
            "help" => "Show available commands",
            "reset" => "Reset the current agent session",
            "new <agent>" => "Start a new agent session",
            "pwd <path>" => "Show or set the working directory",
            _ => "",
        };
        text.push_str(&format!("  /bot {variant:<15} {desc}\n"));
    }
    text
}

pub async fn try_handle<T: Request>(
    req: &T,
    content: &str,
    bot: Option<&mut impl Bot>,
    conversation: &str,
) -> bool {
    let Some(cmd_str) = strip_prefix(content) else {
        return false;
    };

    let args: Vec<&str> = std::iter::once("bot")
        .chain(cmd_str.split_whitespace())
        .collect();

    let cmd = match Command::try_parse_from(&args) {
        Ok(c) => c,
        Err(e) => {
            req.resp(&format!(
                "{}\nType /bot help for available commands",
                e.to_string().lines().next().unwrap_or("Invalid command")
            ))
            .await;
            return true;
        }
    };

    match cmd {
        Command::Help => {
            req.resp(&help_text()).await;
        }
        Command::Reset => {
            req.resp("Session reset. A new session will be created on your next message.")
                .await;
        }
        Command::New { agent: None } => {
            req.resp("Usage: /bot new <agent>\nAvailable agents: claude, opencode").await;
        }
        Command::New {
            agent: Some(agent_name),
        } => {
            const SUPPORTED_AGENTS: &[&str] = &["claude", "opencode"];
            if !SUPPORTED_AGENTS.contains(&agent_name.as_str()) {
                req.resp(&format!(
                    "Unknown agent: {agent_name}. Available agents: {}",
                    SUPPORTED_AGENTS.join(", ")
                ))
                .await;
                return true;
            }
            if let Some(bot) = bot {
                bot.set_agent(conversation, &agent_name).await;
            }
            req.resp(&format!("Agent {agent_name} will be used for the next message. Send a message to start.")
            ).await;
        }
        Command::Pwd { path: None } => {
            if let Some(bot) = bot {
                let pwd = bot.get_pwd(conversation);
                req.resp(&format!("Working directory: {}", pwd.display())).await;
            } else {
                req.resp("Usage: /bot pwd <path>").await;
            }
        }
        Command::Pwd { path: Some(path) } => {
            let pwd = match std::fs::canonicalize(&path) {
                Ok(p) if p.is_dir() => p,
                Ok(_) => {
                    req.resp(&format!("Not a directory: {path}")).await;
                    return true;
                }
                Err(e) => {
                    req.resp(&format!("Invalid path: {path} ({e})")).await;
                    return true;
                }
            };
            if let Some(bot) = bot {
                bot.set_pwd(conversation, pwd.clone()).await;
            }
            req.resp(&format!("Working directory set to: {}", pwd.display()))
                .await;
        }
    }

    true
}
