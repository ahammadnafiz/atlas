//! **Atlas artifacts** — the wire shapes that leave the machine.
//!
//! One artifact is one row on its way to the Organisation. The shapes mirror
//! the local tables deliberately, so the drain is a copy rather than a
//! translation, and every artifact carries the two fields the server dedupes
//! on: a monotonic `seq` and a `contentHash`.
//!
//! # What is deliberately absent
//!
//! **There is no author field.** Authorship is stamped by the ingest worker
//! from the verified token subject, never read from the payload. The desktop is
//! untrusted, so authorship must be derived rather than declared — otherwise a
//! modified client could push Sessions as a colleague, and a shared timeline
//! that can be forged is worse than no timeline.
//!
//! **Large payloads are not inlined.** A body over the spill threshold travels
//! as a blob reference; the blob itself is uploaded first. The measured corpus
//! has a single 2.02 MB message, already past the server's per-row cap.

use serde::{Deserialize, Serialize};

/// One row on its way to the Organisation.
///
/// Tagged by `type`, matching the server's discriminated union.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AtlasArtifact {
    AgentSession(SessionArtifact),
    AgentMessage(MessageArtifact),
    ToolCall(ToolCallArtifact),
    Checkpoint(CheckpointArtifact),
}

impl AtlasArtifact {
    /// The local row this came from, so the drain can mark exactly what the
    /// server acknowledged rather than assuming a whole batch succeeded.
    pub fn row_id(&self) -> &str {
        match self {
            Self::AgentSession(a) => &a.base.row_id,
            Self::AgentMessage(a) => &a.base.row_id,
            Self::ToolCall(a) => &a.base.row_id,
            Self::Checkpoint(a) => &a.base.row_id,
        }
    }

    pub fn seq(&self) -> i64 {
        match self {
            Self::AgentSession(a) => a.base.seq,
            Self::AgentMessage(a) => a.base.seq,
            Self::ToolCall(a) => a.base.seq,
            Self::Checkpoint(a) => a.base.seq,
        }
    }

    /// Blob keys this artifact references.
    ///
    /// The drain uploads these **before** sending the artifact, so a row never
    /// lands pointing at a payload that is not there.
    pub fn blob_refs(&self) -> Vec<&str> {
        match self {
            Self::AgentMessage(a) => a.body_ref.iter().map(String::as_str).collect(),
            Self::ToolCall(a) => a
                .arguments_ref
                .iter()
                .chain(a.result_ref.iter())
                .map(String::as_str)
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Approximate wire size, for the byte ceiling on a batch.
    ///
    /// A count limit alone is not enough: a hundred artifacts can be enormous,
    /// and the server enforces a request byte cap.
    pub fn approx_bytes(&self) -> usize {
        serde_json::to_string(self).map(|s| s.len()).unwrap_or(0)
    }
}

/// Fields every artifact carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactBase {
    /// Local row id. Echoed back in a per-artifact result so the drain knows
    /// precisely which rows were accepted.
    pub row_id: String,
    pub org_id: String,
    pub workspace_id: String,
    /// Monotonic per-store sequence — half of the server's dedupe key.
    pub seq: i64,
    /// The other half. A replayed batch is recognised rather than duplicated.
    pub content_hash: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionArtifact {
    #[serde(flatten)]
    pub base: ArtifactBase,
    pub session_id: String,
    pub source: String,
    pub native_session_id: String,
    pub title: Option<String>,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub token_totals: serde_json::Value,
    pub started_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageArtifact {
    #[serde(flatten)]
    pub base: ArtifactBase,
    pub session_id: String,
    pub turn_seq: i64,
    pub role: String,
    pub mode: String,
    pub preview: String,
    /// Inline when small.
    pub body: Option<String>,
    /// Blob key when spilled. Uploaded before this artifact is sent.
    pub body_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallArtifact {
    #[serde(flatten)]
    pub base: ArtifactBase,
    pub session_id: String,
    pub turn_seq: i64,
    /// Canonical, derived — what the sidebar groups by.
    pub tool_name: String,
    pub title: Option<String>,
    pub kind: Option<String>,
    pub status: String,
    pub locations: serde_json::Value,
    pub arguments: Option<String>,
    pub arguments_ref: Option<String>,
    pub result: Option<String>,
    pub result_ref: Option<String>,
    pub result_binary: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointArtifact {
    #[serde(flatten)]
    pub base: ArtifactBase,
    pub session_id: String,
    /// With `session_id`, the Checkpoint's natural key — and the server's
    /// idempotency key for it.
    pub commit_sha: String,
    pub patch_id: Option<String>,
    pub link_state: String,
    pub branch: Option<String>,
    /// Display-only. Not an identity claim: git emails are self-asserted.
    pub git_author_name: Option<String>,
    pub git_author_email: Option<String>,
    pub files_touched: Vec<String>,
    pub insertions: i64,
    pub deletions: i64,
}

/// One push.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IngestRequest {
    pub artifacts: Vec<AtlasArtifact>,
}

/// What the server says happened to each artifact.
///
/// **Per-artifact, not per-batch.** The outbox is a per-row state machine, so a
/// batch-level verdict cannot express its outcomes: one malformed row must be
/// markable as failed while everything behind it keeps draining. Today's server
/// returns a single 403 for a whole batch if any org check fails, which is why
/// the drain also carries a bisect fallback — see [`crate::sync`].
///
/// An empty `results` body means the whole batch was accepted — the 202
/// contract. A **non-empty** body that omits a row is *not* acceptance: the
/// drain treats the omitted row as unacknowledged and leaves it pending,
/// because a server that silently dropped a row must not have it marked sent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IngestResponse {
    #[serde(default)]
    pub results: Vec<ArtifactResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactResult {
    pub row_id: String,
    pub accepted: bool,
    /// Why it was rejected. Present only when `accepted` is false.
    #[serde(default)]
    pub reason: Option<String>,
}
