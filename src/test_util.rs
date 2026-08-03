//! Shared test helpers for the whole crate.
//!
//! The module is declared `#[cfg(test)]` in `lib.rs`, so it never reaches a
//! release build.

use std::collections::HashMap;

use crate::discord::{DiscordEvent, DiscordPresence, MemberInfo};

// ---------------------------------------------------------------------------
// Null resolvers
// ---------------------------------------------------------------------------

pub(crate) struct NullResolver;

impl crate::formatting::DiscordResolver for NullResolver {
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

pub(crate) struct NullIrcResolver;

impl crate::formatting::IrcMentionResolver for NullIrcResolver {
    fn resolve_nick(&self, _: &str) -> Option<String> {
        None
    }
}

// ---------------------------------------------------------------------------
// Member snapshot builders
// ---------------------------------------------------------------------------

/// Guild ID that the snapshot builders use.
pub(crate) const TEST_GUILD: u64 = 999;

/// Channel ID that the snapshot builders report as bridged.
pub(crate) const TEST_CHANNEL: u64 = 111;

/// Build one [`MemberInfo`].
pub(crate) fn member(
    user_id: u64,
    username: &str,
    display_name: &str,
    presence: DiscordPresence,
) -> MemberInfo {
    MemberInfo {
        user_id,
        username: username.to_owned(),
        display_name: display_name.to_owned(),
        presence,
    }
}

/// Build a [`DiscordEvent::MemberSnapshot`] for the common test shape.
///
/// Guild [`TEST_GUILD`] owns channel [`TEST_CHANNEL`]. There are no channel or
/// role names, and the bot user ID is 0. Use [`snapshot_with`] when a test
/// needs to change any of those.
pub(crate) fn snapshot(members: Vec<MemberInfo>) -> DiscordEvent {
    snapshot_with(SnapshotOpts::default(), members)
}

/// Fields of a member snapshot that some tests must change.
pub(crate) struct SnapshotOpts {
    pub(crate) guild_id: u64,
    pub(crate) channel_ids: Vec<u64>,
    pub(crate) channel_names: HashMap<u64, String>,
    pub(crate) role_names: HashMap<u64, String>,
    pub(crate) bot_user_id: u64,
}

impl Default for SnapshotOpts {
    fn default() -> Self {
        Self {
            guild_id: TEST_GUILD,
            channel_ids: vec![TEST_CHANNEL],
            channel_names: HashMap::new(),
            role_names: HashMap::new(),
            bot_user_id: 0,
        }
    }
}

/// Build a [`DiscordEvent::MemberSnapshot`] with explicit options.
pub(crate) fn snapshot_with(opts: SnapshotOpts, members: Vec<MemberInfo>) -> DiscordEvent {
    DiscordEvent::MemberSnapshot {
        guild_id: opts.guild_id,
        members,
        channel_ids: opts.channel_ids,
        channel_names: opts.channel_names,
        role_names: opts.role_names,
        bot_user_id: opts.bot_user_id,
    }
}
