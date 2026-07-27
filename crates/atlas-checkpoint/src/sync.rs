//! Getting the record off the machine and into the Organisation.
//!
//! This is the only module here that touches the network, and everything about
//! its shape follows from one rule: **capture never waits for it**. Offline is
//! the ordinary case, not the error case. Rows accumulate as `pending`, the
//! drain moves them when it can, and nothing upstream ever blocks or reports a
//! failure because a connection was unavailable.
//!
//! # Ordering is blob-first, and that is a rule
//!
//! A row referencing a spilled payload is sent only after that payload is
//! confirmed in blob storage, and is marked `sent` only after the server
//! acknowledges the row. The failure this forbids: a row lands, its blob upload
//! failed, and a *teammate* opens the message to find nothing — a dangling
//! reference in a shared timeline, discovered by the reader rather than the
//! sender. The inverse leak (blob uploaded, row never sent) is tolerable: an
//! unreferenced object costs pennies, not trust, and is logged for a future GC.
//!
//! # Failure is a state machine, not an error
//!
//! * **Offline / 5xx** — rows stay pending, backoff, nothing surfaces.
//! * **429** — honoured with backoff. A rate limit is not a poison row.
//! * **401** — the access token expired. Refresh and continue *mid-drain*:
//!   tokens are short-lived and a post-promotion first drain runs far longer
//!   than one token lifetime.
//! * **403** — terminal. Removed from the Organisation, or the Workspace was
//!   deleted. Retrying forever would be a permanent spinner; this stops and
//!   says "no longer authorized". Local capture continues untouched.
//! * **A single bad row** — marked failed and skipped, so one malformed record
//!   never stalls everything behind it.

use std::time::Duration;

use crate::artifacts::*;
use crate::error::{Error, Result};
use crate::model::SyncState;
use crate::store::Store;

/// Most artifacts in one request. Matches the server's array cap.
pub const MAX_BATCH_COUNT: usize = 100;

/// Most bytes in one request.
///
/// A count ceiling alone is not enough — a hundred artifacts can be enormous,
/// and the measured corpus contains a single 2.02 MB message. Without this the
/// drain would build a request the server refuses, and retry it forever.
pub const MAX_BATCH_BYTES: usize = 4 * 1024 * 1024;

/// Attempts before a row is treated as poison and skipped.
pub const MAX_ATTEMPTS: i64 = 5;

/// Where the desktop sends artifacts.
///
/// Two rungs — environment variable, then a build-time default — matching the
/// ladder auth already uses. Deliberately **not** telemetry's three-rung ladder
/// with an on-disk override: a file that redirects where a developer's
/// transcripts are sent is a phishing foothold, and one stray write should not
/// be able to create it. An environment variable at least requires code
/// execution in the user's session.
pub fn ingest_base() -> String {
    const DEFAULT: &str = "https://ingest.tryatlas.cc";
    const BUILD: Option<&str> = option_env!("ATLAS_INGEST_URL");

    let raw = std::env::var("ATLAS_INGEST_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| BUILD.map(str::to_string))
        .unwrap_or_else(|| DEFAULT.to_string());
    raw.trim().trim_end_matches('/').to_string()
}

/// Why a drain stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrainStatus {
    /// Everything pending was sent.
    Drained,
    /// The server could not be reached, or returned 5xx. Rows stay pending and
    /// nothing is surfaced to the developer — this is the ordinary offline case.
    Offline,
    /// Rate limited. Back off and try later.
    RateLimited,
    /// No longer authorized for this Workspace — removed from the Organisation,
    /// or the Workspace was deleted server-side. Terminal: retrying is pointless
    /// and presenting it as a transient failure would be a lie. Local capture is
    /// unaffected; only the drain stops.
    NotAuthorized,
    /// There was no credential to send with.
    NoCredential,
}

/// What one drain pass did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrainOutcome {
    pub status: DrainStatus,
    pub sent: usize,
    /// Rows the server rejected permanently, marked failed and skipped.
    pub failed: usize,
    pub blobs_uploaded: usize,
    /// Blobs uploaded for a row that then failed permanently. Unreferenced
    /// objects, logged so a future GC can reap them.
    pub orphaned_blobs: usize,
    pub still_pending: i64,
}

/// Everything the drain needs from the host.
///
/// The base URL is injected rather than hardcoded so tests drive a real stub
/// server on loopback — the same reason the auth tests can exercise the real
/// client. The token is a closure because access tokens are short-lived and the
/// drain must be able to mint a fresh one *during* a long backlog rather than
/// failing at the first expiry.
pub struct SyncConfig<'a> {
    pub base_url: String,
    pub org_id: String,
    pub workspace_id: String,
    /// Mint or return a current access token. `None` means "not signed in",
    /// which parks the drain rather than failing it.
    pub token: &'a dyn Fn() -> Option<String>,
    pub timeout: Duration,
}

impl SyncConfig<'_> {
    fn client(&self) -> Result<reqwest::blocking::Client> {
        reqwest::blocking::Client::builder()
            .timeout(self.timeout)
            .build()
            .map_err(|e| Error::Storage(format!("http client: {e}")))
    }
}

/// Send everything pending for a Workspace.
///
/// Never blocks capture: it runs on its own thread and touches the store only
/// through short transactions. Safe to call repeatedly — the server dedupes on
/// `(seq, contentHash)`, so a replayed batch cannot duplicate rows.
pub fn drain(store: &Store, config: &SyncConfig<'_>) -> Result<DrainOutcome> {
    let mut outcome = DrainOutcome {
        status: DrainStatus::Drained,
        sent: 0,
        failed: 0,
        blobs_uploaded: 0,
        orphaned_blobs: 0,
        still_pending: 0,
    };

    let Some(mut token) = (config.token)() else {
        outcome.status = DrainStatus::NoCredential;
        outcome.still_pending = store.row_count_in_state(&config.workspace_id, SyncState::Pending)?;
        return Ok(outcome);
    };

    let client = config.client()?;

    loop {
        let batch = store.pending_artifacts(
            &config.workspace_id,
            &config.org_id,
            MAX_BATCH_COUNT,
            MAX_BATCH_BYTES,
        )?;
        if batch.is_empty() {
            break;
        }

        // Blob-first. A row is never sent before its payload is confirmed.
        let mut deferred = Vec::new();
        for artifact in &batch {
            for key in artifact.blob_refs() {
                match upload_blob(&client, config, &token, store, key) {
                    Ok(true) => outcome.blobs_uploaded += 1,
                    Ok(false) => {}
                    Err(status) => {
                        // The row stays pending. Sending it now would put a
                        // dangling reference in a shared timeline.
                        deferred.push(artifact.row_id().to_string());
                        if matches!(status, DrainStatus::Offline | DrainStatus::RateLimited) {
                            outcome.status = status;
                            outcome.still_pending = store
                                .row_count_in_state(&config.workspace_id, SyncState::Pending)?;
                            return Ok(outcome);
                        }
                    }
                }
            }
        }
        let ready: Vec<AtlasArtifact> = batch
            .into_iter()
            .filter(|a| !deferred.contains(&a.row_id().to_string()))
            .collect();
        if ready.is_empty() {
            break;
        }

        match push(&client, config, &token, &ready) {
            Push::Accepted(results) => {
                for artifact in &ready {
                    let accepted = results
                        .iter()
                        .find(|r| r.row_id == artifact.row_id())
                        // A server that returns no per-artifact results has
                        // accepted the whole batch — the 202 contract.
                        .map(|r| r.accepted)
                        .unwrap_or(true);
                    if accepted {
                        store.mark_sent(artifact.row_id())?;
                        outcome.sent += 1;
                    } else {
                        store.mark_failed(artifact.row_id())?;
                        outcome.failed += 1;
                        outcome.orphaned_blobs += artifact.blob_refs().len();
                    }
                }
            }
            Push::Unauthenticated => {
                // Short-lived tokens expire mid-drain as a matter of course:
                // a post-promotion backlog runs far longer than one lifetime.
                // Refresh once and retry the same batch.
                match (config.token)() {
                    Some(fresh) if fresh != token => {
                        token = fresh;
                        continue;
                    }
                    _ => {
                        outcome.status = DrainStatus::NoCredential;
                        break;
                    }
                }
            }
            Push::Forbidden => {
                outcome.status = DrainStatus::NotAuthorized;
                break;
            }
            Push::RateLimited => {
                outcome.status = DrainStatus::RateLimited;
                break;
            }
            Push::Rejected => {
                // The whole batch was refused without per-artifact detail.
                // Bisect rather than give up: the alternative is failing a
                // hundred good rows because one is malformed. A single-artifact
                // batch that is still refused is the poison row itself.
                if ready.len() == 1 {
                    store.record_attempt(ready[0].row_id())?;
                    if store.attempts(ready[0].row_id())? >= MAX_ATTEMPTS {
                        store.mark_failed(ready[0].row_id())?;
                        outcome.failed += 1;
                        outcome.orphaned_blobs += ready[0].blob_refs().len();
                    } else {
                        // Leave it pending but stop this pass, so one bad row
                        // cannot spin the loop.
                        outcome.status = DrainStatus::Offline;
                        break;
                    }
                } else {
                    for artifact in &ready {
                        store.record_attempt(artifact.row_id())?;
                    }
                    // The next pass takes a smaller batch, converging on the
                    // offending row.
                    outcome.status = DrainStatus::Offline;
                    break;
                }
            }
            Push::Unreachable => {
                outcome.status = DrainStatus::Offline;
                break;
            }
        }
    }

    outcome.still_pending = store.row_count_in_state(&config.workspace_id, SyncState::Pending)?;
    Ok(outcome)
}

/// The server's verdict on one push.
enum Push {
    Accepted(Vec<ArtifactResult>),
    /// 401 — the token expired.
    Unauthenticated,
    /// 403 — terminal.
    Forbidden,
    RateLimited,
    /// 4xx that is not an auth problem: the payload itself was refused.
    Rejected,
    /// No response at all, or 5xx.
    Unreachable,
}

fn push(
    client: &reqwest::blocking::Client,
    config: &SyncConfig<'_>,
    token: &str,
    artifacts: &[AtlasArtifact],
) -> Push {
    let response = client
        .post(format!("{}/ingest", config.base_url))
        .bearer_auth(token)
        .json(&IngestRequest { artifacts: artifacts.to_vec() })
        .send();

    let Ok(response) = response else {
        return Push::Unreachable;
    };

    let status = response.status();
    if status.is_success() {
        // A body with per-artifact results is preferred; its absence means the
        // whole batch was accepted.
        return Push::Accepted(
            response
                .json::<IngestResponse>()
                .map(|r| r.results)
                .unwrap_or_default(),
        );
    }
    match status.as_u16() {
        401 => Push::Unauthenticated,
        403 => Push::Forbidden,
        429 => Push::RateLimited,
        // 5xx is the server's problem, not this row's.
        500..=599 => Push::Unreachable,
        _ => Push::Rejected,
    }
}

/// Upload one blob if the server does not already have it.
///
/// Returns `Ok(true)` when it was uploaded, `Ok(false)` when it was already
/// there, and `Err(status)` when it could not be.
fn upload_blob(
    client: &reqwest::blocking::Client,
    config: &SyncConfig<'_>,
    token: &str,
    store: &Store,
    key: &str,
) -> std::result::Result<bool, DrainStatus> {
    let Ok(bytes) = store.blobs().get(key) else {
        // The blob is missing locally. Nothing can be done about it here, and
        // blocking the whole drain on one lost payload helps nobody.
        return Err(DrainStatus::NotAuthorized);
    };

    let response = client
        .put(format!("{}/blobs/{key}", config.base_url))
        .bearer_auth(token)
        .body(bytes)
        .send();

    let Ok(response) = response else {
        return Err(DrainStatus::Offline);
    };
    let status = response.status();
    if status.is_success() {
        return Ok(true);
    }
    match status.as_u16() {
        // Content-addressed: already present is success.
        409 => Ok(false),
        401 | 403 => Err(DrainStatus::NotAuthorized),
        429 => Err(DrainStatus::RateLimited),
        _ => Err(DrainStatus::Offline),
    }
}

// ── Workspace registration (ATL-86) ─────────────────────────────────────────

/// Whether a Slug is free within an Organisation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlugAvailability {
    Available,
    Taken,
    /// The check itself could not be made. Distinct from `Taken`, because
    /// telling a developer their name is taken when the network merely blinked
    /// is a lie they will act on.
    Unknown,
}

/// Ask whether `slug` is free.
///
/// Called while typing, debounced by the caller, so the answer arrives before
/// the developer commits to a name rather than as a rejection afterwards.
pub fn check_slug(config: &SyncConfig<'_>, slug: &str) -> SlugAvailability {
    let Some(token) = (config.token)() else {
        return SlugAvailability::Unknown;
    };
    let Ok(client) = config.client() else {
        return SlugAvailability::Unknown;
    };

    let response = client
        .get(format!("{}/workspaces/slug-available", config.base_url))
        .bearer_auth(token)
        .query(&[("orgId", config.org_id.as_str()), ("slug", slug)])
        .send();

    match response {
        Ok(response) if response.status().is_success() => response
            .json::<serde_json::Value>()
            .ok()
            .and_then(|v| v.get("available").and_then(serde_json::Value::as_bool))
            .map(|available| {
                if available {
                    SlugAvailability::Available
                } else {
                    SlugAvailability::Taken
                }
            })
            .unwrap_or(SlugAvailability::Unknown),
        Ok(response) if response.status() == 409 => SlugAvailability::Taken,
        _ => SlugAvailability::Unknown,
    }
}

/// Register a Workspace with an Organisation.
///
/// **Server first.** The Slug is globally unique within the Organisation, so it
/// has to be settled server-side before anything local changes — otherwise a
/// rejected Slug leaves a half-bound Workspace behind. The identity signals
/// travel as advisory data: the server must accept a registration with neither.
pub fn register_workspace(
    config: &SyncConfig<'_>,
    slug: &str,
    root_commit_sha: Option<&str>,
    git_url: Option<&str>,
) -> Result<String> {
    let token = (config.token)()
        .ok_or_else(|| Error::Storage("not signed in".into()))?;
    let client = config.client()?;

    let response = client
        .post(format!("{}/workspaces", config.base_url))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "orgId": config.org_id,
            "slug": slug,
            "rootCommitSha": root_commit_sha,
            "gitUrl": git_url,
        }))
        .send()
        .map_err(|e| Error::Storage(format!("register workspace: {e}")))?;

    let status = response.status();
    if status == 409 {
        // Taken between the check and the confirm. Told plainly, and nothing
        // local has changed.
        return Err(Error::Storage(format!("the slug \"{slug}\" is already taken")));
    }
    if !status.is_success() {
        return Err(Error::Storage(format!(
            "register workspace: server returned {status}"
        )));
    }

    response
        .json::<serde_json::Value>()
        .ok()
        .and_then(|v| {
            v.get("workspaceId")
                .or_else(|| v.get("id"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .ok_or_else(|| Error::Storage("register workspace: no id in response".into()))
}

/// A Workspace as the Organisation knows it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteWorkspace {
    pub id: String,
    pub slug: String,
    /// Advisory fingerprints, as whoever created the Workspace supplied them.
    #[serde(default)]
    pub root_commit_sha: Option<String>,
    #[serde(default)]
    pub git_url: Option<String>,
}

/// Which Workspace the popover should pre-select, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Preselection {
    /// Exactly one Workspace matches. One click.
    One { workspace: RemoteWorkspace, reason: MatchReason },
    /// Several matched. **Pre-select nothing** and show them all.
    ///
    /// Every repository created from the same GitHub template shares a root
    /// commit, so a fingerprint match is *not* proof of the same project. A
    /// confident wrong pre-selection is worse than none: connecting the wrong
    /// directory pollutes a **shared** Organisation timeline with foreign
    /// Sessions, and nobody notices until someone reads them.
    Ambiguous { candidates: Vec<RemoteWorkspace> },
    /// Nothing matched. Warn, then trust the human — a shallow clone, a squashed
    /// history, a fresh repository joining existing work and a monorepo
    /// subdirectory are all legitimate, and a hard block would break every one.
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchReason {
    /// Same root commit. Survives forks, folder renames and remote changes.
    Fingerprint,
    /// Same normalised origin, where the fingerprint did not match.
    OriginUrl,
}

/// Decide what to pre-select for a local repository.
///
/// Pure, so the rule that actually matters — ambiguity pre-selects nothing — is
/// testable without a server.
pub fn preselect(
    workspaces: &[RemoteWorkspace],
    root_commit_sha: Option<&str>,
    git_url: Option<&str>,
) -> Preselection {
    // The fingerprint is tried first because it is the identity that survives a
    // fork: same root commit, different origin URL.
    if let Some(sha) = root_commit_sha.filter(|s| !s.is_empty()) {
        let matches: Vec<RemoteWorkspace> = workspaces
            .iter()
            .filter(|w| w.root_commit_sha.as_deref() == Some(sha))
            .cloned()
            .collect();
        match matches.len() {
            1 => {
                return Preselection::One {
                    workspace: matches[0].clone(),
                    reason: MatchReason::Fingerprint,
                }
            }
            0 => {}
            _ => return Preselection::Ambiguous { candidates: matches },
        }
    }

    if let Some(url) = git_url.filter(|u| !u.is_empty()) {
        let matches: Vec<RemoteWorkspace> = workspaces
            .iter()
            .filter(|w| w.git_url.as_deref() == Some(url))
            .cloned()
            .collect();
        match matches.len() {
            1 => {
                return Preselection::One {
                    workspace: matches[0].clone(),
                    reason: MatchReason::OriginUrl,
                }
            }
            0 => {}
            _ => return Preselection::Ambiguous { candidates: matches },
        }
    }

    Preselection::None
}

/// Every Workspace in the Organisation the developer can reach.
pub fn list_workspaces(config: &SyncConfig<'_>) -> Result<Vec<RemoteWorkspace>> {
    let token = (config.token)().ok_or_else(|| Error::Storage("not signed in".into()))?;
    let client = config.client()?;

    let response = client
        .get(format!("{}/workspaces", config.base_url))
        .bearer_auth(token)
        .query(&[("orgId", config.org_id.as_str())])
        .send()
        .map_err(|e| Error::Storage(format!("list workspaces: {e}")))?;

    if !response.status().is_success() {
        return Err(Error::Storage(format!(
            "list workspaces: server returned {}",
            response.status()
        )));
    }
    response
        .json::<serde_json::Value>()
        .ok()
        .and_then(|v| {
            v.get("workspaces")
                .cloned()
                .and_then(|w| serde_json::from_value(w).ok())
        })
        .ok_or_else(|| Error::Storage("list workspaces: unreadable response".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(id: &str, sha: Option<&str>, url: Option<&str>) -> RemoteWorkspace {
        RemoteWorkspace {
            id: id.to_string(),
            slug: id.to_string(),
            root_commit_sha: sha.map(str::to_string),
            git_url: url.map(str::to_string),
        }
    }

    #[test]
    fn a_single_fingerprint_match_preselects_it() {
        let all = vec![
            workspace("atlas", Some("root-1"), Some("github.com/tryatlas/atlas")),
            workspace("server", Some("root-2"), Some("github.com/tryatlas/server")),
        ];
        assert!(matches!(
            preselect(&all, Some("root-1"), None),
            Preselection::One { reason: MatchReason::Fingerprint, .. }
        ));
    }

    #[test]
    fn a_fork_preselects_correctly_despite_a_different_origin() {
        // Same root commit, different remote — the case the fingerprint exists
        // for.
        let all = vec![workspace("atlas", Some("root-1"), Some("github.com/tryatlas/atlas"))];
        let chosen = preselect(&all, Some("root-1"), Some("github.com/nafiz/atlas-fork"));
        assert!(matches!(
            chosen,
            Preselection::One { reason: MatchReason::Fingerprint, .. }
        ));
    }

    #[test]
    fn an_origin_match_preselects_when_the_fingerprint_does_not() {
        // A squashed history has a different root commit but the same remote.
        let all = vec![workspace("atlas", Some("other-root"), Some("github.com/tryatlas/atlas"))];
        assert!(matches!(
            preselect(&all, Some("root-1"), Some("github.com/tryatlas/atlas")),
            Preselection::One { reason: MatchReason::OriginUrl, .. }
        ));
    }

    #[test]
    fn several_fingerprint_matches_preselect_nothing() {
        // Every repository made from the same GitHub template shares a root
        // commit, so a match is not proof. A confident wrong answer here
        // pollutes a shared timeline with foreign Sessions.
        let all = vec![
            workspace("from-template-a", Some("template-root"), None),
            workspace("from-template-b", Some("template-root"), None),
        ];
        match preselect(&all, Some("template-root"), None) {
            Preselection::Ambiguous { candidates } => {
                assert_eq!(candidates.len(), 2, "all matches are shown");
            }
            other => panic!("expected no pre-selection, got {other:?}"),
        }
    }

    #[test]
    fn nothing_matching_preselects_nothing_but_does_not_block() {
        assert_eq!(
            preselect(&[workspace("atlas", Some("root-1"), None)], Some("unrelated"), None),
            Preselection::None
        );
    }

    #[test]
    fn a_repository_with_no_signals_at_all_preselects_nothing() {
        // A shallow clone with no remote, or a directory that is not a
        // repository. Still connectable — the developer chooses.
        assert_eq!(
            preselect(&[workspace("atlas", Some("root-1"), None)], None, None),
            Preselection::None
        );
    }

    /// One test, because these mutate a process-global and `cargo test` runs
    /// tests in parallel — split apart they race each other and fail at random.
    #[test]
    fn the_ingest_base_resolves_through_its_ladder() {
        std::env::set_var("ATLAS_INGEST_URL", "http://127.0.0.1:9999/");
        assert_eq!(
            ingest_base(),
            "http://127.0.0.1:9999",
            "an override wins, with no trailing slash"
        );

        std::env::set_var("ATLAS_INGEST_URL", "   ");
        assert!(
            ingest_base().starts_with("https://"),
            "an empty override falls through rather than pointing at nothing"
        );

        std::env::remove_var("ATLAS_INGEST_URL");
    }
}
