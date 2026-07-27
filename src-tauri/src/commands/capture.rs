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
use std::sync::Mutex;

use atlas_agents::{
    MessageRole, OutboundMiddleware, SessionDelta, SessionDeltaEnvelope,
};
use atlas_checkpoint::{
    Capture, Mode, Role, SessionKey, Source, Store, TokenTotals, TurnContent, WorkspaceMode,
};
use tauri::{AppHandle, Manager};

/// What the middleware knows about a session, learned at send time.
///
/// Resolved from the manager's session snapshot rather than from
/// `SharedMemoryStore::session_meta`: that store is only populated when the
/// per-project memory-sharing toggle is on, which is off by default, so reusing
/// it would silently capture nothing for most users.
#[derive(Clone)]
struct Binding {
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
        binding: Binding,
        prompt: String,
    },
    Turn {
        binding: Binding,
        native_message_id: String,
        role: Role,
        mode: Mode,
        body: String,
    },
    FinishTurn {
        binding: Binding,
    },
    Usage {
        binding: Binding,
        totals: TokenTotals,
    },
}

/// App-wide capture state: the session registry and the worker's channel.
pub struct CaptureState {
    sessions: Mutex<HashMap<String, Binding>>,
    tx: mpsc::Sender<Job>,
}

impl CaptureState {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        // Unbounded on purpose. A bounded channel would have to choose between
        // blocking the emit thread and dropping a turn, and both are worse than
        // holding a few queued turns in memory — the worker drains them in
        // milliseconds.
        std::thread::Builder::new()
            .name("atlas-capture".into())
            .spawn(move || worker(rx))
            .expect("capture worker thread");
        Self {
            sessions: Mutex::new(HashMap::new()),
            tx,
        }
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
                .or_insert_with(|| Binding {
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

    fn binding(&self, session_id: &str) -> Option<Binding> {
        self.sessions
            .lock()
            .expect("capture registry")
            .get(session_id)
            .cloned()
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
fn worker(rx: mpsc::Receiver<Job>) {
    let mut stores: HashMap<PathBuf, Option<Store>> = HashMap::new();
    // Session ids are assigned by the store on first write and reused after.
    let mut session_ids: HashMap<String, String> = HashMap::new();

    while let Ok(job) = rx.recv() {
        let binding = match &job {
            Job::Prompt { binding, .. }
            | Job::Turn { binding, .. }
            | Job::FinishTurn { binding }
            | Job::Usage { binding, .. } => binding.clone(),
        };

        let root = binding.workspace_root.clone();
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

        let key = SessionKey {
            // The Workspace binding proper arrives with the enable popover; until
            // then a Workspace is its project directory, which is the same
            // identity `.atlas/` already uses.
            workspace_id: root.to_string_lossy().to_string(),
            source: binding.source,
            native_session_id: binding.native_session_id.clone(),
        };

        let mut capture = Capture::new(store, WorkspaceMode::Local);
        let outcome = match job {
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
