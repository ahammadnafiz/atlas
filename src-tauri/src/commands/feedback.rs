//! In-app feedback submission.
//!
//! One command, one PostHog event. The interesting decisions are all about
//! honesty:
//!
//! - **It sends even when "Share anonymous usage data" is off.** A button
//!   labelled "Send" that silently discards the user's bug report is a worse
//!   betrayal than the send itself. The panel says so in plain text, and every
//!   submission carries `telemetry_opt_in` so the consent state travels with it.
//!   An **inert** build still sends nothing — no key means no network, ever.
//!
//! - **The message is not redacted.** Every other free-text field here goes
//!   through `redact_message`, which strips path- and URL-shaped tokens. Doing
//!   that to a bug report would gut it: "it crashes when I open
//!   /Users/me/project" is the report. The user typed it into a box marked
//!   feedback and pressed Send.
//!
//! - **Identity is resolved here, not sent by the renderer.** The frontend
//!   passes a single `anonymous` bit; Rust reads the account from its own auth
//!   state. The renderer never handles the credential, and there is no way for
//!   a payload carrying a screenshot to also carry someone's email by accident.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::json;
use tauri::State;

use crate::auth::AuthSnapshot;
use crate::commands::auth::AuthState;
use crate::telemetry::TelemetryClient;

/// Cap on the message. Generous enough for a real report with a stack trace,
/// small enough that no single event can wedge ingest.
const MAX_MESSAGE: usize = 8_000;

/// Cap on the attached image, as base64 characters (~500 KB).
///
/// The renderer already downscales to ≤1280px JPEG before this is called; the
/// cap is the backstop for a screenshot that is still too large after that (a
/// 6K display of dense text). Over the cap the event still records that a
/// screenshot was taken and how big it was — it just doesn't carry the pixels,
/// which beats failing the whole submission over an attachment.
const MAX_SCREENSHOT_B64: usize = 500_000;

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackCategory {
    Issue,
    FeatureRequest,
    Improvement,
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackInput {
    pub category: FeedbackCategory,
    pub message: String,
    /// `true` to submit without the Atlas account even while signed in.
    #[serde(default)]
    pub anonymous: bool,
    /// Downscaled JPEG/PNG as a bare base64 string (no `data:` prefix).
    #[serde(default)]
    pub screenshot_base64: Option<String>,
    #[serde(default)]
    pub screenshot_mime_type: Option<String>,
    /// Where the panel was opened from, and what the user was looking at.
    /// Non-identifying: a tab *type* (`"chat"`), never a title or a path.
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub active_tab: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackReceipt {
    pub sent: bool,
    /// What actually happened, so the panel can say "sent anonymously" truthfully
    /// rather than echoing what was requested.
    pub anonymous: bool,
    /// True when a screenshot was attached but too large to carry.
    pub screenshot_dropped: bool,
}

/// Submit feedback as a `feedback_submitted` PostHog event.
///
/// Fails loudly — the panel surfaces the error and keeps the user's draft —
/// rather than pretending, which is the opposite of every other telemetry path
/// in Atlas and is the right call for something the user is watching.
#[tauri::command]
pub async fn feedback_submit(
    input: FeedbackInput,
    telemetry: State<'_, Arc<TelemetryClient>>,
    auth: State<'_, AuthState>,
) -> Result<FeedbackReceipt, String> {
    let message = input.message.trim();
    if message.is_empty() {
        return Err("Write something first.".into());
    }
    let message: String = message.chars().take(MAX_MESSAGE).collect();

    let shot = input
        .screenshot_base64
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let screenshot_bytes = shot.map(|s| (s.len() * 3) / 4).unwrap_or(0);
    let too_large = shot.is_some_and(|s| s.len() > MAX_SCREENSHOT_B64);

    // Identity, resolved from Rust's own auth state.
    let (account_id, account_email, active_org_id) = match (input.anonymous, auth.core().snapshot())
    {
        (
            false,
            AuthSnapshot::SignedIn {
                user: Some(user),
                active_org_id,
                ..
            },
        ) => (Some(user.id), Some(user.email), active_org_id),
        _ => (None, None, None),
    };
    let anonymous = account_id.is_none();

    let mut props = json!({
        "category": input.category,
        "message": message,
        "anonymous": anonymous,
        "has_screenshot": shot.is_some(),
        "screenshot_bytes": screenshot_bytes,
        "screenshot_dropped": too_large,
        "source": input.source,
        "active_tab": input.active_tab,
    });
    if let Some(map) = props.as_object_mut() {
        if let Some(id) = account_id {
            map.insert("account_id".into(), json!(id));
            map.insert("account_email".into(), json!(account_email));
            map.insert("active_org_id".into(), json!(active_org_id));
        }
        if let (Some(s), false) = (shot, too_large) {
            map.insert("screenshot_b64".into(), json!(s));
            map.insert(
                "screenshot_mime_type".into(),
                json!(input
                    .screenshot_mime_type
                    .as_deref()
                    .unwrap_or("image/jpeg")),
            );
        }
    }

    let client = Arc::clone(&telemetry);
    client
        .capture_user_initiated("feedback_submitted", props)
        .await?;

    Ok(FeedbackReceipt {
        sent: true,
        anonymous,
        screenshot_dropped: too_large,
    })
}
