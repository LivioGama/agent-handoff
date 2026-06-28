use serde::Serialize;
use std::path::PathBuf;
use std::time::SystemTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Agent {
    ClaudeCode,
    Codex,
    OpenCode,
    Cursor,
    Devin,
}

impl Agent {
    pub fn label(&self) -> &'static str {
        match self {
            Agent::ClaudeCode => "claude",
            Agent::Codex => "codex",
            Agent::OpenCode => "opencode",
            Agent::Cursor => "cursor",
            Agent::Devin => "devin",
        }
    }

    /// Parse a `--agent` filter value. Accepts a few aliases.
    pub fn parse(s: &str) -> Option<Agent> {
        match s.to_ascii_lowercase().as_str() {
            "claude" | "claude-code" | "cc" => Some(Agent::ClaudeCode),
            "codex" => Some(Agent::Codex),
            "opencode" | "oc" => Some(Agent::OpenCode),
            "cursor" => Some(Agent::Cursor),
            "devin" => Some(Agent::Devin),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

impl Role {
    pub fn label(&self) -> &'static str {
        match self {
            Role::User => "User",
            Role::Assistant => "Assistant",
        }
    }
}

/// A discovered session, with a cheap fingerprint used to identify which terminal it belongs to.
#[derive(Debug, Clone, Serialize)]
pub struct SessionRef {
    pub agent: Agent,
    pub id: String,
    #[serde(skip)]
    pub path: PathBuf,
    pub cwd: String,
    pub branch: Option<String>,
    #[serde(skip)]
    pub modified: SystemTime,
    pub prompts: usize,
    pub first: Option<String>,
    pub last: Option<String>,
}

/// One conversation turn after content-block flattening.
#[derive(Debug, Clone, Serialize)]
pub struct Turn {
    pub role: Role,
    pub text: String,
}
