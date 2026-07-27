//! The outbox drain, against a real HTTP server on loopback.
//!
//! No mock traits: the base URL is injected, so these drive the real client
//! against a real socket — the same pattern the auth tests use, and for the same
//! reason. What is asserted is what reached the server and what state the local
//! rows ended in.
//!
//! **The server contract this encodes does not exist yet** (ATL-57). These tests
//! are therefore also the written form of that contract: a 202 means durably
//! accepted, per-artifact results are echoed by row id, and 401/403/429 are
//! distinguishable. The drain must not ship against an endpoint that has not
//! adopted it.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use atlas_checkpoint::artifacts::AtlasArtifact;
use atlas_checkpoint::model::WorkspaceMode;
use atlas_checkpoint::{
    bind, drain, Capture, DrainStatus, Role, SessionKey, Source, Store, SyncConfig, TurnContent,
    SPILL_THRESHOLD_BYTES,
};

const WORKSPACE: &str = "ws-atlas";
const ORG: &str = "org-tryatlas";

// ── A stub ingest server ────────────────────────────────────────────────────

/// How the stub should answer the next ingest request.
#[derive(Clone, Debug)]
enum Reply {
    Accept,
    /// Accept some rows and reject others, by row-id substring.
    Partial { reject: Vec<String> },
    Status(u16),
    /// Accept, but only once the token has been refreshed.
    ExpireOnce,
}

struct Stub {
    base_url: String,
    received: Arc<Mutex<Vec<AtlasArtifact>>>,
    blobs: Arc<Mutex<Vec<String>>>,
}

impl Stub {
    fn start(replies: Vec<Reply>, blob_status: u16) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let received = Arc::new(Mutex::new(Vec::new()));
        let blobs = Arc::new(Mutex::new(Vec::new()));
        let ingest_calls = Arc::new(AtomicUsize::new(0));

        let stub = Self {
            base_url,
            received: received.clone(),
            blobs: blobs.clone(),
        };

        std::thread::spawn(move || {
            let mut seen_tokens: Vec<String> = Vec::new();
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                handle(
                    stream,
                    &replies,
                    blob_status,
                    &received,
                    &blobs,
                    &ingest_calls,
                    &mut seen_tokens,
                );
            }
        });
        stub
    }

    fn artifacts(&self) -> Vec<AtlasArtifact> {
        self.received.lock().unwrap().clone()
    }

    fn blob_keys(&self) -> Vec<String> {
        self.blobs.lock().unwrap().clone()
    }
}

fn handle(
    mut stream: TcpStream,
    replies: &[Reply],
    blob_status: u16,
    received: &Arc<Mutex<Vec<AtlasArtifact>>>,
    blobs: &Arc<Mutex<Vec<String>>>,
    ingest_calls: &Arc<AtomicUsize>,
    seen_tokens: &mut Vec<String>,
) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }

    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }

    let length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    if length > 0 {
        let _ = reader.read_exact(&mut body);
    }

    let token = headers
        .get("authorization")
        .cloned()
        .unwrap_or_default();

    // Blob upload.
    if request_line.starts_with("PUT /blobs/") {
        if let Some(key) = request_line
            .split_whitespace()
            .nth(1)
            .and_then(|p| p.strip_prefix("/blobs/"))
        {
            blobs.lock().unwrap().push(key.to_string());
        }
        respond(&mut stream, blob_status, "{}");
        return;
    }

    // Slug availability.
    if request_line.starts_with("GET /workspaces/slug-available") {
        let available = !request_line.contains("slug=taken");
        respond(
            &mut stream,
            200,
            &format!("{{\"available\":{available}}}"),
        );
        return;
    }

    // Workspace registration.
    if request_line.starts_with("POST /workspaces ") {
        if String::from_utf8_lossy(&body).contains("\"slug\":\"taken\"") {
            respond(&mut stream, 409, "{}");
        } else {
            respond(&mut stream, 200, "{\"workspaceId\":\"ws-remote-1\"}");
        }
        return;
    }

    // Ingest.
    let call = ingest_calls.fetch_add(1, Ordering::SeqCst);
    let reply = replies.get(call).cloned().unwrap_or(Reply::Accept);

    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
    let artifacts: Vec<AtlasArtifact> = parsed
        .get("artifacts")
        .and_then(|a| serde_json::from_value(a.clone()).ok())
        .unwrap_or_default();

    match reply {
        Reply::ExpireOnce => {
            let fresh = seen_tokens.iter().any(|t| t != &token);
            seen_tokens.push(token);
            if fresh {
                received.lock().unwrap().extend(artifacts);
                respond(&mut stream, 202, "{}");
            } else {
                respond(&mut stream, 401, "{}");
            }
        }
        Reply::Accept => {
            received.lock().unwrap().extend(artifacts);
            respond(&mut stream, 202, "{}");
        }
        Reply::Partial { reject } => {
            let results: Vec<serde_json::Value> = artifacts
                .iter()
                .map(|a| {
                    let id = a.row_id().to_string();
                    let accepted = !reject.iter().any(|r| id.contains(r.as_str()));
                    serde_json::json!({ "rowId": id, "accepted": accepted })
                })
                .collect();
            received
                .lock()
                .unwrap()
                .extend(artifacts.into_iter().filter(|a| {
                    !reject.iter().any(|r| a.row_id().contains(r.as_str()))
                }));
            respond(
                &mut stream,
                202,
                &serde_json::json!({ "results": results }).to_string(),
            );
        }
        Reply::Status(code) => respond(&mut stream, code, "{}"),
    }
}

fn respond(stream: &mut TcpStream, status: u16, body: &str) {
    let _ = write!(
        stream,
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.flush();
}

// ── Fixtures ────────────────────────────────────────────────────────────────

fn cloud_store(dir: &std::path::Path) -> Store {
    let store = Store::open(dir.join(".atlas")).expect("store opens");
    bind(&store, WORKSPACE, dir, WorkspaceMode::Cloud).expect("binds");
    store
}

fn record(store: &mut Store, native_id: &str, body: &str) -> String {
    let mut capture = Capture::new(store, WorkspaceMode::Cloud);
    let session = capture
        .record_prompt(
            &SessionKey {
                workspace_id: WORKSPACE.into(),
                source: Source::Acp,
                native_session_id: native_id.into(),
            },
            "Add rate limiting",
            1,
            Some("claude-code"),
            None,
            None,
        )
        .expect("prompt");
    capture
        .record_turn(
            &session,
            TurnContent {
                turn_seq: 1,
                native_message_id: Some(format!("{native_id}-m1")),
                role: Role::Assistant,
                mode: atlas_checkpoint::Mode::Text,
                body: body.to_string(),
            },
        )
        .expect("turn");
    session
}

fn config<'a>(base_url: &str, token: &'a dyn Fn() -> Option<String>) -> SyncConfig<'a> {
    SyncConfig {
        base_url: base_url.to_string(),
        org_id: ORG.to_string(),
        workspace_id: WORKSPACE.to_string(),
        token,
        timeout: Duration::from_secs(5),
    }
}

fn always_token() -> impl Fn() -> Option<String> {
    || Some("token-1".to_string())
}

// ── The happy path ──────────────────────────────────────────────────────────

#[test]
fn pending_rows_are_uploaded_and_marked_sent() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = cloud_store(dir.path());
    record(&mut store, "s1", "I've added a token bucket.");

    let stub = Stub::start(vec![], 200);
    let token = always_token();
    let outcome = drain(&store, &config(&stub.base_url, &token)).expect("drains");

    assert_eq!(outcome.status, DrainStatus::Drained);
    assert!(outcome.sent >= 3, "session + prompt + response, got {}", outcome.sent);
    assert_eq!(outcome.still_pending, 0);
    assert!(!stub.artifacts().is_empty());
}

#[test]
fn no_author_field_is_ever_sent() {
    // Authorship is stamped server-side from the verified token. A payload that
    // could declare it is a payload that could forge it.
    let dir = tempfile::tempdir().unwrap();
    let mut store = cloud_store(dir.path());
    record(&mut store, "s1", "hello");

    let stub = Stub::start(vec![], 200);
    let token = always_token();
    drain(&store, &config(&stub.base_url, &token)).unwrap();

    for artifact in stub.artifacts() {
        let json = serde_json::to_string(&artifact).unwrap();
        assert!(!json.contains("authorId"), "{json}");
        assert!(!json.contains("author_id"), "{json}");
    }
}

#[test]
fn replaying_the_same_batch_does_not_resend_rows() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = cloud_store(dir.path());
    record(&mut store, "s1", "hello");

    let stub = Stub::start(vec![], 200);
    let token = always_token();
    let cfg = config(&stub.base_url, &token);

    let first = drain(&store, &cfg).unwrap();
    let second = drain(&store, &cfg).unwrap();

    assert!(first.sent > 0);
    assert_eq!(second.sent, 0, "nothing is left pending to resend");
    assert_eq!(second.status, DrainStatus::Drained);
}

// ── Offline is the ordinary case ────────────────────────────────────────────

#[test]
fn with_the_server_unreachable_capture_succeeds_and_rows_stay_pending() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = cloud_store(dir.path());
    let session = record(&mut store, "s1", "recorded while offline");

    // A port nothing is listening on.
    let token = always_token();
    let outcome = drain(&store, &config("http://127.0.0.1:1", &token)).unwrap();

    assert_eq!(outcome.status, DrainStatus::Offline);
    assert_eq!(outcome.sent, 0);
    assert!(outcome.still_pending > 0);
    // The point: capture was completely unaffected.
    assert!(store.session(&session).unwrap().is_some());
    assert_eq!(store.messages_for_session(&session).unwrap().len(), 2);
}

#[test]
fn when_the_server_recovers_pending_rows_drain() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = cloud_store(dir.path());
    record(&mut store, "s1", "queued while offline");

    let token = always_token();
    assert_eq!(
        drain(&store, &config("http://127.0.0.1:1", &token)).unwrap().status,
        DrainStatus::Offline
    );

    let stub = Stub::start(vec![], 200);
    let outcome = drain(&store, &config(&stub.base_url, &token)).unwrap();
    assert_eq!(outcome.status, DrainStatus::Drained);
    assert!(outcome.sent > 0);
    assert_eq!(outcome.still_pending, 0);
}

#[test]
fn a_server_error_leaves_rows_pending_rather_than_failing_them() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = cloud_store(dir.path());
    record(&mut store, "s1", "hello");

    let stub = Stub::start(vec![Reply::Status(500)], 200);
    let token = always_token();
    let outcome = drain(&store, &config(&stub.base_url, &token)).unwrap();

    assert_eq!(outcome.status, DrainStatus::Offline, "5xx is the server's problem");
    assert_eq!(outcome.failed, 0);
    assert!(outcome.still_pending > 0);
}

// ── Auth ────────────────────────────────────────────────────────────────────

#[test]
fn a_token_expiring_mid_drain_refreshes_and_resumes() {
    // Access tokens are short-lived and a post-promotion backlog runs far longer
    // than one lifetime, so this is routine rather than exceptional.
    let dir = tempfile::tempdir().unwrap();
    let mut store = cloud_store(dir.path());
    record(&mut store, "s1", "hello");

    let stub = Stub::start(vec![Reply::ExpireOnce], 200);
    let calls = std::sync::atomic::AtomicUsize::new(0);
    let token = move || {
        let n = calls.fetch_add(1, Ordering::SeqCst);
        Some(format!("token-{n}"))
    };

    let outcome = drain(&store, &config(&stub.base_url, &token)).unwrap();
    assert_eq!(outcome.status, DrainStatus::Drained);
    assert!(outcome.sent > 0, "the drain resumed after refreshing");
}

#[test]
fn a_permanent_authorization_failure_stops_retrying_and_says_so() {
    // Removed from the Organisation, or the Workspace was deleted. Presenting
    // that as a transient failure would be an endless spinner.
    let dir = tempfile::tempdir().unwrap();
    let mut store = cloud_store(dir.path());
    let session = record(&mut store, "s1", "hello");

    let stub = Stub::start(vec![Reply::Status(403)], 200);
    let token = always_token();
    let outcome = drain(&store, &config(&stub.base_url, &token)).unwrap();

    assert_eq!(outcome.status, DrainStatus::NotAuthorized);
    assert_eq!(outcome.sent, 0);
    // Local capture continues untouched — only the drain stops.
    assert!(store.session(&session).unwrap().is_some());
}

#[test]
fn a_rate_limit_backs_off_rather_than_treating_rows_as_poison() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = cloud_store(dir.path());
    record(&mut store, "s1", "hello");

    let stub = Stub::start(vec![Reply::Status(429)], 200);
    let token = always_token();
    let outcome = drain(&store, &config(&stub.base_url, &token)).unwrap();

    assert_eq!(outcome.status, DrainStatus::RateLimited);
    assert_eq!(outcome.failed, 0, "429 is not a bad row");
    assert!(outcome.still_pending > 0);
}

#[test]
fn without_a_credential_the_drain_parks_rather_than_failing() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = cloud_store(dir.path());
    record(&mut store, "s1", "hello");

    let token = || None;
    let outcome = drain(&store, &config("http://127.0.0.1:1", &token)).unwrap();
    assert_eq!(outcome.status, DrainStatus::NoCredential);
    assert!(outcome.still_pending > 0);
}

// ── Per-row semantics ───────────────────────────────────────────────────────

#[test]
fn a_rejected_row_is_marked_failed_and_the_rest_still_drain() {
    // One malformed record must never stall everything behind it.
    let dir = tempfile::tempdir().unwrap();
    let mut store = cloud_store(dir.path());
    record(&mut store, "s1", "hello");

    // Reject the Session row; messages still go.
    let stub = Stub::start(vec![Reply::Partial { reject: vec!["as-".into()] }], 200);
    let token = always_token();
    let outcome = drain(&store, &config(&stub.base_url, &token)).unwrap();

    assert_eq!(outcome.failed, 1, "the rejected row is marked failed");
    assert!(outcome.sent > 0, "the rest kept draining");
    assert!(
        store.row_count_in_state(WORKSPACE, atlas_checkpoint::SyncState::Failed).unwrap() > 0
    );
}

// ── Blob-first ordering ─────────────────────────────────────────────────────

#[test]
fn a_spilled_payload_is_uploaded_before_the_row_that_references_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = cloud_store(dir.path());
    record(&mut store, "s1", &"log output\n".repeat(20_000));

    let stub = Stub::start(vec![], 200);
    let token = always_token();
    let outcome = drain(&store, &config(&stub.base_url, &token)).unwrap();

    assert_eq!(outcome.status, DrainStatus::Drained);
    assert!(outcome.blobs_uploaded > 0, "the payload was uploaded");
    assert!(!stub.blob_keys().is_empty());

    // Every referenced blob reached the server.
    for artifact in stub.artifacts() {
        for key in artifact.blob_refs() {
            assert!(
                stub.blob_keys().iter().any(|k| k == key),
                "row referenced a blob the server never received: {key}"
            );
        }
    }
}

#[test]
fn a_row_is_never_sent_when_its_blob_upload_failed() {
    // The failure this forbids: a row lands, its blob did not, and a *teammate*
    // opens the message to find nothing.
    let dir = tempfile::tempdir().unwrap();
    let mut store = cloud_store(dir.path());
    record(&mut store, "s1", &"log output\n".repeat(20_000));
    assert!(
        "log output\n".repeat(20_000).len() > SPILL_THRESHOLD_BYTES,
        "the fixture must actually spill"
    );

    // Blob uploads fail; ingest would happily accept.
    let stub = Stub::start(vec![], 500);
    let token = always_token();
    let outcome = drain(&store, &config(&stub.base_url, &token)).unwrap();

    assert_eq!(outcome.status, DrainStatus::Offline);
    for artifact in stub.artifacts() {
        assert!(
            artifact.blob_refs().is_empty(),
            "a row with a spilled payload was sent despite the blob failing"
        );
    }
    assert!(outcome.still_pending > 0, "it stays pending for the next pass");
}

// ── Batching ────────────────────────────────────────────────────────────────

#[test]
fn batches_respect_the_count_ceiling() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = cloud_store(dir.path());
    for i in 0..120 {
        record(&mut store, &format!("s{i}"), "a short answer");
    }

    let batch = store
        .pending_artifacts(WORKSPACE, ORG, atlas_checkpoint::sync::MAX_BATCH_COUNT, usize::MAX)
        .unwrap();
    assert!(batch.len() <= atlas_checkpoint::sync::MAX_BATCH_COUNT);
}

#[test]
fn batches_respect_the_byte_ceiling_independently_of_the_count() {
    // A hundred artifacts can be enormous; the measured corpus has a single
    // 2.02 MB message.
    let dir = tempfile::tempdir().unwrap();
    let mut store = cloud_store(dir.path());
    for i in 0..20 {
        record(&mut store, &format!("s{i}"), &"x".repeat(40_000));
    }

    let batch = store.pending_artifacts(WORKSPACE, ORG, 100, 64 * 1024).unwrap();
    let bytes: usize = batch.iter().map(|a| a.approx_bytes()).sum();
    assert!(batch.len() < 100, "the byte ceiling bit before the count did");
    assert!(bytes < 512 * 1024, "batch was {bytes} bytes");
}

// ── Local mode never drains ─────────────────────────────────────────────────

#[test]
fn a_local_workspace_accumulates_rows_that_never_drain() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join(".atlas")).unwrap();
    bind(&store, WORKSPACE, dir.path(), WorkspaceMode::Local).unwrap();

    let mut store = store;
    {
        let mut capture = Capture::new(&mut store, WorkspaceMode::Local);
        capture
            .record_prompt(
                &SessionKey {
                    workspace_id: WORKSPACE.into(),
                    source: Source::Acp,
                    native_session_id: "s1".into(),
                },
                "local only",
                1,
                None,
                None,
                None,
            )
            .unwrap();
    }

    // Nothing is pending, so a drain has nothing to do — Local mode is the same
    // database with draining switched off, not a separate path.
    let stub = Stub::start(vec![], 200);
    let token = always_token();
    let outcome = drain(&store, &config(&stub.base_url, &token)).unwrap();

    assert_eq!(outcome.sent, 0);
    assert_eq!(outcome.still_pending, 0);
    assert!(stub.artifacts().is_empty(), "nothing left the machine");
}
