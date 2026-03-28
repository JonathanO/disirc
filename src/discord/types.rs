/// Presence status of a Discord user, translated from serenity's `OnlineStatus`.
///
/// `OnlineStatus` is `#[non_exhaustive]`; any unrecognised variant maps to
/// [`DiscordPresence::Offline`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscordPresence {
    Online,
    Idle,
    DoNotDisturb,
    /// Covers `Offline`, `Invisible`, and any future unknown variants.
    Offline,
}

impl DiscordPresence {
    /// Returns `true` for any presence that should be represented on IRC
    /// (online, idle, do-not-disturb).  Returns `false` only for `Offline`,
    /// which is used to exclude members from the initial burst and from cache
    /// snapshots on config reload.
    #[must_use]
    pub fn is_non_offline(self) -> bool {
        self != Self::Offline
    }

    /// Returns the IRC `AWAY` message text for this presence, or `None` if the
    /// user is considered online (`DiscordPresence::Online`).
    #[must_use]
    pub fn away_message(self) -> Option<&'static str> {
        match self {
            Self::Online => None,
            Self::Idle => Some("idle"),
            Self::DoNotDisturb => Some("do not disturb"),
            Self::Offline => Some("offline"),
        }
    }
}

/// A snapshot of a single guild member, used to populate the IRC burst.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberInfo {
    pub user_id: u64,
    /// Display name resolved as: guild nickname → global display name → username.
    pub display_name: String,
    pub presence: DiscordPresence,
}

/// Protocol-agnostic events emitted by the Discord connection module to the
/// processing task over an `mpsc` channel.
#[derive(Debug, Clone, PartialEq)]
pub enum DiscordEvent {
    /// A message was received in a bridged channel and passed self-message
    /// filtering.
    MessageReceived {
        channel_id: u64,
        author_id: u64,
        author_name: String,
        content: String,
        /// CDN URLs of any attachments, in order.
        attachments: Vec<String>,
    },
    /// A user's presence changed.
    PresenceUpdated {
        user_id: u64,
        guild_id: u64,
        presence: DiscordPresence,
    },
    /// A new member joined a guild that has at least one bridged channel.
    MemberAdded {
        user_id: u64,
        guild_id: u64,
        display_name: String,
    },
    /// A member left or was removed from a guild.
    MemberRemoved { user_id: u64, guild_id: u64 },
    /// Initial member snapshot delivered once per guild after `guild_create`.
    /// Used to populate the IRC burst.
    MemberSnapshot {
        guild_id: u64,
        members: Vec<MemberInfo>,
        /// Discord channel IDs in this guild that have a `[[bridge]]` entry.
        /// Supplied so the bridge loop can derive the IRC channel list for this
        /// guild without needing direct access to the Discord cache.
        channel_ids: Vec<u64>,
    },
}

/// Commands sent to the Discord connection module by the bridging layer.
#[derive(Debug, Clone, PartialEq)]
pub enum DiscordCommand {
    /// Send a message to a Discord channel on behalf of an IRC user.
    SendMessage {
        channel_id: u64,
        /// If present, deliver via this webhook URL; otherwise fall back to
        /// plain `channel.send()`.
        webhook_url: Option<String>,
        /// IRC nick of the sender. The send layer enforces the 2–32 char
        /// Discord webhook username constraint.
        sender_nick: String,
        text: String,
    },
    /// Notify the Discord module that the bridge configuration has changed.
    ///
    /// The Discord module will update its routing tables and self-message
    /// filter, and will fetch a member snapshot for any newly added channels.
    ReloadBridges {
        /// Channel IDs to begin routing `MESSAGE_CREATE` events for.
        added_channel_ids: Vec<u64>,
        /// Channel IDs to stop routing `MESSAGE_CREATE` events for.
        removed_channel_ids: Vec<u64>,
        /// Webhook user IDs to add to the self-message filter.
        added_webhook_ids: Vec<u64>,
        /// Webhook user IDs to remove from the self-message filter.
        /// Only removed if no remaining bridge still uses the same webhook ID.
        removed_webhook_ids: Vec<u64>,
    },
}

/// Extract the numeric webhook ID from a Discord webhook URL.
///
/// Supports `discord.com`, `canary.discord.com`, and `ptb.discord.com`.
/// Returns `None` if the URL is not a recognised webhook URL or the ID cannot
/// be parsed as a `u64`.
///
/// The extracted ID is equal to the `author.id` field on `MESSAGE_CREATE`
/// events originating from that webhook, making it safe to store in the
/// self-message filter set.
#[must_use]
pub fn webhook_id_from_url(url: &str) -> Option<u64> {
    let path = url
        .strip_prefix("https://discord.com/api/webhooks/")
        .or_else(|| url.strip_prefix("https://canary.discord.com/api/webhooks/"))
        .or_else(|| url.strip_prefix("https://ptb.discord.com/api/webhooks/"))?;
    let id_str = path.split('/').next()?;
    id_str.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // --- DiscordPresence::away_message ---

    // --- is_non_offline ---

    #[test]
    fn offline_is_not_non_offline() {
        assert!(!DiscordPresence::Offline.is_non_offline());
    }

    #[test]
    fn online_is_non_offline() {
        assert!(DiscordPresence::Online.is_non_offline());
    }

    #[test]
    fn idle_is_non_offline() {
        assert!(DiscordPresence::Idle.is_non_offline());
    }

    #[test]
    fn dnd_is_non_offline() {
        assert!(DiscordPresence::DoNotDisturb.is_non_offline());
    }

    // --- away_message ---

    #[test]
    fn online_is_not_away() {
        assert_eq!(DiscordPresence::Online.away_message(), None);
    }

    #[test]
    fn idle_away_message() {
        assert_eq!(DiscordPresence::Idle.away_message(), Some("idle"));
    }

    #[test]
    fn dnd_away_message() {
        assert_eq!(
            DiscordPresence::DoNotDisturb.away_message(),
            Some("do not disturb")
        );
    }

    #[test]
    fn offline_away_message() {
        assert_eq!(DiscordPresence::Offline.away_message(), Some("offline"));
    }

    // --- webhook_id_from_url ---

    #[test]
    fn webhook_id_standard_url() {
        assert_eq!(
            webhook_id_from_url("https://discord.com/api/webhooks/123456789012345678/sometoken"),
            Some(123_456_789_012_345_678_u64)
        );
    }

    #[test]
    fn webhook_id_canary_url() {
        assert_eq!(
            webhook_id_from_url("https://canary.discord.com/api/webhooks/987654321098765432/tok"),
            Some(987_654_321_098_765_432_u64)
        );
    }

    #[test]
    fn webhook_id_ptb_url() {
        assert_eq!(
            webhook_id_from_url("https://ptb.discord.com/api/webhooks/111222333444555666/tok"),
            Some(111_222_333_444_555_666_u64)
        );
    }

    #[test]
    fn webhook_id_wrong_host_returns_none() {
        assert_eq!(
            webhook_id_from_url("https://example.com/api/webhooks/123/token"),
            None
        );
    }

    #[test]
    fn webhook_id_non_numeric_id_returns_none() {
        assert_eq!(
            webhook_id_from_url("https://discord.com/api/webhooks/notanumber/token"),
            None
        );
    }

    #[test]
    fn webhook_id_url_without_token_segment() {
        // URL with only the id and no trailing /token is still valid
        assert_eq!(
            webhook_id_from_url("https://discord.com/api/webhooks/123456789012345678"),
            Some(123_456_789_012_345_678_u64)
        );
    }

    proptest! {
        #[test]
        fn webhook_id_roundtrips(id in 0u64..=u64::MAX) {
            let url = format!("https://discord.com/api/webhooks/{id}/sometoken");
            prop_assert_eq!(webhook_id_from_url(&url), Some(id));
        }
    }
}
