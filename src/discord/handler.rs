use std::collections::HashSet;
use std::sync::Arc;

use serenity::async_trait;
use serenity::client::{Context, EventHandler};
use serenity::model::gateway::{Presence, Ready};
use serenity::model::guild::Guild;
use serenity::model::user::OnlineStatus;
use tokio::sync::{RwLock, mpsc};
use tracing::info;

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
    /// Discord channel IDs that have an active bridge entry (used in task 4).
    #[allow(dead_code)]
    pub(crate) bridged_channel_ids: Arc<HashSet<u64>>,
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
