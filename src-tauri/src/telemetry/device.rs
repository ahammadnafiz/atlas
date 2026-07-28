//! Device-stable telemetry identity.
//!
//! One PostHog person per device. The id lives in `<app_config_dir>/device.json`
//! and is **Rust-owned**: it is never round-tripped through the frontend, which
//! is the whole point. Its predecessor, `AppState.telemetry_anon_id`, rode along
//! in `state.json` — and because `save_app_state` replaced the entire struct
//! with a frontend-built payload that omitted the field, every settings change
//! wiped it and the next launch minted a fresh id. A single install showed up in
//! PostHog as a crowd. See [`crate::state::AppStatePatch`] for the structural
//! half of that fix; this module is the other half.
//!
//! **Why a random UUID and not the machine's hardware id.** `IOPlatformUUID` /
//! `gethostuuid` is a cross-application deterministic fingerprint: every other
//! app on the machine derives the same value, it survives uninstall, and the
//! user has no way to reset it. That makes it personal data by construction, to
//! solve what was only ever a persistence bug. A random UUID in a file the user
//! can delete gives the same stability with none of that — and "delete
//! `device.json` to reset your analytics person" is a promise we can keep.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

/// On-disk shape of `device.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceIdentity {
    pub device_id: String,
    /// ISO-8601, for `$set_once: { atlas_first_seen }`.
    pub created_at: String,
    /// `"random"` — freshly minted. `"adopted"` — migrated from the legacy
    /// `state.json` `telemetryAnonId`, so an existing install keeps its person.
    pub source: String,
}

fn path(app: &AppHandle) -> Option<PathBuf> {
    app.path()
        .app_config_dir()
        .ok()
        .map(|d| d.join("device.json"))
}

/// Load the device identity, creating it on first run.
///
/// `adopt` is the legacy `state.json` `telemetry_anon_id`. When `device.json`
/// does not exist yet but that id does, it becomes the device id — an upgrading
/// install must not fork into a second PostHog person.
///
/// Returns `(identity, is_new_device)`. `is_new_device` is the honest
/// `is_first_launch` signal: the old one keyed off `telemetry_anon_id.is_none()`
/// which, thanks to the wipe bug, fired on roughly every launch after a settings
/// save.
///
/// Never panics and never blocks boot. An unresolvable config dir yields an
/// in-memory id for this run only.
pub fn load_or_create(app: &AppHandle, adopt: Option<&str>) -> (DeviceIdentity, bool) {
    let Some(file) = path(app) else {
        tracing::debug!(
            target: "atlas::telemetry",
            "no app_config_dir; using an in-memory device id for this run"
        );
        return (mint("random"), true);
    };

    if let Ok(raw) = std::fs::read_to_string(&file) {
        if let Ok(id) = serde_json::from_str::<DeviceIdentity>(&raw) {
            if !id.device_id.trim().is_empty() {
                return (id, false);
            }
        }
        // Present but unreadable/empty — fall through and rewrite it rather than
        // running id-less. A corrupt file must not cost the user their person
        // silently *and* forever.
        tracing::debug!(target: "atlas::telemetry", "device.json unreadable; regenerating");
    }

    let adopted = adopt.map(str::trim).filter(|s| !s.is_empty());
    let identity = match adopted {
        Some(legacy) => DeviceIdentity {
            device_id: legacy.to_string(),
            created_at: now_iso(),
            source: "adopted".into(),
        },
        None => mint("random"),
    };
    // A brand-new person only when we truly had nothing to carry over.
    let is_new = adopted.is_none();

    if let Err(e) = save(&file, &identity) {
        tracing::debug!(target: "atlas::telemetry", "device.json write failed: {e}");
    }
    (identity, is_new)
}

fn mint(source: &str) -> DeviceIdentity {
    DeviceIdentity {
        device_id: uuid::Uuid::new_v4().to_string(),
        created_at: now_iso(),
        source: source.to_string(),
    }
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Atomic write (tmp + rename), matching `AppState::save`.
fn save(file: &PathBuf, id: &DeviceIdentity) -> std::io::Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = file.with_extension("json.tmp");
    let raw = serde_json::to_string_pretty(id)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    std::fs::write(&tmp, raw)?;
    std::fs::rename(tmp, file)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The file I/O half, exercised without a `TauriHandle` (which
    /// `load_or_create` needs only to resolve the directory).
    fn read_back(file: &PathBuf) -> DeviceIdentity {
        serde_json::from_str(&std::fs::read_to_string(file).expect("device.json")).expect("parse")
    }

    #[test]
    fn save_then_read_round_trips() {
        let dir = std::env::temp_dir().join(format!("atlas-dev-{}", uuid::Uuid::new_v4()));
        let file = dir.join("device.json");
        let id = mint("random");
        save(&file, &id).expect("write");
        assert_eq!(read_back(&file).device_id, id.device_id);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn minted_ids_are_unique_and_tagged() {
        let a = mint("random");
        let b = mint("random");
        assert_ne!(a.device_id, b.device_id);
        assert_eq!(a.source, "random");
        assert!(!a.created_at.is_empty());
    }
}
