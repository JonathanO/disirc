use chrono::{DateTime, Utc};

use crate::config::BridgeEntry;
use crate::formatting::DiscordResolver;
use crate::irc::S2SCommand;

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
// Discord → IRC relay
// ---------------------------------------------------------------------------

/// Build the `S2SCommand`s needed to relay a Discord message to IRC.
///
/// Returns an empty vec if the message should be skipped:
/// - Content is whitespace-only **and** there are no attachments.
///
/// Each formatted text line becomes one `S2SCommand::SendMessage`.
/// Attachment URLs follow as additional `SendMessage` commands, in order.
pub fn discord_to_irc_commands(
    uid: &str,
    irc_channel: &str,
    content: &str,
    attachments: &[String],
    timestamp: Option<DateTime<Utc>>,
    resolver: &dyn DiscordResolver,
) -> Vec<S2SCommand> {
    let trimmed = content.trim();
    if trimmed.is_empty() && attachments.is_empty() {
        return vec![];
    }

    let mut commands = Vec::new();

    // Text lines (skipped if content is whitespace-only)
    if !trimmed.is_empty() {
        let lines = crate::formatting::discord_to_irc(content, resolver);
        for line in lines {
            commands.push(S2SCommand::SendMessage {
                from_uid: uid.to_string(),
                target: irc_channel.to_string(),
                text: line,
                timestamp,
            });
        }
    }

    // Attachment URLs (one PRIVMSG each)
    for url in attachments {
        commands.push(S2SCommand::SendMessage {
            from_uid: uid.to_string(),
            target: irc_channel.to_string(),
            text: url.clone(),
            timestamp,
        });
    }

    commands
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use crate::formatting::DiscordResolver;
    use crate::irc::S2SCommand;

    struct NullResolver;
    impl DiscordResolver for NullResolver {
        fn resolve_user(&self, _: &str) -> Option<String> {
            None
        }
        fn resolve_channel(&self, _: &str) -> Option<String> {
            None
        }
        fn resolve_role(&self, _: &str) -> Option<String> {
            None
        }
    }

    // --- discord_to_irc_commands ---

    #[test]
    fn empty_content_no_attachments_returns_empty() {
        let cmds = discord_to_irc_commands("uid1", "#chan", "", &[], None, &NullResolver);
        assert!(cmds.is_empty());
    }

    #[test]
    fn whitespace_only_content_no_attachments_returns_empty() {
        let cmds = discord_to_irc_commands("uid1", "#chan", "   \n  ", &[], None, &NullResolver);
        assert!(cmds.is_empty());
    }

    #[test]
    fn simple_text_produces_one_privmsg() {
        let cmds = discord_to_irc_commands("uid1", "#chan", "hello", &[], None, &NullResolver);
        assert_eq!(cmds.len(), 1);
        assert!(matches!(
            &cmds[0],
            S2SCommand::SendMessage { from_uid, target, text, timestamp: None }
            if from_uid == "uid1" && target == "#chan" && text == "hello"
        ));
    }

    #[test]
    fn attachment_only_produces_one_privmsg() {
        let urls = vec!["https://cdn.discord.com/x.png".to_string()];
        let cmds = discord_to_irc_commands("uid1", "#chan", "", &urls, None, &NullResolver);
        assert_eq!(cmds.len(), 1);
        assert!(matches!(
            &cmds[0],
            S2SCommand::SendMessage { text, .. } if text == "https://cdn.discord.com/x.png"
        ));
    }

    #[test]
    fn text_then_attachments_in_order() {
        let urls = vec![
            "https://cdn.discord.com/a.png".to_string(),
            "https://cdn.discord.com/b.png".to_string(),
        ];
        let cmds =
            discord_to_irc_commands("uid1", "#chan", "look at this", &urls, None, &NullResolver);
        assert_eq!(cmds.len(), 3);
        assert!(matches!(&cmds[0], S2SCommand::SendMessage { text, .. } if text == "look at this"));
        assert!(matches!(&cmds[1], S2SCommand::SendMessage { text, .. } if text.contains("a.png")));
        assert!(matches!(&cmds[2], S2SCommand::SendMessage { text, .. } if text.contains("b.png")));
    }

    #[test]
    fn multiline_content_produces_multiple_privmsgs() {
        let cmds = discord_to_irc_commands(
            "uid1",
            "#chan",
            "line one\nline two",
            &[],
            None,
            &NullResolver,
        );
        assert!(
            cmds.len() >= 2,
            "expected at least 2 commands, got {}",
            cmds.len()
        );
    }

    #[test]
    fn timestamp_is_propagated_to_all_commands() {
        use chrono::TimeZone;
        let ts = chrono::Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let urls = vec!["https://cdn.discord.com/x.png".to_string()];
        let cmds = discord_to_irc_commands("uid1", "#chan", "hi", &urls, Some(ts), &NullResolver);
        for cmd in &cmds {
            assert!(
                matches!(
                    cmd,
                    S2SCommand::SendMessage {
                        timestamp: Some(_),
                        ..
                    }
                ),
                "expected timestamp in {cmd:?}"
            );
        }
    }

    // --- BridgeMap ---

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
