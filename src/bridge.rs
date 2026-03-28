use crate::config::BridgeEntry;

// ---------------------------------------------------------------------------
// BridgeMap
// ---------------------------------------------------------------------------

/// Immutable snapshot of one bridge entry as seen by the routing layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeInfo {
    /// Discord channel ID (numeric).
    pub discord_channel_id: u64,
    /// IRC channel name (e.g. `#general`).
    pub irc_channel: String,
    /// Optional webhook URL for the preferred send path.
    pub webhook_url: Option<String>,
}

/// Bidirectional channel routing table.
///
/// Built from `[[bridge]]` config entries.  Provides O(1) lookups in both
/// directions.  Replaced atomically on config reload — the processing task
/// swaps the whole map rather than mutating it in place.
#[derive(Debug, Default, Clone)]
pub struct BridgeMap {
    /// Discord channel ID → bridge info.
    by_discord: std::collections::HashMap<u64, BridgeInfo>,
    /// IRC channel name (lowercased) → discord channel ID (for reverse lookup).
    by_irc: std::collections::HashMap<String, u64>,
}

impl BridgeMap {
    /// Build a `BridgeMap` from a slice of config bridge entries.
    ///
    /// Entries with an unparseable `discord_channel_id` are silently skipped
    /// (config validation should have already rejected them).
    #[must_use]
    pub fn from_config(bridges: &[BridgeEntry]) -> Self {
        let mut map = Self::default();
        for entry in bridges {
            let Ok(discord_id) = entry.discord_channel_id.parse::<u64>() else {
                continue;
            };
            let info = BridgeInfo {
                discord_channel_id: discord_id,
                irc_channel: entry.irc_channel.clone(),
                webhook_url: entry.webhook_url.clone(),
            };
            map.by_irc
                .insert(entry.irc_channel.to_lowercase(), discord_id);
            map.by_discord.insert(discord_id, info);
        }
        map
    }

    /// Look up a bridge by Discord channel ID.
    #[must_use]
    pub fn by_discord_id(&self, id: u64) -> Option<&BridgeInfo> {
        self.by_discord.get(&id)
    }

    /// Look up a bridge by IRC channel name (case-insensitive).
    #[must_use]
    pub fn by_irc_channel(&self, channel: &str) -> Option<&BridgeInfo> {
        self.by_irc
            .get(&channel.to_lowercase())
            .and_then(|id| self.by_discord.get(id))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(discord_id: &str, irc: &str, webhook: Option<&str>) -> BridgeEntry {
        BridgeEntry {
            discord_channel_id: discord_id.to_string(),
            irc_channel: irc.to_string(),
            webhook_url: webhook.map(str::to_string),
        }
    }

    #[test]
    fn lookup_by_discord_id_finds_entry() {
        let map = BridgeMap::from_config(&[entry("111", "#general", None)]);
        let info = map.by_discord_id(111).expect("should find entry");
        assert_eq!(info.irc_channel, "#general");
        assert_eq!(info.webhook_url, None);
    }

    #[test]
    fn lookup_by_irc_channel_finds_entry() {
        let map = BridgeMap::from_config(&[entry(
            "222",
            "#lobby",
            Some("https://discord.com/api/webhooks/99/tok"),
        )]);
        let info = map.by_irc_channel("#lobby").expect("should find entry");
        assert_eq!(info.discord_channel_id, 222);
        assert_eq!(
            info.webhook_url.as_deref(),
            Some("https://discord.com/api/webhooks/99/tok")
        );
    }

    #[test]
    fn irc_lookup_is_case_insensitive() {
        let map = BridgeMap::from_config(&[entry("333", "#General", None)]);
        assert!(map.by_irc_channel("#general").is_some());
        assert!(map.by_irc_channel("#GENERAL").is_some());
        assert!(map.by_irc_channel("#General").is_some());
    }

    #[test]
    fn unknown_discord_id_returns_none() {
        let map = BridgeMap::from_config(&[entry("111", "#general", None)]);
        assert!(map.by_discord_id(999).is_none());
    }

    #[test]
    fn unknown_irc_channel_returns_none() {
        let map = BridgeMap::from_config(&[entry("111", "#general", None)]);
        assert!(map.by_irc_channel("#other").is_none());
    }

    #[test]
    fn unparseable_discord_id_is_skipped() {
        let bad = BridgeEntry {
            discord_channel_id: "not-a-number".to_string(),
            irc_channel: "#test".to_string(),
            webhook_url: None,
        };
        let map = BridgeMap::from_config(&[bad]);
        assert!(map.by_irc_channel("#test").is_none());
    }

    #[test]
    fn multiple_entries_all_accessible() {
        let map = BridgeMap::from_config(&[
            entry("100", "#alpha", None),
            entry(
                "200",
                "#beta",
                Some("https://discord.com/api/webhooks/1/tok"),
            ),
        ]);
        assert!(map.by_discord_id(100).is_some());
        assert!(map.by_discord_id(200).is_some());
        assert!(map.by_irc_channel("#alpha").is_some());
        assert!(map.by_irc_channel("#beta").is_some());
    }

    #[test]
    fn from_config_empty_slice_gives_empty_map() {
        let map = BridgeMap::from_config(&[]);
        assert!(map.by_discord_id(1).is_none());
        assert!(map.by_irc_channel("#x").is_none());
    }
}
