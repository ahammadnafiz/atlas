//! One visible signal per Workspace, replacing nine invisible failures.
//!
//! Every failure path in this feature shares a design instinct — never block the
//! developer's work — which is correct. But "never block" plus "never tell"
//! equals a record with holes that nobody discovers until they need the history
//! and it is not there. The failures are real and each is silent on its own:
//!
//! * the commit cursor was garbage collected and detection recovered by re-scan;
//! * the disk filled and turns stopped being recorded;
//! * a second Atlas window holds the writer lock, so this one is not capturing;
//! * a redaction failure flagged a Session nobody is looking at;
//! * rows are stuck in a failed sync state;
//! * the git watcher died, so commits stop being linked.
//!
//! The fix is not error handling scattered across all of them. It is **one
//! state, per Workspace, with a reason** — computed here from what the store
//! already records, so the individual paths keep failing quietly and this is the
//! single place that says so.
//!
//! This module deliberately fixes none of the underlying failures; each has its
//! own criteria in its own ticket. It makes them visible.

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::model::SyncState;
use crate::store::Store;

/// How capture is doing for one Workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthState {
    /// Not recording *by choice* — never enabled, or deliberately paused.
    ///
    /// Ordered below `Ok` so it can never win the worst-first sort, though in
    /// practice `evaluate` returns it early and it never competes.
    Off,
    /// Recording, watcher attached, store writable.
    Ok,
    /// Still recording, but something needs attention.
    Degraded,
    /// Meant to be recording and is not. The reason says why.
    Stopped,
}

impl HealthState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Ok => "ok",
            Self::Degraded => "degraded",
            Self::Stopped => "stopped",
        }
    }
}

/// One thing that is wrong, and what to do about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthIssue {
    pub state: HealthState,
    /// What happened, in a sentence a developer can act on.
    pub reason: String,
    /// What to do next. Empty when it will clear by itself.
    pub next_step: String,
}

/// The whole picture for one Workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureHealth {
    /// The worst of the contributing conditions.
    pub state: HealthState,
    /// A one-line summary for the status indicator.
    pub summary: String,
    /// Every contributing condition, worst first. Clicking the indicator shows
    /// these — one signal to notice, the detail available when wanted.
    pub issues: Vec<HealthIssue>,
    /// Sessions flagged during capture (redaction or storage failure).
    pub flagged_sessions: i64,
    /// Rows the drain gave up on.
    pub failed_rows: i64,
    /// Rows waiting to be sent. Not a problem — shown so a developer knows
    /// whether their work has reached their team.
    pub pending_rows: i64,
}

/// What the host knows that the store cannot.
///
/// The crate is Tauri-free, and watcher liveness lives in the Tauri layer — so
/// it is passed in rather than guessed at. That distinction matters: **watcher
/// liveness must be checked against actual watcher state**, never inferred from
/// "no events lately", because a quiet repository and a dead watcher are
/// indistinguishable from the event stream.
#[derive(Debug, Clone, Copy, Default)]
pub struct HostSignals {
    /// Whether a git watcher is currently attached for this Workspace, as
    /// reported by the watcher registry itself.
    pub watcher_attached: bool,
    /// Whether this Workspace *needs* a watcher — a non-git Workspace never has
    /// one and is perfectly healthy without it.
    pub expects_watcher: bool,
}

/// Compute the current health of a Workspace.
///
/// Cheap: a handful of indexed counts. Intended to be called on Workspace open,
/// on turn completion, on watcher events and when the drain hits a terminal
/// error — not on a timer.
pub fn evaluate(store: &Store, workspace_id: &str, host: HostSignals) -> Result<CaptureHealth> {
    // A Workspace nobody enabled — or one the developer deliberately paused — is
    // *off*, not broken. Every check below assumes capture is meant to be
    // running, so falling through would report a missing watcher and an unheld
    // writer lock for something nobody asked to record: three alarms for a
    // Workspace whose only real state is "not set up yet". That turns the first
    // thing a new user sees into an incident.
    match store.binding()? {
        None => return off(store, workspace_id, "Session capture is off"),
        Some(binding) if !binding.is_capturing() => {
            return off(store, workspace_id, "Session capture is paused")
        }
        Some(_) => {}
    }

    let mut issues = Vec::new();

    // ── Stopped: not recording at all ───────────────────────────────────────

    if !store.is_writer() {
        issues.push(HealthIssue {
            state: HealthState::Stopped,
            reason: "Another Atlas window is recording this Workspace.".into(),
            next_step: "Sessions are being captured by the other window. Close it to record here."
                .into(),
        });
    }

    if let Err(err) = store.check_writable() {
        issues.push(HealthIssue {
            state: HealthState::Stopped,
            reason: format!("The session store cannot be written to: {err}"),
            next_step: "Check free disk space and the permissions on the .atlas directory.".into(),
        });
    }

    // Checked against real watcher state, not inferred from silence.
    if host.expects_watcher && !host.watcher_attached {
        issues.push(HealthIssue {
            state: HealthState::Stopped,
            reason: "The git watcher for this Workspace is not running, so commits are not being \
                     linked to Sessions."
                .into(),
            next_step: "Reopen the Workspace to restart it. Commits made meanwhile are picked up \
                        by the open-time walk."
                .into(),
        });
    }

    // ── Degraded: recording, but something needs attention ──────────────────

    let flagged_sessions = store.flagged_session_count(workspace_id)?;
    if flagged_sessions > 0 {
        issues.push(HealthIssue {
            state: HealthState::Degraded,
            reason: format!(
                "{flagged_sessions} Session{} could not be fully recorded.",
                plural(flagged_sessions)
            ),
            next_step: "Open the Session to see what was flagged. Content that could not be \
                        scrubbed was not stored."
                .into(),
        });
    }

    let failed_rows = store.row_count_in_state(workspace_id, SyncState::Failed)?;
    if failed_rows > 0 {
        issues.push(HealthIssue {
            state: HealthState::Degraded,
            reason: format!(
                "{failed_rows} record{} could not be sent to your Organisation.",
                plural(failed_rows)
            ),
            next_step: "They are skipped so the rest keep syncing. Retry from the sync status."
                .into(),
        });
    }

    if store.cursor_recovered(workspace_id)? {
        issues.push(HealthIssue {
            state: HealthState::Degraded,
            reason: "The commit history moved in a way that lost this Workspace's place, so \
                     recent commits were re-scanned."
                .into(),
            next_step: "No action needed. Some older commits may not have been linked.".into(),
        });
    }

    let pending_rows = store.row_count_in_state(workspace_id, SyncState::Pending)?;

    // Worst first, so the summary and the indicator agree.
    issues.sort_by_key(|issue| std::cmp::Reverse(issue.state));
    let state = issues.first().map(|i| i.state).unwrap_or(HealthState::Ok);

    Ok(CaptureHealth {
        state,
        summary: summarize(state, &issues, pending_rows),
        issues,
        flagged_sessions,
        failed_rows,
        pending_rows,
    })
}

/// Health for a Workspace that is not recording on purpose.
///
/// The counts are still reported: pausing does not discard what was already
/// captured, and a developer who paused with work still unsent should be able to
/// see that.
fn off(store: &Store, workspace_id: &str, summary: &str) -> Result<CaptureHealth> {
    Ok(CaptureHealth {
        state: HealthState::Off,
        summary: summary.into(),
        issues: Vec::new(),
        flagged_sessions: store.flagged_session_count(workspace_id)?,
        failed_rows: store.row_count_in_state(workspace_id, SyncState::Failed)?,
        pending_rows: store.row_count_in_state(workspace_id, SyncState::Pending)?,
    })
}

fn summarize(state: HealthState, issues: &[HealthIssue], pending_rows: i64) -> String {
    match state {
        // The healthy line still carries the one number a developer wants
        // continuously: has my work reached my team?
        HealthState::Ok if pending_rows > 0 => {
            format!("{pending_rows} pending")
        }
        HealthState::Ok => "Synced".into(),
        // Set by `off`, which passes its own summary and never reaches here.
        HealthState::Off => "Session capture is off".into(),
        // One issue reads better as itself than as a count of one.
        _ if issues.len() == 1 => issues[0].reason.clone(),
        _ => format!(
            "{} — {} issues need attention",
            if state == HealthState::Stopped { "Capture stopped" } else { "Capture degraded" },
            issues.len()
        ),
    }
}

fn plural(count: i64) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_ordering_puts_stopped_worst_and_off_below_healthy() {
        assert!(HealthState::Stopped > HealthState::Degraded);
        assert!(HealthState::Degraded > HealthState::Ok);
        // Off must never win the worst-first sort — being switched off is not a
        // fault, and ranking it above Ok would light the alarm on every
        // Workspace nobody has enabled yet.
        assert!(HealthState::Off < HealthState::Ok);
    }

    #[test]
    fn a_healthy_workspace_with_nothing_pending_reads_as_synced() {
        assert_eq!(summarize(HealthState::Ok, &[], 0), "Synced");
    }

    #[test]
    fn a_healthy_workspace_still_reports_its_pending_count() {
        assert_eq!(summarize(HealthState::Ok, &[], 47), "47 pending");
    }
}
