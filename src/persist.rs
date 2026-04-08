//! State persistence: save and restore pseudoclient state across restarts.
//!
//! Persists only data that cannot be reconstructed from live sources:
//! channel memberships, activity timestamps, and offline transition times.
//! UIDs, nicks, usernames, display names, and presence are all reconstructed
//! from Discord events and IRC burst on startup.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::pseudoclients::PseudoclientManager;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum PersistError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported state file version: {0}")]
    UnsupportedVersion(u32),
}

// ---------------------------------------------------------------------------
// Serializable types
// ---------------------------------------------------------------------------

/// Current state file format version.
const STATE_VERSION: u32 = 1;

/// Top-level persisted state.
#[derive(Debug, Serialize, Deserialize)]
pub struct PersistedState {
    pub version: u32,
    /// Map of Discord user ID (as string) → persisted pseudoclient data.
    pub pseudoclients: HashMap<String, PersistedPseudoclient>,
}

/// Per-user persisted data.
///
/// Only fields that cannot be reconstructed from live Discord/IRC sources.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedPseudoclient {
    pub channels: Vec<String>,
    pub last_active: u64,
    #[serde(default)]
    pub channel_last_active: HashMap<String, u64>,
    #[serde(default)]
    pub went_offline_at: Option<u64>,
}

// ---------------------------------------------------------------------------
// Load / Save
// ---------------------------------------------------------------------------

/// Load persisted state from a JSON file.
///
/// Returns `Ok(state)` on success, or an error if the file cannot be read,
/// parsed, or has an unsupported version.  Callers should log the error and
/// fall back to empty state rather than aborting startup.
pub fn load_state(path: &Path) -> Result<PersistedState, PersistError> {
    let contents = std::fs::read_to_string(path)?;
    let state: PersistedState = serde_json::from_str(&contents)?;
    if state.version != STATE_VERSION {
        return Err(PersistError::UnsupportedVersion(state.version));
    }
    Ok(state)
}

/// Save state to a JSON file using atomic write (temp + fsync + rename).
///
/// Callers should log errors rather than aborting on failure.
pub fn save_state(path: &Path, state: &PersistedState) -> Result<(), PersistError> {
    let json = serde_json::to_string_pretty(state)?;

    let tmp_path = path.with_extension("json.tmp");

    let mut file = std::fs::File::create(&tmp_path)?;
    file.write_all(json.as_bytes())?;
    file.sync_all()?;

    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

/// Extract persistable state from the current `PseudoclientManager`.
pub fn snapshot_from_pm(pm: &PseudoclientManager) -> PersistedState {
    let mut pseudoclients = HashMap::new();

    for state in pm.iter_states() {
        // Skip pseudoclients pending reintroduction — they have no valid
        // IRC presence and will be reintroduced fresh.
        if state.needs_reintroduce {
            continue;
        }

        pseudoclients.insert(
            state.discord_user_id.to_string(),
            PersistedPseudoclient {
                channels: state.channels.clone(),
                last_active: state.last_active,
                channel_last_active: state.channel_last_active.clone(),
                went_offline_at: state.went_offline_at,
            },
        );
    }

    PersistedState {
        version: STATE_VERSION,
        pseudoclients,
    }
}

/// Convert a `PersistedState` into a seed map keyed by Discord user ID.
///
/// Entries for channels not in `valid_channels` are filtered out.
pub fn into_seed_map(
    state: PersistedState,
    valid_channels: &[&str],
) -> HashMap<u64, PersistedPseudoclient> {
    let mut seed = HashMap::new();

    for (id_str, mut pc) in state.pseudoclients {
        let Ok(discord_id) = id_str.parse::<u64>() else {
            continue;
        };

        // Filter channels against current bridge config.
        pc.channels
            .retain(|ch| valid_channels.contains(&ch.as_str()));
        pc.channel_last_active
            .retain(|ch, _| valid_channels.contains(&ch.as_str()));

        seed.insert(discord_id, pc);
    }

    seed
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_state() -> PersistedState {
        let mut pseudoclients = HashMap::new();
        let mut channel_last_active = HashMap::new();
        channel_last_active.insert("#general".to_string(), 1_700_000_000);

        pseudoclients.insert(
            "42".to_string(),
            PersistedPseudoclient {
                channels: vec!["#general".to_string(), "#dev".to_string()],
                last_active: 1_700_000_100,
                channel_last_active,
                went_offline_at: Some(1_700_000_050),
            },
        );

        PersistedState {
            version: STATE_VERSION,
            pseudoclients,
        }
    }

    #[test]
    fn roundtrip_save_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");

        let state = sample_state();
        save_state(&path, &state).unwrap();
        let loaded = load_state(&path).unwrap();

        assert_eq!(loaded.version, STATE_VERSION);
        assert_eq!(loaded.pseudoclients.len(), 1);
        let pc = &loaded.pseudoclients["42"];
        assert_eq!(pc.channels, vec!["#general", "#dev"]);
        assert_eq!(pc.last_active, 1_700_000_100);
        assert_eq!(pc.went_offline_at, Some(1_700_000_050));
        assert_eq!(pc.channel_last_active["#general"], 1_700_000_000);
    }

    #[test]
    fn load_missing_file_returns_error() {
        let result = load_state(Path::new("/nonexistent/state.json"));
        assert!(result.is_err());
        assert!(
            matches!(&result.unwrap_err(), PersistError::Io(_)),
            "expected Io error for missing file"
        );
    }

    #[test]
    fn load_corrupt_json_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, "not json at all {{{").unwrap();

        let result = load_state(&path);
        assert!(matches!(result.unwrap_err(), PersistError::Json(_)));
    }

    #[test]
    fn load_unsupported_version_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, r#"{"version": 99, "pseudoclients": {}}"#).unwrap();

        let result = load_state(&path);
        assert!(matches!(
            result.unwrap_err(),
            PersistError::UnsupportedVersion(99)
        ));
    }

    #[test]
    fn into_seed_map_filters_channels() {
        let state = sample_state();
        let seed = into_seed_map(state, &["#general"]);

        let pc = &seed[&42];
        assert_eq!(pc.channels, vec!["#general"]);
        assert!(pc.channel_last_active.contains_key("#general"));
        assert!(!pc.channel_last_active.contains_key("#dev"));
    }

    #[test]
    fn into_seed_map_skips_invalid_ids() {
        let mut state = sample_state();
        state.pseudoclients.insert(
            "not_a_number".to_string(),
            PersistedPseudoclient {
                channels: vec![],
                last_active: 0,
                channel_last_active: HashMap::new(),
                went_offline_at: None,
            },
        );

        let seed = into_seed_map(state, &["#general", "#dev"]);
        assert_eq!(seed.len(), 1, "invalid IDs should be skipped");
        assert!(seed.contains_key(&42));
    }

    #[test]
    fn snapshot_from_pm_captures_state() {
        let mut pm = PseudoclientManager::new("0D0", "discord");
        pm.introduce(
            42,
            "alice",
            "Alice",
            &[],
            1_000_000,
            crate::discord::DiscordPresence::Online,
        );
        pm.ensure_in_channel(42, "#general", 1_000_000);
        pm.record_activity(42, "#general", 1_000_100);

        let snapshot = snapshot_from_pm(&pm);
        assert_eq!(snapshot.version, STATE_VERSION);

        let pc = &snapshot.pseudoclients["42"];
        assert_eq!(pc.channels, vec!["#general"]);
        assert_eq!(pc.last_active, 1_000_100);
        assert_eq!(pc.channel_last_active["#general"], 1_000_100);
    }

    #[test]
    fn snapshot_skips_needs_reintroduce() {
        let mut pm = PseudoclientManager::new("0D0", "discord");
        pm.introduce(
            42,
            "alice",
            "Alice",
            &[],
            1_000_000,
            crate::discord::DiscordPresence::Online,
        );
        pm.mark_needs_reintroduce(42);

        let snapshot = snapshot_from_pm(&pm);
        assert!(
            snapshot.pseudoclients.is_empty(),
            "needs_reintroduce entries should be skipped"
        );
    }

    #[test]
    fn empty_state_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");

        let state = PersistedState {
            version: STATE_VERSION,
            pseudoclients: HashMap::new(),
        };
        save_state(&path, &state).unwrap();
        let loaded = load_state(&path).unwrap();
        assert!(loaded.pseudoclients.is_empty());
    }

    #[test]
    fn missing_optional_fields_deserialize_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        // JSON without channel_last_active and went_offline_at
        std::fs::write(
            &path,
            r##"{"version": 1, "pseudoclients": {"42": {"channels": ["#test"], "last_active": 100}}}"##,
        )
        .unwrap();

        let loaded = load_state(&path).unwrap();
        let pc = &loaded.pseudoclients["42"];
        assert!(pc.channel_last_active.is_empty());
        assert_eq!(pc.went_offline_at, None);
    }
}
