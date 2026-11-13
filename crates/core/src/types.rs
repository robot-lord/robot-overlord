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

    /// How a status is shown to people (the stored values stay as they
    /// are). Each means one thing: `needs you` is blocked mid-turn on the
    /// user (a permission prompt, a question); `your turn` is a finished
    /// turn waiting for the next prompt.
    pub fn label(self) -> &'static str {
        match self {
            Status::Starting => "starting",
            Status::Working => "working",
            Status::WaitingInput => "needs you",
            Status::Idle => "your turn",
            Status::Stopped => "stopped",
            Status::Error => "error",
        }
    }

    /// The label for a stored status value (unknown ones as they are).
    pub fn label_of(stored: &str) -> &str {
        match stored {
            "waiting_input" => Status::WaitingInput.label(),
            "idle" => Status::Idle.label(),
            other => other,
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
    pub repo_path: Option<String>,
    pub workspace_path: Option<String>,
    pub branch: Option<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub notes: String,
    /// Model from launch_spec, when one was pinned.
    pub model: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Sub-agent counts (filled by list/get).
    #[serde(default)]
    pub agents_running: i64,
    #[serde(default)]
    pub agents_total: i64,
    /// '' for ordinary sessions, 'overlord' for the management session.
    #[serde(default)]
    pub role: String,
    /// What it did, in a paragraph (made when it's archived; see
    /// `rlord_daemon::summary`).
    #[serde(default)]
    pub summary: String,
    pub summary_at: Option<i64>,
    /// When rlord itself last typed a prompt into it (`send_to_session`,
    /// a resume after a usage limit, `overlord ask`). `last_user_msg_at`
    /// is the user's own prompts only.
    #[serde(default)]
    pub last_rlord_msg_at: Option<i64>,
}

impl Session {
    /// The last prompt it got, whoever typed it: the user or rlord.
    pub fn last_prompt_at(&self) -> Option<i64> {
        self.last_user_msg_at.max(self.last_rlord_msg_at)
    }
}

/// What rlord keeps of a prompt's text to recognise it again: never the
/// text itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptMark {
    /// `PromptMark::hash_of` the text.
    pub hash: String,
    /// The agent may have changed the text between the paste and its
    /// report (a `[Pasted text #1 +30 lines]` placeholder left in it, or a
    /// text long enough to be cut or stored aside), so the hash can miss.
    pub pasted: bool,
}

impl PromptMark {
    /// Texts this long (characters) count as `pasted`.
    pub const LONG_CHARS: usize = 1000;

    pub fn of(text: &str) -> Self {
        PromptMark {
            hash: Self::hash_of(text),
            pasted: text.contains("[Pasted text #") || text.chars().count() >= Self::LONG_CHARS,
        }
    }

    /// A hash of a prompt's text, the same for what rlord pasted and what
    /// the agent reports of it: whitespace runs count as one space (a
    /// paste may change line ends), the ends are trimmed. FNV-1a, written
    /// out: the hook client and the daemon must agree across versions.
    pub fn hash_of(text: &str) -> String {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut eat = |b: u8| {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        };
        for (i, word) in text.split_whitespace().enumerate() {
            if i > 0 {
                eat(b' ');
            }
            word.bytes().for_each(&mut eat);
        }
        format!("{h:016x}")
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn new_id() -> String {
    ulid::Ulid::generate().to_string().to_lowercase()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subagent {
    pub agent_id: String,
    pub session_id: String,
    pub agent_type: String,
    pub description: String,
    pub tool_use_id: Option<String>,
    pub spawn_depth: i64,
    pub background: bool,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub transcript_path: Option<String>,
    pub result: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
}
