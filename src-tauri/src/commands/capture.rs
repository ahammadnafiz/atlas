//! Wiring `atlas-checkpoint` into the running app.
//!
//! The crate itself is Tauri-free and knows nothing about agents. This module is
//! the adapter: it turns the agent delta stream into capture calls, and owns the
//! per-Workspace stores.
//!
//! Three decisions here are not obvious from the crate's API, and all three come
//! from how the runtime actually behaves rather than from how it reads:
//!
//! **Capture is a pipeline stage, not a bus subscriber.** The `atlas-bus`
//! broadcast drops events for a subscriber that lags past the ring capacity —
//! correct for the UI fan-out, where a dropped frame is invisible, and wrong
//! here, where a dropped event is a turn missing from the permanent record. The
//! [`OutboundPipeline`] slot runs synchronously on the emit thread and cannot
//! lag by construction. (`atlas_bus::middleware` has a test pinning that
//! contrast.)
//!
//! **The user's prompt does not arrive on the delta stream.** The session actor
//! deliberately skips emitting a message-appended delta for user messages — the
//! frontend adds them optimistically — and turn start is a status flip carrying
//! no text. A delta subscriber alone would therefore produce Sessions with no
//! prompts and no titles. [`note_prompt`] is called from the send path instead,
//! with the text the user actually typed, *before* Atlas's memory-injection
//! blocks are prepended: injected context is machinery, not something the user
//! said, and titling a Session after it would be nonsense.
//!
//! **Writes happen on one owned thread, not on the emit thread.** SQLite work is
//! disk work and the streaming hot path must never block on it, but
//! `spawn_blocking` would also let two turns land out of order. A single worker
//! thread behind an unbounded channel gets both properties: `on_event` returns
//! immediately, and turns are written in the order they happened.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use atlas_agents::{
    MessageRole, OutboundMiddleware, SessionDelta, SessionDeltaEnvelope, ToolCallStatus,
};
use atlas_checkpoint::tools::{extract_paths, resolve_path, ResolvedPath, ToolName};
use atlas_checkpoint::{
    Capture, FileWrite, Mode, Role, SessionKey, Source, Store, TokenTotals, ToolCallContent,
    ToolStatus, TurnContent, WorkspaceMode,
};
use tauri::{AppHandle, Manager};

/// What the middleware knows about one agent session, learned at send time.
///
/// Distinct from `atlas_checkpoint::Binding`, which is how the *Workspace* is
/// bound (mode, Slug, fingerprints). This is per-conversation routing state.
///
/// Resolved from the manager's session snapshot rather than from
/// `SharedMemoryStore::session_meta`: that store is only populated when the
/// per-project memory-sharing toggle is on, which is off by default, so reusing
/// it would silently capture nothing for most users.
#[derive(Clone)]
struct SessionBinding {
    workspace_root: PathBuf,
    source: Source,
    native_session_id: String,
    agent: Option<String>,
    model: Option<String>,
    cwd: String,
    /// The turn a message-appended delta belongs to. Message deltas carry no
    /// turn identity of their own, so the send path stamps it here.
    turn_seq: i64,
}

/// Work for the capture thread.
enum Job {
    Prompt {
        binding: SessionBinding,
        prompt: String,
    },
    Turn {
        binding: SessionBinding,
        native_message_id: String,
        role: Role,
        mode: Mode,
        body: String,
    },
    ToolCall {
        binding: SessionBinding,
        native_call_id: String,
        tool_name: ToolName,
        title: Option<String>,
        kind: Option<String>,
        status: ToolStatus,
        locations: serde_json::Value,
        arguments: Option<String>,
        result: Option<String>,
        /// Paths this call touches, with whether each existed *before* the agent
        /// wrote — sampled on first sighting, because after the write the answer
        /// is unknowable.
        writes: Vec<PendingWrite>,
        /// The call has finished, so the file on disk is now what the agent
        /// left and can be hashed.
        terminal: bool,
        /// The patch an edit-shaped call applied, when the arguments carry one.
        patch: Option<String>,
    },
    /// Send everything pending for this Workspace.
    ///
    /// Progressive and interruptible by construction: each pass sends what it
    /// can and leaves the rest pending, so closing Atlas mid-backlog resumes
    /// rather than restarting.
    Drain {
        workspace_root: PathBuf,
    },
    /// Import any on-disk transcripts for this Workspace that are not yet
    /// recorded — the historical backfill and the ongoing terminal-gap scan,
    /// which are the same operation run at different times.
    ImportTranscripts {
        workspace_root: PathBuf,
    },
    /// Walk from the last-seen commit to HEAD and link what it finds.
    ///
    /// Not tied to a Session — it is driven by the repository moving, and the
    /// Sessions it might link to are whatever the store already holds.
    WalkCommits {
        workspace_root: PathBuf,
        workspace_id: String,
    },
    FinishTurn {
        binding: SessionBinding,
    },
    Usage {
        binding: SessionBinding,
        totals: TokenTotals,
    },
}

/// A file a tool call is about to write, and what we knew before it did.
#[derive(Clone)]
struct PendingWrite {
    path: ResolvedPath,
    existed_before: bool,
}

/// App-wide capture state: the session registry and the worker's channel.
pub struct CaptureState {
    sessions: Mutex<HashMap<String, SessionBinding>>,
    /// `existed_before` per (tool call, path), sampled the first time we see the
    /// call and reused when it completes.
    ///
    /// This has to be sampled on the emit thread rather than on the worker: the
    /// first-sighting delta arrives *before* the agent performs the write, and
    /// that is the only moment the answer is knowable. A `stat` is microseconds,
    /// so it does not meaningfully cost the hot path — unlike hashing the file,
    /// which is deferred to the worker.
    pending_writes: Mutex<HashMap<String, Vec<PendingWrite>>>,
    /// How the drain gets an access token. Shared with the worker.
    token: TokenProvider,
    tx: mpsc::Sender<Job>,
}

/// A late-bound source of access tokens, shared between the command surface and
/// the capture worker.
type TokenProvider = Arc<Mutex<Option<Box<dyn Fn() -> Option<String> + Send>>>>;

impl CaptureState {
    /// `token` mints a fresh access token for the drain. A closure rather than a
    /// value because tokens are short-lived and a post-promotion backlog runs
    /// far longer than one lifetime.
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        // Late-bound: the auth core is managed inside `setup`, after this state
        // is registered, so the provider is installed rather than passed in.
        // Until it is, the closure yields no credential — which parks the drain
        // instead of failing it, and is exactly Local-mode behaviour.
        let token: TokenProvider = Arc::new(Mutex::new(None));
        let token_for_worker = token.clone();
        // Unbounded on purpose. A bounded channel would have to choose between
        // blocking the emit thread and dropping a turn, and both are worse than
        // holding a few queued turns in memory — the worker drains them in
        // milliseconds.
        std::thread::Builder::new()
            .name("atlas-capture".into())
            .spawn(move || worker(rx, token_for_worker))
            .expect("capture worker thread");
        Self {
            sessions: Mutex::new(HashMap::new()),
            pending_writes: Mutex::new(HashMap::new()),
            token,
            tx,
        }
    }

    /// Install the credential source the drain uses.
    ///
    /// Called once from `setup`, after the auth core exists.
    pub fn install_token_provider(&self, provider: Box<dyn Fn() -> Option<String> + Send>) {
        *self.token.lock().expect("token provider") = Some(provider);
    }

    /// Record the user's prompt and bind the session, from the send path.
    ///
    /// `prompt` must be the text the user typed, not the memory-prefixed version
    /// that reaches the agent. `session_id` is the agent's own id for the
    /// conversation, which is what makes a later import recognise it as already
    /// captured.
    pub fn note_prompt(
        &self,
        session_id: &str,
        cwd: &str,
        plugin_id: &str,
        model: Option<&str>,
        prompt: &str,
    ) {
        if cwd.is_empty() {
            // No project directory means nowhere to put `.atlas/`. Capture is a
            // no-op rather than an error — the agent turn is unaffected.
            return;
        }

        let binding = {
            let mut sessions = self.sessions.lock().expect("capture registry");
            let entry = sessions
                .entry(session_id.to_string())
                .or_insert_with(|| SessionBinding {
                    workspace_root: PathBuf::from(cwd),
                    source: source_for(plugin_id),
                    native_session_id: session_id.to_string(),
                    agent: Some(plugin_id.to_string()),
                    model: model.map(str::to_string),
                    cwd: cwd.to_string(),
                    turn_seq: 0,
                });
            entry.turn_seq += 1;
            if model.is_some() {
                entry.model = model.map(str::to_string);
            }
            entry.clone()
        };

        self.submit(Job::Prompt {
            binding,
            prompt: prompt.to_string(),
        });
    }

    /// Send everything pending for a Workspace.
    ///
    /// Handed to the worker rather than run inline, because a post-promotion
    /// backlog is hundreds of megabytes and must never block the click that
    /// started it.
    pub fn note_drain(&self, workspace_root: &std::path::Path) {
        self.submit(Job::Drain {
            workspace_root: workspace_root.to_path_buf(),
        });
    }

    /// Import on-disk transcripts for a Workspace.
    ///
    /// The same call serves the one-time backfill (on enable) and the ongoing
    /// watch (on the worker's interval), because they are the same reconciling
    /// scan — a file that has not grown is skipped by a size check, which is
    /// what makes running it repeatedly affordable.
    pub fn note_import(&self, workspace_root: &std::path::Path) {
        self.submit(Job::ImportTranscripts {
            workspace_root: workspace_root.to_path_buf(),
        });
    }

    /// The repository moved, or a Workspace was just opened — walk for new
    /// commits.
    ///
    /// This is the **in-process consumer** of the git watcher. The walk is
    /// invoked from the watcher callback directly rather than by round-tripping
    /// through the frontend, so commit detection does not depend on a window
    /// being open, on the frontend having subscribed, or on a renderer that may
    /// be busy.
    ///
    /// Also called on Workspace open, and that call is not a fallback: a watcher
    /// exists only for a Workspace activated at least once this app session, so
    /// for a never-activated or evicted Workspace the open-time walk is the only
    /// thing that will ever link its commits.
    pub fn note_git_change(&self, workspace_root: &std::path::Path) {
        self.submit(Job::WalkCommits {
            workspace_root: workspace_root.to_path_buf(),
            workspace_id: workspace_root.to_string_lossy().to_string(),
        });
    }

    fn binding(&self, session_id: &str) -> Option<SessionBinding> {
        self.sessions
            .lock()
            .expect("capture registry")
            .get(session_id)
            .cloned()
    }

    /// Note which files a call is about to touch, and whether each existed.
    ///
    /// Sampled once per call — the first sighting arrives before the agent
    /// writes, and every later sighting reuses that answer. Re-sampling on the
    /// completion update would always report `true`, which would make the link
    /// rule credit the agent for files a human wrote.
    fn sample_writes(
        &self,
        call_id: &str,
        workspace_root: &std::path::Path,
        locations: &[serde_json::Value],
        arguments: &serde_json::Value,
    ) -> Vec<PendingWrite> {
        let mut pending = self.pending_writes.lock().expect("pending writes");
        if let Some(existing) = pending.get(call_id) {
            return existing.clone();
        }

        let writes: Vec<PendingWrite> = extract_paths(locations, arguments)
            .into_iter()
            .map(|raw| {
                let path = resolve_path(&raw, workspace_root);
                let existed_before = workspace_root.join(&path.path).exists();
                PendingWrite { path, existed_before }
            })
            .collect();

        if !writes.is_empty() {
            pending.insert(call_id.to_string(), writes.clone());
        }
        writes
    }

    fn forget_writes(&self, call_id: &str) {
        self.pending_writes
            .lock()
            .expect("pending writes")
            .remove(call_id);
    }

    fn submit(&self, job: Job) {
        // A dead worker must not take the agent down with it. The turn is lost,
        // which is what the capture-health signal exists to surface.
        if self.tx.send(job).is_err() {
            tracing::error!(target: "atlas::capture", "capture worker is gone; turn not recorded");
        }
    }
}

impl Default for CaptureState {
    fn default() -> Self {
        Self::new()
    }
}

// ── Command surface ─────────────────────────────────────────────────────────
//
// These are synchronous store reads and writes, not delta-stream work, so they
// run on the caller's thread rather than through the capture worker. They are
// fast (one SQLite row, a handful of `git` reads) and the popover needs an
// answer to render at all.

/// What Atlas can work out about a directory before anything is bound.
///
/// Drives the popover's "Detected" block: origin, root commit, whether this is
/// a repository at all.
#[tauri::command]
pub fn capture_detect(project_path: String) -> Result<atlas_checkpoint::WorkspaceDetection, String> {
    Ok(atlas_checkpoint::detect(std::path::Path::new(&project_path)))
}

/// How this Workspace is bound, or `null` if capture was never enabled.
#[tauri::command]
pub fn capture_binding(project_path: String) -> Result<Option<atlas_checkpoint::Binding>, String> {
    let store = open_store(&project_path)?;
    store.binding().map_err(|e| e.to_string())
}

/// Turn capture on for this Workspace.
///
/// Local mode makes no network call and needs no account, which is the whole
/// point: Atlas has to be useful before anyone signs up for anything.
#[tauri::command]
pub fn capture_enable(
    project_path: String,
    mode: String,
    app: AppHandle,
) -> Result<atlas_checkpoint::Binding, String> {
    let mode = WorkspaceMode::parse(&mode)
        .ok_or_else(|| format!("unknown workspace mode: {mode}"))?;
    let root = std::path::Path::new(&project_path);
    let store = open_store(&project_path)?;

    let binding = atlas_checkpoint::bind(&store, &project_path, root, mode)
        .map_err(|e| e.to_string())?;

    // Establish the commit cursor immediately, so the first commit after
    // enabling is linked rather than waiting for a walk that has no baseline.
    let _ = atlas_checkpoint::walk_new_commits(&store, &project_path, root, mode);

    // Backfill this project's existing transcripts, so the timeline is
    // populated now rather than months from now. Handed to the worker rather
    // than done here, because a multi-hundred-megabyte corpus must never block
    // the click that started it.
    //
    // Local imports without ceremony — nothing leaves the machine. A Cloud
    // Workspace is a bulk disclosure and waits for `capture_import_confirm`,
    // after the developer has seen the real numbers.
    if mode == WorkspaceMode::Local {
        app.state::<CaptureState>().note_import(root);
    }
    Ok(binding)
}

/// What importing this Workspace's transcripts would disclose.
///
/// Real numbers, before the decision — how many Sessions, over what dates, how
/// much data. A developer cannot otherwise know what they are about to publish,
/// and this is one of only two bulk-disclosure moments in the whole feature.
#[tauri::command]
pub fn capture_import_preview(
    project_path: String,
) -> Result<atlas_checkpoint::ImportPreview, String> {
    let store = open_store(&project_path)?;
    let mode = store
        .binding()
        .map_err(|e| e.to_string())?
        .map(|b| b.mode)
        .unwrap_or(WorkspaceMode::Local);
    let root = std::path::Path::new(&project_path);
    let Some(source) = transcript_source_for(root) else {
        return Ok(atlas_checkpoint::ImportPreview::default());
    };
    Ok(atlas_checkpoint::import_preview(&source, mode))
}

/// Start the import, after the developer has confirmed it.
#[tauri::command]
pub fn capture_import_confirm(project_path: String, app: AppHandle) -> Result<(), String> {
    app.state::<CaptureState>()
        .note_import(std::path::Path::new(&project_path));
    Ok(())
}

/// Re-read the identity signals for an already-bound Workspace.
///
/// Called after the inline `git init` offer: the Workspace must start producing
/// Checkpoints without a restart, which it can only do once the fingerprint it
/// had no way to know is filled in.
#[tauri::command]
pub fn capture_refresh(project_path: String) -> Result<Option<atlas_checkpoint::Binding>, String> {
    let root = std::path::Path::new(&project_path);
    let store = open_store(&project_path)?;
    let binding = atlas_checkpoint::refresh_detection(&store, root).map_err(|e| e.to_string())?;
    if let Some(binding) = &binding {
        let _ = atlas_checkpoint::walk_new_commits(&store, &project_path, root, binding.mode);
    }
    Ok(binding)
}

/// Stop capturing. Nothing already recorded is deleted.
#[tauri::command]
pub fn capture_disable(project_path: String) -> Result<(), String> {
    atlas_checkpoint::disable(&open_store(&project_path)?).map_err(|e| e.to_string())
}

/// Initialise a repository in a non-git Workspace, then re-detect.
///
/// Framed in the UI as unlocking commit linkage rather than as a requirement,
/// because that is what it is: Sessions are captured either way.
#[tauri::command]
pub fn capture_git_init(project_path: String) -> Result<Option<atlas_checkpoint::Binding>, String> {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(&project_path)
        .arg("init")
        .output()
        .map_err(|e| format!("git init: {e}"))?;
    if !status.status.success() {
        return Err(String::from_utf8_lossy(&status.stderr).trim().to_string());
    }
    capture_refresh(project_path)
}

/// The capture-health state for a Workspace.
///
/// Watcher liveness is read from the watcher registry itself rather than
/// inferred from "no events lately" — a quiet repository and a dead watcher are
/// indistinguishable from the event stream, which is exactly how the clear-all
/// bug stayed invisible.
#[tauri::command]
pub fn capture_health(
    project_path: String,
    workspace_id: Option<String>,
    watchers: tauri::State<'_, super::git_watcher::GitWatcherState>,
) -> Result<atlas_checkpoint::CaptureHealth, String> {
    let root = std::path::Path::new(&project_path);
    let store = open_store(&project_path)?;

    let expects_watcher = atlas_checkpoint::git::is_repository(root);
    let watcher_attached = expects_watcher
        && watchers.is_watching(workspace_id.as_deref().unwrap_or(&project_path));

    atlas_checkpoint::evaluate_health(
        &store,
        &project_path,
        atlas_checkpoint::HostSignals { watcher_attached, expects_watcher },
    )
    .map_err(|e| e.to_string())
}

/// Every Session captured in this Workspace, newest first.
///
/// The whole point of the recorder. Reading is safe from any window — the
/// writer lock guards writes only — so this deliberately does not check
/// `is_writer`: a second Atlas window cannot record, but it can still read back
/// what the first one did.
#[tauri::command]
pub fn artifacts_sessions(
    project_path: String,
    workspace_id: Option<String>,
) -> Result<Vec<atlas_checkpoint::SessionSummary>, String> {
    let store = open_store(&project_path)?;
    let workspace_id = workspace_id.unwrap_or_else(|| project_path.clone());
    atlas_checkpoint::session_summaries(&store, &workspace_id).map_err(|e| e.to_string())
}

/// One Session as an ordered timeline.
///
/// Commit subjects are resolved from git here rather than in the crate: this is
/// the layer that knows the Workspace root, and git remains the single source of
/// truth for a commit message rather than a copy in the store that goes stale
/// after a reword.
#[tauri::command]
pub fn artifacts_session(
    project_path: String,
    session_id: String,
) -> Result<Option<atlas_checkpoint::SessionDetail>, String> {
    let root = std::path::Path::new(&project_path).to_path_buf();
    let store = open_store(&project_path)?;

    // A Session usually carries a handful of Checkpoints, but one `git show` per
    // Checkpoint is still a process spawn each. Resolving them once up front
    // keeps a long Session from paying that cost repeatedly.
    let mut subjects: HashMap<String, String> = HashMap::new();
    if atlas_checkpoint::git::is_repository(&root) {
        for checkpoint in store.checkpoints_for_session(&session_id).map_err(|e| e.to_string())? {
            if let Ok(info) = atlas_checkpoint::git::commit_info(&root, &checkpoint.commit_sha) {
                subjects.insert(checkpoint.commit_sha, info.subject);
            }
        }
    }

    atlas_checkpoint::session_detail(&store, &session_id, |sha| subjects.get(sha).cloned())
        .map_err(|e| e.to_string())
}

/// Is this Slug free within the Organisation?
///
/// Debounced by the caller so the answer arrives while the developer is still
/// typing, rather than as a rejection after they commit to a name.
#[tauri::command]
pub fn capture_slug_available(
    project_path: String,
    org_id: String,
    slug: String,
    app: AppHandle,
) -> Result<String, String> {
    let token = token_provider(&app);
    let config = sync_config(&project_path, &org_id, &token);
    Ok(match atlas_checkpoint::sync::check_slug(&config, &slug) {
        atlas_checkpoint::SlugAvailability::Available => "available",
        atlas_checkpoint::SlugAvailability::Taken => "taken",
        // Distinct from "taken": telling a developer their name is gone when
        // the network merely blinked is a lie they will act on.
        atlas_checkpoint::SlugAvailability::Unknown => "unknown",
    }
    .to_string())
}

/// Register this Workspace with an Organisation and switch it to Cloud.
///
/// **Server first.** The Slug is unique within the Organisation, so it has to be
/// settled server-side before any local state changes — otherwise a rejected
/// Slug leaves a half-bound Workspace behind. A failure here leaves the
/// Workspace capturing locally, which is a retryable state rather than a broken
/// one.
#[tauri::command]
pub fn capture_register_cloud(
    project_path: String,
    org_id: String,
    slug: String,
    app: AppHandle,
) -> Result<atlas_checkpoint::Binding, String> {
    let store = open_store(&project_path)?;
    let binding = store
        .binding()
        .map_err(|e| e.to_string())?
        .ok_or("enable capture for this Workspace first")?;

    let token = token_provider(&app);
    let config = sync_config(&project_path, &org_id, &token);

    // Advisory only — the server must accept a registration with neither.
    atlas_checkpoint::register_workspace(
        &config,
        &slug,
        binding.root_commit_sha.as_deref(),
        binding.git_url.as_deref(),
    )
    .map_err(|e| e.to_string())?;

    store
        .set_cloud_binding(&org_id, &slug)
        .map_err(|e| e.to_string())?;
    store.binding().map_err(|e| e.to_string())?.ok_or_else(|| "binding vanished".into())
}

/// The Organisation's Workspaces, with the one this repository most likely
/// belongs to already picked out.
///
/// Returns no pre-selection when several match — every repository created from
/// the same GitHub template shares a root commit, so a match is not proof, and a
/// confident wrong answer pollutes a *shared* timeline with foreign Sessions.
#[tauri::command]
pub fn capture_connect_options(
    project_path: String,
    org_id: String,
    app: AppHandle,
) -> Result<ConnectOptions, String> {
    let token = token_provider(&app);
    let config = sync_config(&project_path, &org_id, &token);
    let workspaces = atlas_checkpoint::list_workspaces(&config).map_err(|e| e.to_string())?;

    let detection = atlas_checkpoint::detect(std::path::Path::new(&project_path));
    let chosen = atlas_checkpoint::preselect(
        &workspaces,
        detection.root_commit_sha.as_deref(),
        detection.git_url.as_deref(),
    );

    Ok(match chosen {
        atlas_checkpoint::Preselection::One { workspace, .. } => ConnectOptions {
            workspaces,
            preselected: Some(workspace.id),
            // A shallow clone's fingerprint is a graft boundary rather than the
            // true root, so even a match is worth flagging.
            warning: detection
                .is_shallow
                .then(|| "This is a shallow clone, so its fingerprint is not authoritative.".into()),
        },
        atlas_checkpoint::Preselection::Ambiguous { candidates } => ConnectOptions {
            workspaces,
            preselected: None,
            warning: Some(format!(
                "{} Workspaces share this repository\u{2019}s root commit — repositories created \
                 from the same template do. Pick the right one.",
                candidates.len()
            )),
        },
        atlas_checkpoint::Preselection::None => ConnectOptions {
            workspaces,
            preselected: None,
            warning: Some(
                "This directory does not match any Workspace. Connecting anyway is fine — a \
                 shallow clone, a squashed history or a fresh repository all look like this."
                    .into(),
            ),
        },
    })
}

/// What Connect offers the developer.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectOptions {
    pub workspaces: Vec<atlas_checkpoint::RemoteWorkspace>,
    /// `None` when nothing matched, or when several did.
    pub preselected: Option<String>,
    /// Shown, never blocking.
    pub warning: Option<String>,
}

/// Connect this repository to an existing Workspace.
///
/// From here on it behaves exactly like a Workspace created as Cloud — same
/// capture, same drain, no separate code path.
#[tauri::command]
pub fn capture_connect(
    project_path: String,
    org_id: String,
    slug: String,
    app: AppHandle,
) -> Result<atlas_checkpoint::Binding, String> {
    let store = open_store(&project_path)?;
    store
        .set_cloud_binding(&org_id, &slug)
        .map_err(|e| e.to_string())?;
    app.state::<CaptureState>()
        .note_drain(std::path::Path::new(&project_path));
    store
        .binding()
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "enable capture for this Workspace first".into())
}

/// What promoting this Workspace to Cloud would disclose.
///
/// Real numbers before the decision — how many Sessions, over what dates, how
/// many secrets were redacted on the way in. This is one of only two
/// bulk-disclosure moments in the feature, and the developer genuinely cannot
/// otherwise know what they are about to publish.
#[tauri::command]
pub fn capture_promotion_preview(project_path: String) -> Result<PromotionPreview, String> {
    let store = open_store(&project_path)?;
    let sessions = store
        .sessions_for_workspace(&project_path)
        .map_err(|e| e.to_string())?;

    let secrets_redacted: u64 = sessions
        .iter()
        .filter_map(|s| s.redaction_counts.as_object())
        .flat_map(|counts| counts.values())
        .filter_map(serde_json::Value::as_u64)
        .sum();

    Ok(PromotionPreview {
        session_count: sessions.len(),
        earliest: sessions.iter().map(|s| s.started_at).min().map(|t| t.to_rfc3339()),
        latest: sessions.iter().map(|s| s.started_at).max().map(|t| t.to_rfc3339()),
        secrets_redacted,
    })
}

/// What a promotion is about to publish.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromotionPreview {
    pub session_count: usize,
    pub earliest: Option<String>,
    pub latest: Option<String>,
    pub secrets_redacted: u64,
}

/// Promote a Local Workspace to Cloud, bringing its history.
///
/// The entire mechanism is flipping `local` rows to `pending`. There is
/// deliberately **no separate backfill path**: the accumulated history joins the
/// same queue as everything else, so there is one drain to keep correct rather
/// than two, and closing Atlas mid-drain resumes for free.
#[tauri::command]
pub fn capture_promote(
    project_path: String,
    org_id: String,
    slug: String,
    app: AppHandle,
) -> Result<i64, String> {
    // Registration first, so cancelling or failing leaves the Workspace exactly
    // as it was: still Local, still captured, nothing sent.
    capture_register_cloud(project_path.clone(), org_id, slug, app.clone())?;

    let store = open_store(&project_path)?;
    let moved = store
        .promote_local_rows(&project_path)
        .map_err(|e| e.to_string())?;

    app.state::<CaptureState>().note_drain(std::path::Path::new(&project_path));
    Ok(moved)
}

/// A closure the drain can call to mint or refresh an access token.
///
/// A closure rather than a value because tokens are short-lived and a
/// post-promotion backlog runs far longer than one lifetime — the drain has to
/// be able to get a fresh one *during* a long pass.
fn token_provider(app: &AppHandle) -> impl Fn() -> Option<String> {
    let core = app.state::<crate::commands::auth::AuthState>().core();
    move || {
        // Blocking on the async mint is fine here: every caller is either a
        // Tauri command thread or the capture worker, never the UI thread.
        tauri::async_runtime::block_on(core.mint_access_token()).ok()
    }
}

fn sync_config<'a>(
    project_path: &str,
    org_id: &str,
    token: &'a dyn Fn() -> Option<String>,
) -> atlas_checkpoint::SyncConfig<'a> {
    atlas_checkpoint::SyncConfig {
        base_url: atlas_checkpoint::sync::ingest_base(),
        org_id: org_id.to_string(),
        workspace_id: project_path.to_string(),
        token,
        timeout: std::time::Duration::from_secs(30),
    }
}

/// Open a Workspace's store for a one-shot command.
///
/// A second window holding the writer lock still gets a readable store, so the
/// popover renders; only the write paths refuse.
fn open_store(project_path: &str) -> Result<Store, String> {
    Store::open(atlas_checkpoint::atlas_dir(project_path)).map_err(|e| e.to_string())
}

/// Which capture path a session came from.
///
/// The distinction matters downstream: the native agent reports a real
/// input/output token split where ACP agents only surface a context gauge, and
/// the importer needs to tell an in-app Session from one it read off disk.
fn source_for(plugin_id: &str) -> Source {
    if plugin_id == atlas_agents::CERSEI_PLUGIN_ID {
        Source::Cersei
    } else {
        Source::Acp
    }
}

/// Owns every Workspace's store and does all the writing, in order.
fn worker(rx: mpsc::Receiver<Job>, drain_token: TokenProvider) {
    let mut stores: HashMap<PathBuf, Option<Store>> = HashMap::new();
    // Session ids are assigned by the store on first write and reused after.
    let mut session_ids: HashMap<String, String> = HashMap::new();

    loop {
        // A timeout rather than a blocking receive, so the ongoing transcript
        // scan reaches **every bound Workspace** — including backgrounded ones.
        // The existing sessions watcher is a global singleton pointed at the
        // active workspace, so inheriting it would miss exactly the terminal
        // Sessions this is meant to catch.
        let job = match rx.recv_timeout(IMPORT_SCAN_INTERVAL) {
            Ok(job) => job,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                for root in stores.keys().cloned().collect::<Vec<_>>() {
                    if let Some(Some(store)) = stores.get_mut(&root) {
                        import_for(store, &root);
                        // Reconnecting requires no action from the developer:
                        // the same tick that scans for transcripts retries the
                        // outbox.
                        drain_for(store, &root, &drain_token);
                    }
                }
                continue;
            }
            // The app is shutting down.
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };

        // A commit walk is not tied to a Session, so it carries its own root
        // rather than a binding.
        let (session_binding, root) = match &job {
            Job::WalkCommits { workspace_root, .. }
            | Job::ImportTranscripts { workspace_root }
            | Job::Drain { workspace_root } => (None, workspace_root.clone()),
            Job::Prompt { binding, .. }
            | Job::Turn { binding, .. }
            | Job::ToolCall { binding, .. }
            | Job::FinishTurn { binding }
            | Job::Usage { binding, .. } => {
                (Some(binding.clone()), binding.workspace_root.clone())
            }
        };
        let store = stores.entry(root.clone()).or_insert_with(|| {
            match Store::open(atlas_checkpoint::atlas_dir(&root)) {
                Ok(store) => {
                    if !store.is_writer() {
                        // Another window owns this Workspace. Deferring is the
                        // whole point of the lock: two writers corrupt the
                        // outbox state machine.
                        tracing::info!(
                            target: "atlas::capture",
                            workspace = %root.display(),
                            "another window is recording this workspace; capture deferred"
                        );
                    }
                    Some(store)
                }
                Err(e) => {
                    tracing::error!(
                        target: "atlas::capture",
                        workspace = %root.display(),
                        "session store unavailable: {e}"
                    );
                    None
                }
            }
        });

        let Some(store) = store.as_mut() else { continue };
        if !store.is_writer() {
            continue;
        }

        // Capture is opt-in per Workspace, and pausing it must actually stop new
        // records. An unbound Workspace records nothing at all: the developer
        // has not asked for it, and writing a store for every directory they
        // ever opened an agent in would be a surprise rather than a feature.
        let Ok(Some(workspace)) = store.binding() else {
            continue;
        };
        if !workspace.is_capturing() {
            continue;
        }
        let mode = workspace.mode;

        // The commit walk needs the store but no Session, so it is handled
        // before the Session-scoped jobs below.
        if let Job::WalkCommits { workspace_id, .. } = &job {
            match atlas_checkpoint::walk_new_commits(store, workspace_id, &root, mode) {
                Ok(outcome) if outcome.checkpoints_created > 0 => tracing::info!(
                    target: "atlas::capture",
                    commits = outcome.commits_seen,
                    checkpoints = outcome.checkpoints_created,
                    "linked commits to sessions"
                ),
                Ok(outcome) if outcome.cursor_recovered => tracing::warn!(
                    target: "atlas::capture",
                    workspace = %root.display(),
                    "commit cursor could not be resolved; recovered by re-scan"
                ),
                Ok(_) => {}
                Err(e) => tracing::warn!(target: "atlas::capture", "commit walk failed: {e}"),
            }

            // Same trigger, because a rewrite moves refs exactly like a commit
            // does. Cheap when nothing was rewritten: it does no work at all
            // once every Checkpoint's commit is still reachable.
            match atlas_checkpoint::reconcile_rewrites(store, workspace_id, &root) {
                Ok(outcome) if outcome.is_mass_orphan() => tracing::warn!(
                    target: "atlas::capture",
                    orphaned = outcome.orphaned,
                    "history-wide rewrite orphaned Checkpoints in bulk"
                ),
                Ok(outcome) if outcome.relinked + outcome.orphaned + outcome.recovered > 0 => {
                    tracing::info!(
                        target: "atlas::capture",
                        relinked = outcome.relinked,
                        orphaned = outcome.orphaned,
                        recovered = outcome.recovered,
                        "reconciled Checkpoints after a history rewrite"
                    )
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(target: "atlas::capture", "reconciliation failed: {e}")
                }
            }
            continue;
        }

        if let Job::ImportTranscripts { .. } = &job {
            import_for(store, &root);
            continue;
        }

        if let Job::Drain { .. } = &job {
            drain_for(store, &root, &drain_token);
            continue;
        }

        let Some(binding) = session_binding else { continue };
        let key = SessionKey {
            // The Workspace binding proper arrives with the enable popover; until
            // then a Workspace is its project directory, which is the same
            // identity `.atlas/` already uses.
            workspace_id: root.to_string_lossy().to_string(),
            source: binding.source,
            native_session_id: binding.native_session_id.clone(),
        };

        let mut capture = Capture::new(store, mode);
        let outcome = match job {
            // Already handled above; none of these needs a Session.
            Job::WalkCommits { .. } | Job::ImportTranscripts { .. } | Job::Drain { .. } => Ok(()),
            Job::Prompt { prompt, .. } => capture
                .record_prompt(
                    &key,
                    &prompt,
                    binding.turn_seq,
                    binding.agent.as_deref(),
                    binding.model.as_deref(),
                    Some(&binding.cwd),
                )
                .map(|id| {
                    session_ids.insert(binding.native_session_id.clone(), id);
                }),
            Job::Turn {
                native_message_id,
                role,
                mode,
                body,
                ..
            } => match session_ids.get(&binding.native_session_id) {
                Some(session_id) => capture
                    .record_turn(
                        session_id,
                        TurnContent {
                            turn_seq: binding.turn_seq,
                            native_message_id: Some(native_message_id),
                            role,
                            mode,
                            body,
                        },
                    )
                    .map(|_| ()),
                // A delta before the session's first send has no Session row to
                // attach to. Normal, not an error.
                None => Ok(()),
            },
            Job::ToolCall {
                native_call_id,
                tool_name,
                title,
                kind,
                status,
                locations,
                arguments,
                result,
                writes,
                terminal,
                patch,
                ..
            } => match session_ids.get(&binding.native_session_id) {
                Some(session_id) => record_tool_call(
                    &mut capture,
                    session_id,
                    &binding,
                    ToolCallJob {
                        native_call_id,
                        tool_name,
                        title,
                        kind,
                        status,
                        locations,
                        arguments,
                        result,
                        writes,
                        terminal,
                        patch,
                    },
                ),
                None => Ok(()),
            },
            Job::FinishTurn { .. } => match session_ids.get(&binding.native_session_id) {
                Some(session_id) => capture.finish_turn(session_id, binding.turn_seq),
                None => Ok(()),
            },
            Job::Usage { totals, .. } => match session_ids.get(&binding.native_session_id) {
                Some(session_id) => capture.record_usage(session_id, &totals),
                None => Ok(()),
            },
        };

        if let Err(e) = outcome {
            // Already flagged on the Session row by the crate where it matters;
            // this is the operator-facing half.
            tracing::warn!(target: "atlas::capture", "capture failed: {e}");
        }
    }
}

/// How often every bound Workspace is re-scanned for new transcripts.
///
/// This is the ongoing half of the importer — the terminal-gap scan. Cheap
/// enough to run on a timer because a file that has not grown is skipped by a
/// size comparison before it is opened.
const IMPORT_SCAN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// Import a Workspace's transcripts, best-effort.
///
/// Never fails the caller: a missing transcript directory (the developer has
/// never run this agent) is the ordinary case, not an error.
fn import_for(store: &mut Store, root: &std::path::Path) {
    let Ok(Some(binding)) = store.binding() else { return };
    if !binding.is_capturing() {
        return;
    }
    let Some(source) = transcript_source_for(root) else { return };

    let workspace_id = root.to_string_lossy().to_string();
    match atlas_checkpoint::import_all(store, &workspace_id, &source, binding.mode) {
        Ok(outcome) if outcome.sessions_imported > 0 => tracing::info!(
            target: "atlas::capture",
            sessions = outcome.sessions_imported,
            messages = outcome.messages_imported,
            malformed = outcome.malformed_lines,
            "imported on-disk transcripts"
        ),
        Ok(_) => {}
        Err(e) => tracing::warn!(target: "atlas::capture", "transcript import failed: {e}"),
    }
}

/// Send everything pending for a Workspace, best-effort.
///
/// Offline is the ordinary case here, not an error: rows simply stay pending and
/// nothing is surfaced to the developer.
fn drain_for(store: &mut Store, root: &std::path::Path, token: &TokenProvider) {
    let Ok(Some(binding)) = store.binding() else { return };
    if binding.mode != WorkspaceMode::Cloud {
        // Local mode is the same database with draining switched off.
        return;
    }
    let Some(org_id) = binding.org_id.clone() else { return };

    let provider = token.clone();
    let mint_token = move || {
        provider
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(|mint| mint()))
            .flatten()
    };

    let config = atlas_checkpoint::SyncConfig {
        base_url: atlas_checkpoint::sync::ingest_base(),
        org_id,
        workspace_id: root.to_string_lossy().to_string(),
        token: &mint_token,
        timeout: std::time::Duration::from_secs(30),
    };

    match atlas_checkpoint::drain(store, &config) {
        Ok(outcome) if outcome.status == atlas_checkpoint::DrainStatus::NotAuthorized => {
            // A terminal state, not a retry loop. Surfaced through the
            // capture-health signal; local capture is unaffected.
            tracing::warn!(
                target: "atlas::capture",
                "no longer authorized for this workspace; drain stopped"
            );
        }
        Ok(outcome) if outcome.sent > 0 || outcome.failed > 0 => tracing::info!(
            target: "atlas::capture",
            sent = outcome.sent,
            failed = outcome.failed,
            pending = outcome.still_pending,
            "drained the outbox"
        ),
        Ok(_) => {}
        Err(e) => tracing::warn!(target: "atlas::capture", "drain failed: {e}"),
    }
}

/// Where this Workspace's agent transcripts live.
///
/// Claude Code encodes the project directory into a folder name under
/// `~/.claude/projects/`; the encoding already exists in `atlas-agents`, so it
/// is reused rather than reproduced.
fn transcript_source_for(root: &std::path::Path) -> Option<atlas_checkpoint::TranscriptSource> {
    let projects = dirs::home_dir()?.join(".claude").join("projects");
    let encoded = atlas_agents::transcript::encode_cwd(&root.to_string_lossy());
    Some(atlas_checkpoint::TranscriptSource::new(projects.join(encoded)))
}

/// The worker's view of one tool call.
struct ToolCallJob {
    native_call_id: String,
    tool_name: ToolName,
    title: Option<String>,
    kind: Option<String>,
    status: ToolStatus,
    locations: serde_json::Value,
    arguments: Option<String>,
    result: Option<String>,
    writes: Vec<PendingWrite>,
    terminal: bool,
    patch: Option<String>,
}

/// Write the tool call, then — once it has finished — hash what it left on disk.
///
/// The hash is taken here rather than at turn end, because by turn end the
/// developer may already have edited the file, and recording their content as
/// the agent's is exactly the false attribution the link rule exists to prevent.
/// Reading the file is real I/O, which is why it happens on the worker thread
/// and not on the emit thread that sampled `existed_before`.
fn record_tool_call(
    capture: &mut Capture<'_>,
    session_id: &str,
    binding: &SessionBinding,
    job: ToolCallJob,
) -> atlas_checkpoint::Result<()> {
    let call_id = capture.record_tool_call(
        session_id,
        ToolCallContent {
            turn_seq: binding.turn_seq,
            native_call_id: Some(&job.native_call_id),
            tool_name: job.tool_name,
            title: job.title.as_deref(),
            kind: job.kind.as_deref(),
            status: job.status,
            locations: &job.locations,
            arguments: job.arguments.as_deref(),
            result: job.result.as_deref().map(str::as_bytes),
        },
    )?;

    if !job.terminal {
        return Ok(());
    }

    for write in &job.writes {
        let absolute = binding.workspace_root.join(&write.path.path);
        // A file the call was going to write that is no longer there was
        // deleted. There is no content hash for a deletion — the marker is the
        // evidence the link rule uses instead.
        let (sha256_after, deleted) = match std::fs::read(&absolute) {
            Ok(bytes) => (Some(atlas_checkpoint::hash_written_content(&bytes)), false),
            Err(_) => (None, true),
        };

        capture.record_file_write(
            session_id,
            &call_id,
            binding.turn_seq,
            FileWrite {
                path: &write.path,
                sha256_after,
                existed_before: write.existed_before,
                deleted,
            },
        )?;

        if let Some(patch) = &job.patch {
            capture.record_edit_patch(
                session_id,
                &call_id,
                binding.turn_seq,
                &write.path.path,
                patch,
            )?;
        }
    }
    Ok(())
}

/// Serialise a call's arguments for storage, dropping an empty object rather
/// than storing `{}` on every shell command.
fn serialize_arguments(arguments: &serde_json::Value) -> Option<String> {
    match arguments {
        serde_json::Value::Object(map) if map.is_empty() => None,
        serde_json::Value::Null => None,
        other => Some(other.to_string()),
    }
}

/// The patch an edit-shaped call applied, from whatever shape its arguments use.
///
/// Attribution's input, and unrecoverable once the Session ends and the file
/// moves on — so a best-effort reconstruction from the before/after strings is
/// worth more than nothing. Agents that hand over a real diff are preferred.
fn edit_patch(arguments: &serde_json::Value) -> Option<String> {
    for key in ["patch", "diff"] {
        if let Some(patch) = arguments.get(key).and_then(serde_json::Value::as_str) {
            if !patch.trim().is_empty() {
                return Some(patch.to_string());
            }
        }
    }

    let old = ["old_string", "oldText", "old_str"]
        .iter()
        .find_map(|k| arguments.get(k).and_then(serde_json::Value::as_str));
    let new = ["new_string", "newText", "new_str", "content"]
        .iter()
        .find_map(|k| arguments.get(k).and_then(serde_json::Value::as_str));

    match (old, new) {
        (None, None) => None,
        (old, new) => Some(format!(
            "--- before\n+++ after\n{}{}",
            old.map(|o| format!("-{}\n", o.replace('\n', "\n-")))
                .unwrap_or_default(),
            new.map(|n| format!("+{}\n", n.replace('\n', "\n+")))
                .unwrap_or_default(),
        )),
    }
}

/// Feeds finalized turns into capture.
///
/// Deliberately only reacts to *finalized* content. Streaming text and thinking
/// chunks are a live UI concern and never become a stored artifact — coalescing
/// to whole turns is what keeps capture off the streaming hot path.
pub struct CaptureMiddleware {
    pub app: AppHandle,
}

impl OutboundMiddleware<SessionDeltaEnvelope> for CaptureMiddleware {
    fn on_event(&self, envelope: &SessionDeltaEnvelope) {
        let state = self.app.state::<CaptureState>();
        let Some(binding) = state.binding(&envelope.session_id) else {
            return;
        };

        match &envelope.delta {
            SessionDelta::MessageAppended { message } => {
                // The user's own message never arrives here — see the module
                // docs. Anything that does is the agent's.
                if message.role == MessageRole::User {
                    return;
                }
                let (mode, body) = match message.mode {
                    atlas_agents::MessageMode::Thinking => {
                        (Mode::Thinking, message.thinking.clone())
                    }
                    atlas_agents::MessageMode::Tool => (Mode::Tool, message.content.clone()),
                    atlas_agents::MessageMode::Text => (Mode::Text, message.content.clone()),
                };
                if body.trim().is_empty() {
                    return;
                }
                state.submit(Job::Turn {
                    binding,
                    native_message_id: message.id.clone(),
                    role: match message.role {
                        MessageRole::Assistant => Role::Assistant,
                        MessageRole::System => Role::System,
                        MessageRole::User => Role::User,
                    },
                    mode,
                    body,
                });
            }

            SessionDelta::ToolCallUpserted { tool_call, .. } => {
                let status = match tool_call.status {
                    ToolCallStatus::Pending => ToolStatus::Pending,
                    ToolCallStatus::Running => ToolStatus::Running,
                    ToolCallStatus::Completed => ToolStatus::Completed,
                    ToolCallStatus::Failed => ToolStatus::Failed,
                };
                let terminal =
                    matches!(status, ToolStatus::Completed | ToolStatus::Failed);

                // Derived, never the wire value: the runtime's `tool_name` is a
                // display title for ACP agents, and grouping by it would produce
                // one bucket per file the agent touched.
                let tool_name = atlas_checkpoint::canonical_name(
                    Some(&tool_call.tool_name),
                    tool_call.title.as_deref(),
                    tool_call.kind.as_deref(),
                    &tool_call.arguments,
                );

                let writes = if tool_name.writes_files() {
                    state.sample_writes(
                        &tool_call.id,
                        &binding.workspace_root,
                        &tool_call.locations,
                        &tool_call.arguments,
                    )
                } else {
                    Vec::new()
                };

                state.submit(Job::ToolCall {
                    binding,
                    native_call_id: tool_call.id.clone(),
                    tool_name,
                    title: tool_call.title.clone(),
                    kind: tool_call.kind.clone(),
                    status,
                    locations: serde_json::Value::Array(tool_call.locations.clone()),
                    arguments: serialize_arguments(&tool_call.arguments),
                    result: tool_call.result.clone(),
                    writes,
                    terminal,
                    patch: edit_patch(&tool_call.arguments),
                });

                if terminal {
                    state.forget_writes(&tool_call.id);
                }
            }

            SessionDelta::UsageUpdated { usage } => {
                // A genuine input/output split — only the native agent reports one.
                state.submit(Job::Usage {
                    binding,
                    totals: TokenTotals {
                        input_tokens: usage.input_tokens,
                        output_tokens: usage.output_tokens,
                        cache_creation_tokens: usage.cache_creation_tokens,
                        cache_read_tokens: usage.cache_read_tokens,
                        ..Default::default()
                    },
                });
            }

            SessionDelta::ContextUsage { used, size, .. } => {
                // A context-window gauge, which is a different measurement.
                // Stored in its own fields so it can never be rendered as a
                // usage split; the accurate ACP split is backfilled from the
                // agent's own transcript by the importer.
                state.submit(Job::Usage {
                    binding,
                    totals: TokenTotals {
                        context_used: Some(*used),
                        context_size: Some(*size),
                        ..Default::default()
                    },
                });
            }

            SessionDelta::TurnFinished { .. } | SessionDelta::TurnFailed { .. } => {
                state.submit(Job::FinishTurn { binding });
            }

            _ => {}
        }
    }
}
