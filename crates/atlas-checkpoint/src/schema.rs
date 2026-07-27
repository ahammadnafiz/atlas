//! Schema and migrations.
//!
//! Atlas has never created a SQLite database before — the one existing rusqlite
//! call site reads Codex's own store read-only — so the versioning policy is
//! established here. It is deliberately the simplest thing that can survive a
//! downgrade: an integer in `user_version`, forward-only migrations, and a hard
//! refusal to touch a database written by a newer build. Silently operating on a
//! schema you do not understand is how you corrupt the record you exist to keep.
//!
//! Indexes are part of the schema rather than an afterthought. The two queries
//! that must never full-scan are the outbox drain and the ordered read behind
//! the session-detail sidebar, and both are covered below.

use rusqlite::Connection;

use crate::error::{Error, Result};

/// Bump when adding a migration, and add the matching arm in [`migrate`].
pub const SCHEMA_VERSION: i64 = 2;

pub fn migrate(conn: &Connection) -> Result<()> {
    let found: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;

    if found > SCHEMA_VERSION {
        return Err(Error::SchemaTooNew {
            found,
            supported: SCHEMA_VERSION,
        });
    }
    if found == SCHEMA_VERSION {
        return Ok(());
    }

    if found < 1 {
        conn.execute_batch(V1)?;
    }
    if found < 2 {
        conn.execute_batch(V2)?;
    }

    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

const V1: &str = r#"
-- One row per Session. Named `agent_session`, never `session`: the server's
-- `session` table belongs to Better Auth, and the local schema mirrors the
-- server shape so that syncing is a copy rather than a translation.
CREATE TABLE IF NOT EXISTS agent_session (
    id                TEXT PRIMARY KEY,
    workspace_id      TEXT NOT NULL,
    source            TEXT NOT NULL,
    native_session_id TEXT NOT NULL,
    title             TEXT,
    agent             TEXT,
    model             TEXT,
    cwd               TEXT,
    token_totals      TEXT NOT NULL DEFAULT '{}',
    summary           TEXT,
    started_at        TEXT NOT NULL,
    updated_at        TEXT NOT NULL,
    needs_attention   INTEGER NOT NULL DEFAULT 0,
    attention_reason  TEXT,
    redaction_counts  TEXT NOT NULL DEFAULT '{}',
    sync_state        TEXT NOT NULL DEFAULT 'local',
    sync_attempts     INTEGER NOT NULL DEFAULT 0,

    -- Identity. Dedupes a re-import *within* a source; deliberately NOT across
    -- sources, because Atlas's ACP-hosted agents also write their own JSONL and
    -- ('acp', id) / ('external_jsonl', id) are both legitimate rows. Skipping
    -- the cross-source duplicate is the importer's explicit job.
    UNIQUE (workspace_id, source, native_session_id)
);

-- One row per finalized turn. The only place transcript text lives — a
-- Checkpoint carries none of its own, so there is exactly one copy to redact,
-- one to sync, and one to keep consistent.
CREATE TABLE IF NOT EXISTS agent_message (
    id                TEXT PRIMARY KEY,
    session_id        TEXT NOT NULL REFERENCES agent_session(id) ON DELETE CASCADE,
    seq               INTEGER NOT NULL,
    turn_seq          INTEGER NOT NULL,
    -- The agent's own id for this message, when it has one. This is what makes
    -- re-processing a turn idempotent rather than duplicating it.
    native_message_id TEXT,
    role              TEXT NOT NULL,
    mode              TEXT NOT NULL,
    preview           TEXT NOT NULL,
    body              TEXT,
    body_ref          TEXT,
    body_bytes        INTEGER NOT NULL DEFAULT 0,
    content_hash      TEXT NOT NULL,
    created_at        TEXT NOT NULL,
    sync_state        TEXT NOT NULL DEFAULT 'local',
    sync_attempts     INTEGER NOT NULL DEFAULT 0,

    UNIQUE (session_id, seq)
);

-- Turn lifecycle, so an agent that died mid-turn is distinguishable from one
-- that finished. Without this an abandoned turn's rows are indistinguishable
-- from a completed turn's, and the record quietly asserts something false.
CREATE TABLE IF NOT EXISTS turn (
    session_id TEXT NOT NULL REFERENCES agent_session(id) ON DELETE CASCADE,
    turn_seq   INTEGER NOT NULL,
    state      TEXT NOT NULL,
    started_at TEXT NOT NULL,
    ended_at   TEXT,
    PRIMARY KEY (session_id, turn_seq)
);

-- Monotonic sequence source. A single row updated inside the same transaction
-- as the rows it numbers, so a crash cannot leave a gap that looks like a lost
-- row to the drain.
CREATE TABLE IF NOT EXISTS counter (
    name  TEXT PRIMARY KEY,
    value INTEGER NOT NULL
);
INSERT OR IGNORE INTO counter (name, value) VALUES ('seq', 0);

-- The outbox drain: pending rows in sequence order, per workspace.
CREATE INDEX IF NOT EXISTS idx_session_outbox
    ON agent_session (workspace_id, sync_state);
CREATE INDEX IF NOT EXISTS idx_message_outbox
    ON agent_message (sync_state, seq);

-- Ordered reads for one Session.
CREATE INDEX IF NOT EXISTS idx_message_session_seq
    ON agent_message (session_id, seq);

-- The session-detail sidebar's Prompts / Responses / Intermediate counts.
-- Covering, so the counts are answerable without reading a single body.
CREATE INDEX IF NOT EXISTS idx_message_facets
    ON agent_message (session_id, role, mode);

-- Re-processing the same turn must not duplicate it. Partial, because a message
-- the agent gave no id to still deserves a row.
CREATE UNIQUE INDEX IF NOT EXISTS idx_message_native_id
    ON agent_message (session_id, native_message_id)
    WHERE native_message_id IS NOT NULL;

-- Timeline ordering across a Workspace.
CREATE INDEX IF NOT EXISTS idx_session_started
    ON agent_session (workspace_id, started_at);
"#;

const V2: &str = r#"
-- What the agent actually did. A row per invocation, never content embedded in
-- a Message body — three independent reasons, any one sufficient:
--
--   * The session-detail sidebar's live counts (Tool calls 4 · File edits 1 ·
--     Bash 1 · Read 2) have to be a GROUP BY. Inside a body they would mean
--     parsing a blob per Session, and board-level filters would full-scan.
--   * A tool `result` is the largest payload in a Session — a read of a big
--     file, a verbose test run. Spilling it independently of the row is what
--     actually keeps rows under the storage row cap.
--   * `arguments` and `result` are the highest-risk content for secrets, so
--     redaction needs them individually addressable.
CREATE TABLE IF NOT EXISTS tool_call (
    id             TEXT PRIMARY KEY,
    session_id     TEXT NOT NULL REFERENCES agent_session(id) ON DELETE CASCADE,
    seq            INTEGER NOT NULL,
    turn_seq       INTEGER NOT NULL,
    -- The agent's own id for the call — the idempotency key across the
    -- first-sighting event and every later update.
    native_call_id TEXT,
    -- Derived, not handed over: the wire has no canonical tool name. Grouping by
    -- the raw wire value would produce one bucket per file the agent touched.
    tool_name      TEXT NOT NULL,
    -- The human display string, kept separately so the detail view can show what
    -- the agent called it without the facet counts inheriting the churn.
    title          TEXT,
    kind           TEXT,
    status         TEXT NOT NULL,
    locations      TEXT NOT NULL DEFAULT '[]',
    arguments      TEXT,
    arguments_ref  TEXT,
    result         TEXT,
    result_ref     TEXT,
    -- A non-UTF8 result (a compiled binary, an image) is stored verbatim and
    -- skipped by string redaction rather than lossily decoded and corrupted.
    result_binary  INTEGER NOT NULL DEFAULT 0,
    created_at     TEXT NOT NULL,
    sync_state     TEXT NOT NULL DEFAULT 'local',
    sync_attempts  INTEGER NOT NULL DEFAULT 0,

    UNIQUE (session_id, seq)
);

-- The derived subset produced by file-writing calls. A shell command or a file
-- read yields neither this nor an edit patch, which is exactly why they cannot
-- stand in for a general tool-call record.
--
-- `existed_before` and `sha256_after` are captured at write time and are
-- unrecoverable afterwards: by the time a commit lands the file exists either
-- way, and the agent's version has been overwritten. The asymmetric link rule
-- is built entirely on those two facts.
CREATE TABLE IF NOT EXISTS file_touch (
    id             TEXT PRIMARY KEY,
    tool_call_id   TEXT NOT NULL REFERENCES tool_call(id) ON DELETE CASCADE,
    session_id     TEXT NOT NULL REFERENCES agent_session(id) ON DELETE CASCADE,
    turn_seq       INTEGER NOT NULL,
    seq            INTEGER NOT NULL,
    -- NFC-normalised, workspace-relative. macOS hands back NFD while git stores
    -- NFC, and a byte comparison of the two fails silently.
    path           TEXT NOT NULL,
    sha256_after   TEXT,
    existed_before INTEGER NOT NULL,
    deleted        INTEGER NOT NULL DEFAULT 0,
    -- Written outside the Workspace root. Can never match a commit, and is
    -- flagged so it is not counted as pending agent work forever.
    out_of_repo    INTEGER NOT NULL DEFAULT 0,
    created_at     TEXT NOT NULL
);

-- Attribution's input. The metric — the agent-versus-human line split — is a
-- pure function over stored rows and can be computed and backfilled whenever;
-- the patch cannot be recaptured once the Session ends and the file moves on.
CREATE TABLE IF NOT EXISTS agent_edit (
    id           TEXT PRIMARY KEY,
    tool_call_id TEXT NOT NULL REFERENCES tool_call(id) ON DELETE CASCADE,
    session_id   TEXT NOT NULL REFERENCES agent_session(id) ON DELETE CASCADE,
    turn_seq     INTEGER NOT NULL,
    path         TEXT NOT NULL,
    patch        TEXT,
    patch_ref    TEXT,
    created_at   TEXT NOT NULL
);

-- The sidebar's per-kind counts.
CREATE INDEX IF NOT EXISTS idx_tool_call_facets
    ON tool_call (session_id, tool_name);
CREATE INDEX IF NOT EXISTS idx_tool_call_session_seq
    ON tool_call (session_id, seq);
CREATE INDEX IF NOT EXISTS idx_tool_call_outbox
    ON tool_call (sync_state, seq);
CREATE UNIQUE INDEX IF NOT EXISTS idx_tool_call_native_id
    ON tool_call (session_id, native_call_id)
    WHERE native_call_id IS NOT NULL;

-- The link rule's lookup: every path a Session touched.
CREATE INDEX IF NOT EXISTS idx_file_touch_path
    ON file_touch (session_id, path);
-- …and the reverse, which is what turns an observed commit into candidate
-- Sessions without scanning every Session in the Workspace.
CREATE INDEX IF NOT EXISTS idx_file_touch_by_path
    ON file_touch (path);
CREATE INDEX IF NOT EXISTS idx_agent_edit_session
    ON agent_edit (session_id, turn_seq);
"#;

/// Index names the store guarantees. Exposed so a test can assert they survived
/// a migration — an index silently dropped is a full scan nobody notices until
/// a developer with a year of history opens the board.
pub const REQUIRED_INDEXES: &[&str] = &[
    "idx_session_outbox",
    "idx_message_outbox",
    "idx_message_session_seq",
    "idx_message_facets",
    "idx_message_native_id",
    "idx_session_started",
    "idx_tool_call_facets",
    "idx_tool_call_session_seq",
    "idx_tool_call_outbox",
    "idx_tool_call_native_id",
    "idx_file_touch_path",
    "idx_file_touch_by_path",
    "idx_agent_edit_session",
];
