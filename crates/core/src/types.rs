use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Starting,
    Working,
    WaitingInput,
    Idle,
    Stopped,
    Error,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Starting => "starting",
            Status::Working => "working",
            Status::WaitingInput => "waiting_input",
            Status::Idle => "idle",
            Status::Stopped => "stopped",
            Status::Error => "error",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub name: String,
    pub name_locked: bool,
    pub agent: String,
    pub agent_session: Option<String>,
    pub agent_session_trust: String,
    pub project_path: String,
    pub tmux_session: Option<String>,
    pub status: Status,
    pub group_path: String,
    pub pinned: bool,
    pub created_at: i64,
    pub archived_at: Option<i64>,
    pub last_user_msg_at: Option<i64>,
    pub last_agent_msg_at: Option<i64>,
    pub last_state_change_at: Option<i64>,
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn new_id() -> String {
    ulid::Ulid::new().to_string().to_lowercase()
}
