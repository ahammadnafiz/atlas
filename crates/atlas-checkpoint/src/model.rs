//! The recorded shapes.
//!
//! These mirror the eventual server tables deliberately, so syncing is a copy
//! rather than a translation. Note `agent_session` rather than `session`: the
//! server's `session` table belongs to Better Auth and must not be reused.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Where a Session came from.
///
/// This is half of the identity constraint, and it exists because Atlas's
/// ACP-hosted agents *also* write their own transcripts to disk. Without the
/// discriminator, the importer would capture every in-app Session a second time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// An ACP-hosted agent running inside Atlas (Claude Code, Codex).
    Acp,
    /// The native agent.
    Cersei,
    /// Read back from an agent's own on-disk transcript, live or historical.
    ExternalJsonl,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Acp => "acp",
            Self::Cersei => "cersei",
            Self::ExternalJsonl => "external_jsonl",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "acp" => Some(Self::Acp),
            "cersei" => Some(Self::Cersei),
            "external_jsonl" => Some(Self::ExternalJsonl),
            _ => None,
        }
    }

    /// Was this Session observed live, with write-time file hashes?
    ///
    /// The link rule needs `existed_before` captured at write time, which an
    /// imported transcript cannot supply — so this is what decides whether a
    /// Session is eligible for Checkpoints at all.
    pub fn is_live(self) -> bool {
        matches!(self, Self::Acp | Self::Cersei)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    System,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::System => "system",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "user" => Some(Self::User),
            "assistant" => Some(Self::Assistant),
            "system" => Some(Self::System),
            _ => None,
        }
    }
}

/// What kind of content a Message carries.
///
/// A column rather than something derived at read time, because the session
/// detail sidebar counts Prompts, Responses and Intermediate steps directly off
/// `role` and `mode` — and doing that by scanning bodies would not scale past a
/// long Session, let alone a board-level filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Text,
    Tool,
    Thinking,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Tool => "tool",
            Self::Thinking => "thinking",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "text" => Some(Self::Text),
            "tool" => Some(Self::Tool),
            "thinking" => Some(Self::Thinking),
            _ => None,
        }
    }
}

/// Where a row is on its way to the Organisation.
///
/// The outbox is this column, not a separate queue — which is what makes Local
/// mode "the same database with draining disabled" rather than a second code
/// path to keep correct.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncState {
    /// Local Workspace, or not yet eligible. Parks here forever in Local mode.
    Local,
    /// Queued for the drain.
    Pending,
    /// The server acknowledged it durably.
    Sent,
    /// Repeatedly rejected. Skipped rather than retried at the head of the
    /// queue, so one poison row never stalls everything behind it.
    Failed,
}

impl SyncState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Pending => "pending",
            Self::Sent => "sent",
            Self::Failed => "failed",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "local" => Some(Self::Local),
            "pending" => Some(Self::Pending),
            "sent" => Some(Self::Sent),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// How a Workspace treats what it captures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceMode {
    /// Never drains. A complete mode, not a buffer.
    Local,
    /// Drains to the Organisation.
    Cloud,
}

impl WorkspaceMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Cloud => "cloud",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "local" => Some(Self::Local),
            "cloud" => Some(Self::Cloud),
            _ => None,
        }
    }

    /// The state a freshly-written row starts in under this mode.
    pub fn initial_sync_state(self) -> SyncState {
        match self {
            Self::Local => SyncState::Local,
            Self::Cloud => SyncState::Pending,
        }
    }
}

/// A turn's lifecycle, so an agent that died mid-turn is distinguishable from
/// one that finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnState {
    /// Started, no end seen yet. Either in flight or abandoned — which of the
    /// two is only knowable once the process restarts.
    Open,
    Completed,
    /// Was open when the store was last closed. Reconciled on next open.
    Aborted,
}

impl TurnState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Completed => "completed",
            Self::Aborted => "aborted",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "open" => Some(Self::Open),
            "completed" => Some(Self::Completed),
            "aborted" => Some(Self::Aborted),
            _ => None,
        }
    }
}

/// Token accounting for a Session.
///
/// Agent-dependent by nature: only the native agent reports a real input/output
/// split. ACP agents surface a context-window gauge instead, which is a
/// different measurement and is kept in different fields so it can never be
/// rendered as a usage split. The accurate ACP figures are backfilled from the
/// agent's own transcript.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenTotals {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    /// Context-window occupancy, for agents that only report that. Never
    /// presented as an input/output split.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_used: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_size: Option<u64>,
}

impl TokenTotals {
    /// Does this carry a genuine input/output split, as opposed to only a
    /// context gauge?
    pub fn has_usage_split(&self) -> bool {
        self.input_tokens > 0 || self.output_tokens > 0
    }
}

/// A recorded Session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub workspace_id: String,
    pub source: Source,
    /// The agent's own id for this conversation — the half of the identity
    /// constraint that makes a re-import a no-op.
    pub native_session_id: String,
    /// First line of the first user prompt, bounded and **redacted**. The most
    /// visible string in the product: it renders on the shared Organisation
    /// board, and prompts routinely contain pasted keys.
    pub title: Option<String>,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub cwd: Option<String>,
    pub token_totals: TokenTotals,
    /// Reserved for a later opt-in enrichment pass. Stays null.
    pub summary: Option<String>,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Something went wrong recording this Session and a human should know.
    pub needs_attention: bool,
    pub attention_reason: Option<String>,
    /// Cumulative redaction tally, as `{category: count}`. Drives the
    /// "N secrets redacted" figure on the bulk-disclosure confirmations.
    pub redaction_counts: serde_json::Value,
    pub sync_state: SyncState,
}

/// One finalized turn's worth of content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub session_id: String,
    /// Monotonic across the whole store, so the drain has a total order and
    /// per-session reads are still correctly ordered.
    pub seq: i64,
    pub turn_seq: i64,
    pub role: Role,
    pub mode: Mode,
    /// First 2 KB, always inline — what a list renders without touching a blob.
    pub preview: String,
    /// The body, when it was small enough to keep on the row.
    pub body: Option<String>,
    /// Blob key, when the body was spilled instead.
    pub body_ref: Option<String>,
    pub body_bytes: i64,
    pub content_hash: String,
    pub created_at: DateTime<Utc>,
    pub sync_state: SyncState,
}

impl Message {
    /// Was this body spilled to a blob rather than kept on the row?
    pub fn is_spilled(&self) -> bool {
        self.body_ref.is_some()
    }
}
