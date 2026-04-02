use std::collections::HashMap;
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

// ---------------------------------------------------------------------------
// Pure / testable helper functions
// ---------------------------------------------------------------------------

/// Resolve the display name for a guild member.
///
/// Priority: guild nickname → global display name → username.
pub(crate) fn resolve_display_name<'a>(
    nick: Option<&'a str>,
    global_name: Option<&'a str>,
    username: &'a str,
) -> &'a str {
    nick.filter(|s| !s.is_empty())
        .or(global_name.filter(|s| !s.is_empty()))
        .unwrap_or(username)
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
        // The explicit arm documents intent; the `_` catch-all below handles
        // any future #[non_exhaustive] variants identically (equivalent mutant).
        OnlineStatus::Offline | OnlineStatus::Invisible | _ => DiscordPresence::Offline,
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

/// Build a [`DiscordEvent::PresenceUpdated`] if the presence has an associated
/// guild ID (DM presences without a guild ID are ignored — returns `None`).
pub(crate) fn presence_event(
    user_id: u64,
    guild_id: Option<u64>,
    status: OnlineStatus,
) -> Option<DiscordEvent> {
    guild_id.map(|gid| DiscordEvent::PresenceUpdated {
        user_id,
        guild_id: gid,
        presence: map_online_status(status),
    })
}

/// Build a [`DiscordEvent::MemberAdded`] for a new guild member.
pub(crate) fn member_addition_event(
    user_id: u64,
    guild_id: u64,
    nick: Option<&str>,
    global_name: Option<&str>,
    username: &str,
) -> DiscordEvent {
    DiscordEvent::MemberAdded {
        user_id,
        guild_id,
        display_name: resolve_display_name(nick, global_name, username).to_owned(),
    }
}

/// Build a [`DiscordEvent::MemberRemoved`] for a departing guild member.
pub(crate) fn member_removal_event(user_id: u64, guild_id: u64) -> DiscordEvent {
    DiscordEvent::MemberRemoved { user_id, guild_id }
}

/// Intermediate representation of a guild member used by
/// [`build_member_snapshot_event`] so it can be tested without serenity types.
pub(crate) struct RawMemberData<'a> {
    pub user_id: u64,
    pub nick: Option<&'a str>,
    pub global_name: Option<&'a str>,
    pub username: &'a str,
    /// `true` when the Discord account is a bot.  Bots lack presence data but
    /// can send and receive channel messages, so they are treated as always
    /// online for pseudoclient purposes.
    pub bot: bool,
}

/// Build a [`DiscordEvent::MemberSnapshot`] from raw member data.
///
/// `presences` maps user IDs to their current [`DiscordPresence`].  Members
/// absent from the map are treated as offline (common during large-guild
/// chunking and on the REST fallback path).
///
/// Bot accounts are always treated as online — they lack Gateway presence data
/// but can send and receive messages in channels, so they should have
/// pseudoclients on IRC.
pub(crate) fn build_member_snapshot_event(
    guild_id: u64,
    members: &[RawMemberData<'_>],
    presences: &HashMap<u64, DiscordPresence>,
    channel_ids: Vec<u64>,
    channel_names: HashMap<u64, String>,
    role_names: HashMap<u64, String>,
) -> DiscordEvent {
    // Only include non-offline members in the burst.  Offline members are
    // excluded to keep the initial IRC channel population small; they will be
    // introduced lazily when they come online (PRESENCE_UPDATE) or first speak
    // (MESSAGE_CREATE).  Bots have no presence data but are always included.
    let member_infos: Vec<MemberInfo> = members
        .iter()
        .filter_map(|m| {
            let presence = if m.bot {
                // Bots never appear in the presences map; treat them as Online.
                DiscordPresence::Online
            } else {
                presences
                    .get(&m.user_id)
                    .copied()
                    .unwrap_or(DiscordPresence::Offline)
            };
            if !presence.is_non_offline() {
                return None;
            }
            Some(MemberInfo {
                user_id: m.user_id,
                display_name: resolve_display_name(m.nick, m.global_name, m.username).to_owned(),
                presence,
            })
        })
        .collect();
    DiscordEvent::MemberSnapshot {
        guild_id,
        members: member_infos,
        channel_ids,
        channel_names,
        role_names,
    }
}

// ---------------------------------------------------------------------------
// DiscordHandler methods — testable inner logic called by the shims below
// ---------------------------------------------------------------------------

impl DiscordHandler {
    /// Record the bot user ID in the self-message filter and log readiness.
    pub(crate) async fn handle_ready(&self, bot_id: u64, tag: &str) {
        self.self_filter.write().await.insert(bot_id);
        info!(bot_id, tag, "Discord bot ready");
    }

    /// Relay a DM `MESSAGE_CREATE` event to the processing task if it passes
    /// self-message filtering.
    pub(crate) async fn handle_dm_event(
        &self,
        author_id: u64,
        author_name: String,
        content: String,
        referenced_content: Option<String>,
    ) {
        let filter = self.self_filter.read().await;
        if filter.contains(&author_id) {
            return;
        }
        drop(filter);
        let _ = self
            .event_tx
            .send(DiscordEvent::DmReceived {
                author_id,
                author_name,
                content,
                referenced_content,
            })
            .await;
    }

    /// Relay a `MESSAGE_CREATE` event to the processing task if it passes
    /// channel routing and self-message filtering.
    pub(crate) async fn handle_message_event(
        &self,
        channel_id: u64,
        author_id: u64,
        author_name: String,
        content: String,
        attachments: Vec<String>,
    ) {
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
        let _ = self
            .event_tx
            .send(DiscordEvent::MessageReceived {
                channel_id,
                author_id,
                author_name,
                content,
                attachments,
            })
            .await;
    }
}

// ---------------------------------------------------------------------------
// Serenity EventHandler shims — thin wrappers; integration-tested only
// ---------------------------------------------------------------------------

#[async_trait]
#[mutants::skip] // Serenity EventHandler shims — require live Discord Gateway to exercise
impl EventHandler for DiscordHandler {
    async fn ready(&self, _ctx: Context, ready: Ready) {
        self.handle_ready(ready.user.id.get(), &ready.user.tag())
            .await;
    }

    async fn guild_create(&self, _ctx: Context, guild: Guild, _is_new: Option<bool>) {
        let presences: HashMap<u64, DiscordPresence> = guild
            .presences
            .iter()
            .map(|(uid, p)| (uid.get(), map_online_status(p.status)))
            .collect();

        let raw: Vec<RawMemberData<'_>> = guild
            .members
            .values()
            .map(|m| RawMemberData {
                user_id: m.user.id.get(),
                nick: m.nick.as_deref(),
                global_name: m.user.global_name.as_deref(),
                username: &m.user.name,
                bot: m.user.bot,
            })
            .collect();

        // Determine which bridged channel IDs belong to this guild.
        let guild_channel_ids: Vec<u64> = {
            let bridged = self.bridged_channel_ids.read().await;
            guild
                .channels
                .keys()
                .filter(|cid| bridged.contains(&cid.get()))
                .map(|cid| cid.get())
                .collect()
        };

        // Extract channel and role names for mention resolution.
        let channel_names: HashMap<u64, String> = guild
            .channels
            .iter()
            .map(|(cid, ch)| (cid.get(), ch.name.clone()))
            .collect();
        let role_names: HashMap<u64, String> = guild
            .roles
            .iter()
            .map(|(rid, role)| (rid.get(), role.name.clone()))
            .collect();

        tracing::debug!(
            guild_id = guild.id.get(),
            total_members = raw.len(),
            total_presences = presences.len(),
            bridged_channels = guild_channel_ids.len(),
            guild_channels = guild.channels.len(),
            guild_roles = role_names.len(),
            "guild_create received"
        );

        let event = build_member_snapshot_event(
            guild.id.get(),
            &raw,
            &presences,
            guild_channel_ids,
            channel_names,
            role_names,
        );

        if let DiscordEvent::MemberSnapshot { ref members, .. } = event {
            tracing::debug!(
                guild_id = guild.id.get(),
                online_members = members.len(),
                "emitting MemberSnapshot"
            );
        }

        let _ = self.event_tx.send(event).await;
    }

    async fn message(&self, ctx: Context, msg: Message) {
        if msg.guild_id.is_none() {
            // DM — resolve referenced message content if this is a reply.
            let referenced_content = if let Some(ref msg_ref) = msg.message_reference {
                if let Some(ref_id) = msg_ref.message_id {
                    msg.channel_id
                        .message(&ctx.http, ref_id)
                        .await
                        .ok()
                        .map(|m| m.content)
                } else {
                    None
                }
            } else {
                None
            };
            self.handle_dm_event(
                msg.author.id.get(),
                msg.author.name.clone(),
                msg.content.clone(),
                referenced_content,
            )
            .await;
        } else {
            self.handle_message_event(
                msg.channel_id.get(),
                msg.author.id.get(),
                msg.author.name.clone(),
                msg.content.clone(),
                msg.attachments.iter().map(|a| a.url.clone()).collect(),
            )
            .await;
        }
    }

    async fn presence_update(&self, _ctx: Context, new_data: Presence) {
        if let Some(event) = presence_event(
            new_data.user.id.get(),
            new_data.guild_id.map(GuildId::get),
            new_data.status,
        ) {
            let _ = self.event_tx.send(event).await;
        }
    }

    async fn guild_member_addition(&self, _ctx: Context, new_member: Member) {
        let event = member_addition_event(
            new_member.user.id.get(),
            new_member.guild_id.get(),
            new_member.nick.as_deref(),
            new_member.user.global_name.as_deref(),
            &new_member.user.name,
        );
        let _ = self.event_tx.send(event).await;
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
            .send(member_removal_event(user.id.get(), guild_id.get()))
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use tokio::sync::mpsc;

    // ---------------------------------------------------------------------------
    // Test helper
    // ---------------------------------------------------------------------------

    fn make_handler(
        tx: mpsc::Sender<DiscordEvent>,
        channel_ids: &[u64],
        self_filter_ids: &[u64],
    ) -> DiscordHandler {
        DiscordHandler {
            event_tx: tx,
            self_filter: Arc::new(RwLock::new(self_filter_ids.iter().copied().collect())),
            bridged_channel_ids: Arc::new(RwLock::new(channel_ids.iter().copied().collect())),
        }
    }

    fn ids(vals: &[u64]) -> HashSet<u64> {
        vals.iter().copied().collect()
    }

    // ---------------------------------------------------------------------------
    // handle_ready
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn handle_ready_inserts_bot_id_into_filter() {
        let (tx, _rx) = mpsc::channel(1);
        let h = make_handler(tx, &[], &[]);
        h.handle_ready(42, "TestBot#0001").await;
        assert!(h.self_filter.read().await.contains(&42));
    }

    #[tokio::test]
    async fn handle_ready_does_not_affect_existing_filter_entries() {
        let (tx, _rx) = mpsc::channel(1);
        let h = make_handler(tx, &[], &[99]); // 99 is a pre-existing webhook ID
        h.handle_ready(42, "Bot").await;
        let f = h.self_filter.read().await;
        assert!(f.contains(&42));
        assert!(f.contains(&99));
    }

    // ---------------------------------------------------------------------------
    // handle_message_event
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn relayed_message_emits_event() {
        let (tx, mut rx) = mpsc::channel(1);
        let h = make_handler(tx, &[10], &[]); // channel 10 bridged, empty self-filter
        h.handle_message_event(10, 99, "alice".into(), "hello".into(), vec![])
            .await;
        let event = rx.try_recv().expect("expected MessageReceived event");
        assert!(matches!(
            event,
            DiscordEvent::MessageReceived {
                channel_id: 10,
                author_id: 99,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn self_message_is_dropped() {
        let (tx, mut rx) = mpsc::channel(1);
        let h = make_handler(tx, &[10], &[99]); // author 99 is in self-filter
        h.handle_message_event(10, 99, "bot".into(), "echo".into(), vec![])
            .await;
        assert!(
            rx.try_recv().is_err(),
            "self-message must not emit an event"
        );
    }

    #[tokio::test]
    async fn non_bridged_channel_is_dropped() {
        let (tx, mut rx) = mpsc::channel(1);
        let h = make_handler(tx, &[10], &[]); // only channel 10 bridged
        h.handle_message_event(99, 1, "user".into(), "hi".into(), vec![])
            .await;
        assert!(
            rx.try_recv().is_err(),
            "non-bridged channel must not emit an event"
        );
    }

    // ---------------------------------------------------------------------------
    // --- handle_dm_event ---

    #[tokio::test]
    async fn dm_event_emits_dm_received() {
        let (tx, mut rx) = mpsc::channel(1);
        let h = make_handler(tx, &[], &[]);
        h.handle_dm_event(42, "alice".into(), "hello".into(), None)
            .await;
        let event = rx.try_recv().expect("expected DmReceived event");
        assert!(matches!(
            event,
            DiscordEvent::DmReceived { author_id: 42, .. }
        ));
    }

    #[tokio::test]
    async fn dm_from_self_is_dropped() {
        let (tx, mut rx) = mpsc::channel(1);
        let h = make_handler(tx, &[], &[42]); // 42 is in self-filter
        h.handle_dm_event(42, "bot".into(), "echo".into(), None)
            .await;
        assert!(rx.try_recv().is_err(), "DM from self should be dropped");
    }

    #[tokio::test]
    async fn dm_event_includes_referenced_content() {
        let (tx, mut rx) = mpsc::channel(1);
        let h = make_handler(tx, &[], &[]);
        h.handle_dm_event(
            42,
            "alice".into(),
            "reply text".into(),
            Some("**[bob]** original".into()),
        )
        .await;
        let event = rx.try_recv().expect("expected DmReceived event");
        if let DiscordEvent::DmReceived {
            referenced_content, ..
        } = event
        {
            assert_eq!(referenced_content.as_deref(), Some("**[bob]** original"));
        } else {
            panic!("expected DmReceived");
        }
    }

    // should_relay_message
    // ---------------------------------------------------------------------------

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

    // ---------------------------------------------------------------------------
    // presence_event
    // ---------------------------------------------------------------------------

    #[test]
    fn presence_event_with_guild_id_emits_event() {
        let ev = presence_event(42, Some(100), OnlineStatus::Idle);
        assert_eq!(
            ev,
            Some(DiscordEvent::PresenceUpdated {
                user_id: 42,
                guild_id: 100,
                presence: DiscordPresence::Idle,
            })
        );
    }

    #[test]
    fn presence_event_without_guild_id_returns_none() {
        assert_eq!(presence_event(42, None, OnlineStatus::Online), None);
    }

    // ---------------------------------------------------------------------------
    // member_addition_event
    // ---------------------------------------------------------------------------

    #[test]
    fn member_addition_resolves_display_name_and_builds_event() {
        let ev = member_addition_event(7, 100, Some("NickName"), Some("Global"), "user");
        assert_eq!(
            ev,
            DiscordEvent::MemberAdded {
                user_id: 7,
                guild_id: 100,
                display_name: "NickName".to_string(),
            }
        );
    }

    // ---------------------------------------------------------------------------
    // ---------------------------------------------------------------------------
    // build_member_snapshot_event
    // ---------------------------------------------------------------------------

    #[test]
    fn snapshot_excludes_offline_members_includes_online() {
        let members = vec![
            RawMemberData {
                user_id: 1,
                nick: None,
                global_name: None,
                username: "alice",
                bot: false,
            },
            RawMemberData {
                user_id: 2,
                nick: None,
                global_name: None,
                username: "bob",
                bot: false,
            },
        ];
        let mut presences = HashMap::new();
        presences.insert(1u64, DiscordPresence::Online);
        // user 2 absent from presences → Offline → must be excluded

        let ev = build_member_snapshot_event(
            99,
            &members,
            &presences,
            vec![],
            HashMap::new(),
            HashMap::new(),
        );
        let DiscordEvent::MemberSnapshot {
            guild_id,
            members: infos,
            ..
        } = ev
        else {
            panic!("expected MemberSnapshot");
        };
        assert_eq!(guild_id, 99);
        assert_eq!(infos.len(), 1, "offline member must be excluded");
        assert_eq!(infos[0].user_id, 1);
        assert_eq!(infos[0].presence, DiscordPresence::Online);
    }

    #[test]
    fn snapshot_with_all_offline_is_empty() {
        let members = vec![RawMemberData {
            user_id: 5,
            nick: Some("N"),
            global_name: None,
            username: "u",
            bot: false,
        }];
        let ev = build_member_snapshot_event(
            10,
            &members,
            &HashMap::new(),
            vec![],
            HashMap::new(),
            HashMap::new(),
        );
        let DiscordEvent::MemberSnapshot { members: infos, .. } = ev else {
            panic!()
        };
        assert!(
            infos.is_empty(),
            "all-offline snapshot must produce no members"
        );
    }

    #[test]
    fn snapshot_non_offline_statuses_all_included() {
        // idle and dnd members must be included (only offline is excluded)
        let members = vec![
            RawMemberData {
                user_id: 10,
                nick: None,
                global_name: None,
                username: "idler",
                bot: false,
            },
            RawMemberData {
                user_id: 11,
                nick: None,
                global_name: None,
                username: "busy",
                bot: false,
            },
        ];
        let mut presences = HashMap::new();
        presences.insert(10u64, DiscordPresence::Idle);
        presences.insert(11u64, DiscordPresence::DoNotDisturb);

        let ev = build_member_snapshot_event(
            1,
            &members,
            &presences,
            vec![],
            HashMap::new(),
            HashMap::new(),
        );
        let DiscordEvent::MemberSnapshot { members: infos, .. } = ev else {
            panic!()
        };
        assert_eq!(infos.len(), 2, "idle and dnd members must be included");
    }

    #[test]
    fn snapshot_bot_included_even_without_presence() {
        // Bot accounts lack Gateway presence data; they must be treated as Online
        // so they get pseudoclients and the bridge can be in the IRC channel.
        let members = vec![
            RawMemberData {
                user_id: 20,
                nick: None,
                global_name: None,
                username: "bridgebot",
                bot: true,
            },
            RawMemberData {
                user_id: 21,
                nick: None,
                global_name: None,
                username: "offlineuser",
                bot: false,
            },
        ];
        // No presences for either member.
        let ev = build_member_snapshot_event(
            50,
            &members,
            &HashMap::new(),
            vec![],
            HashMap::new(),
            HashMap::new(),
        );
        let DiscordEvent::MemberSnapshot { members: infos, .. } = ev else {
            panic!()
        };
        assert_eq!(
            infos.len(),
            1,
            "bot must be included; offline human must not"
        );
        assert_eq!(infos[0].user_id, 20);
        assert_eq!(infos[0].presence, DiscordPresence::Online);
    }

    // ---------------------------------------------------------------------------
    // resolve_display_name / map_online_status (unchanged from before)
    // ---------------------------------------------------------------------------

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
    fn empty_nick_falls_through_to_global_name() {
        assert_eq!(
            resolve_display_name(Some(""), Some("GlobalName"), "u"),
            "GlobalName"
        );
    }

    #[test]
    fn empty_nick_and_global_name_falls_through_to_username() {
        assert_eq!(resolve_display_name(Some(""), Some(""), "user"), "user");
    }

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
