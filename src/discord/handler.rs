use std::collections::HashSet;
use std::sync::Arc;

use serenity::async_trait;
use serenity::client::{Context, EventHandler};
use serenity::model::channel::Message;
use serenity::model::gateway::{Presence, Ready};
use serenity::model::guild::{Guild, Member};
use serenity::model::id::GuildId;
use serenity::model::user::{OnlineStatus, User};
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, info};

use crate::discord::types::{DiscordEvent, DiscordPresence, MemberInfo};

/// Serenity event handler for the Discord Gateway.
///
/// State shared across handler calls is wrapped in `Arc` so the handler can
/// be cheaply cloned if the client needs to be rebuilt.
#[derive(Clone)]
pub(crate) struct DiscordHandler {
    /// Channel to the processing task.
    pub(crate) event_tx: mpsc::Sender<DiscordEvent>,
    /// IDs to suppress on `MESSAGE_CREATE` (bot user ID + webhook user IDs).
    pub(crate) self_filter: Arc<RwLock<HashSet<u64>>>,
    /// Discord channel IDs that have an active bridge entry.
    /// Wrapped in `RwLock` so config reload can add/remove channels at runtime.
    pub(crate) bridged_channel_ids: Arc<RwLock<HashSet<u64>>>,
}

/// Resolve the display name for a guild member.
///
/// Priority: guild nickname → global display name → username.
pub(crate) fn resolve_display_name<'a>(
    nick: Option<&'a str>,
    global_name: Option<&'a str>,
    username: &'a str,
) -> &'a str {
    nick.or(global_name).unwrap_or(username)
}

/// Map a serenity [`OnlineStatus`] to a [`DiscordPresence`].
///
/// `OnlineStatus` is `#[non_exhaustive]`; any unrecognised variant maps to
/// [`DiscordPresence::Offline`].
pub(crate) fn map_online_status(status: OnlineStatus) -> DiscordPresence {
    match status {
        OnlineStatus::Online => DiscordPresence::Online,
        OnlineStatus::Idle => DiscordPresence::Idle,
        OnlineStatus::DoNotDisturb => DiscordPresence::DoNotDisturb,
        OnlineStatus::Offline | OnlineStatus::Invisible => DiscordPresence::Offline,
        _ => DiscordPresence::Offline,
    }
}

/// Decide whether a `MESSAGE_CREATE` event should be relayed to IRC.
///
/// Returns `true` iff the message is in a bridged channel **and** the author
/// is not in the self-message filter set (bot user ID or owned webhook ID).
pub(crate) fn should_relay_message(
    channel_id: u64,
    author_id: u64,
    bridged_channel_ids: &HashSet<u64>,
    self_filter: &HashSet<u64>,
) -> bool {
    bridged_channel_ids.contains(&channel_id) && !self_filter.contains(&author_id)
}

/// Build a [`MemberInfo`] from a presence snapshot, defaulting to offline when
/// the user has no presence entry (common for large guilds before chunking).
fn member_presence(
    presences: &std::collections::HashMap<serenity::model::id::UserId, Presence>,
    user_id: serenity::model::id::UserId,
) -> DiscordPresence {
    presences
        .get(&user_id)
        .map_or(DiscordPresence::Offline, |p| map_online_status(p.status))
}

#[async_trait]
impl EventHandler for DiscordHandler {
    async fn ready(&self, _ctx: Context, ready: Ready) {
        let bot_id = ready.user.id.get();
        self.self_filter.write().await.insert(bot_id);
        info!(
            bot_id,
            tag = %ready.user.tag(),
            "Discord bot ready"
        );
    }

    async fn guild_create(&self, _ctx: Context, guild: Guild, _is_new: Option<bool>) {
        let guild_id = guild.id.get();

        let members: Vec<MemberInfo> = guild
            .members
            .values()
            .map(|m| {
                let user_id = m.user.id.get();
                let display_name = resolve_display_name(
                    m.nick.as_deref(),
                    m.user.global_name.as_deref(),
                    &m.user.name,
                )
                .to_owned();
                let presence = member_presence(&guild.presences, m.user.id);
                MemberInfo {
                    user_id,
                    display_name,
                    presence,
                }
            })
            .collect();

        let _ = self
            .event_tx
            .send(DiscordEvent::MemberSnapshot { guild_id, members })
            .await;
    }

    async fn message(&self, _ctx: Context, msg: Message) {
        let channel_id = msg.channel_id.get();
        let author_id = msg.author.id.get();

        let channels = self.bridged_channel_ids.read().await;
        let filter = self.self_filter.read().await;
        if !should_relay_message(channel_id, author_id, &channels, &filter) {
            debug!(
                channel_id,
                author_id, "dropping non-bridged or self message"
            );
            return;
        }
        drop(filter);
        drop(channels);

        let attachments = msg.attachments.iter().map(|a| a.url.clone()).collect();

        let _ = self
            .event_tx
            .send(DiscordEvent::MessageReceived {
                channel_id,
                author_id,
                author_name: msg.author.name.clone(),
                content: msg.content.clone(),
                attachments,
            })
            .await;
    }

    async fn presence_update(&self, _ctx: Context, new_data: Presence) {
        let Some(guild_id) = new_data.guild_id else {
            return; // ignore DM presences
        };
        let _ = self
            .event_tx
            .send(DiscordEvent::PresenceUpdated {
                user_id: new_data.user.id.get(),
                guild_id: guild_id.get(),
                presence: map_online_status(new_data.status),
            })
            .await;
    }

    async fn guild_member_addition(&self, _ctx: Context, new_member: Member) {
        let display_name = resolve_display_name(
            new_member.nick.as_deref(),
            new_member.user.global_name.as_deref(),
            &new_member.user.name,
        )
        .to_owned();
        let _ = self
            .event_tx
            .send(DiscordEvent::MemberAdded {
                user_id: new_member.user.id.get(),
                guild_id: new_member.guild_id.get(),
                display_name,
            })
            .await;
    }

    async fn guild_member_removal(
        &self,
        _ctx: Context,
        guild_id: GuildId,
        user: User,
        _member_data: Option<Member>,
    ) {
        let _ = self
            .event_tx
            .send(DiscordEvent::MemberRemoved {
                user_id: user.id.get(),
                guild_id: guild_id.get(),
            })
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // --- should_relay_message ---

    fn ids(vals: &[u64]) -> HashSet<u64> {
        vals.iter().copied().collect()
    }

    #[test]
    fn relayed_when_bridged_and_not_self() {
        assert!(should_relay_message(10, 99, &ids(&[10]), &ids(&[])));
    }

    #[test]
    fn not_relayed_when_channel_not_bridged() {
        assert!(!should_relay_message(99, 1, &ids(&[10]), &ids(&[])));
    }

    #[test]
    fn not_relayed_when_author_is_self() {
        assert!(!should_relay_message(10, 1, &ids(&[10]), &ids(&[1])));
    }

    #[test]
    fn not_relayed_when_neither_bridged_nor_self_passes() {
        assert!(!should_relay_message(99, 1, &ids(&[10]), &ids(&[1])));
    }

    proptest! {
        /// For any combination of channel/author IDs and set membership, the
        /// relay decision equals the logical conjunction.
        #[test]
        fn relay_matches_logical_conjunction(
            channel_id in 1u64..100,
            author_id in 1u64..100,
            bridged in proptest::bool::ANY,
            is_self in proptest::bool::ANY,
        ) {
            let bridged_ids: HashSet<u64> = if bridged { ids(&[channel_id]) } else { ids(&[]) };
            let self_ids: HashSet<u64> = if is_self { ids(&[author_id]) } else { ids(&[]) };
            let expected = bridged && !is_self;
            prop_assert_eq!(
                should_relay_message(channel_id, author_id, &bridged_ids, &self_ids),
                expected
            );
        }
    }

    // --- resolve_display_name ---

    #[test]
    fn nick_takes_priority_over_all() {
        assert_eq!(
            resolve_display_name(Some("Nick"), Some("GlobalName"), "username"),
            "Nick"
        );
    }

    #[test]
    fn global_name_used_when_no_nick() {
        assert_eq!(
            resolve_display_name(None, Some("GlobalName"), "username"),
            "GlobalName"
        );
    }

    #[test]
    fn username_used_when_no_nick_or_global_name() {
        assert_eq!(resolve_display_name(None, None, "username"), "username");
    }

    #[test]
    fn empty_nick_still_preferred_over_global_name() {
        // Discord does not allow empty nicks, but guard against it defensively.
        assert_eq!(resolve_display_name(Some(""), Some("GlobalName"), "u"), "");
    }

    // --- map_online_status ---

    #[test]
    fn online_maps_to_online() {
        assert_eq!(
            map_online_status(OnlineStatus::Online),
            DiscordPresence::Online
        );
    }

    #[test]
    fn idle_maps_to_idle() {
        assert_eq!(map_online_status(OnlineStatus::Idle), DiscordPresence::Idle);
    }

    #[test]
    fn dnd_maps_to_dnd() {
        assert_eq!(
            map_online_status(OnlineStatus::DoNotDisturb),
            DiscordPresence::DoNotDisturb
        );
    }

    #[test]
    fn offline_maps_to_offline() {
        assert_eq!(
            map_online_status(OnlineStatus::Offline),
            DiscordPresence::Offline
        );
    }

    #[test]
    fn invisible_maps_to_offline() {
        assert_eq!(
            map_online_status(OnlineStatus::Invisible),
            DiscordPresence::Offline
        );
    }
}
