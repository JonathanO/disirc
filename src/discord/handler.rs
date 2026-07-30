use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use serenity::async_trait;
use serenity::client::{Context, EventHandler};
use serenity::http::Http;
use serenity::model::channel::Message;
use serenity::model::gateway::{Presence, Ready};
use serenity::model::guild::{Guild, Member};
use serenity::model::id::{ChannelId, GuildId, MessageId};
use serenity::model::user::{OnlineStatus, User};
use tokio::sync::{RwLock, mpsc};
use tracing::{info, trace};

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
    username: Option<String>,
    display_name: Option<String>,
) -> Option<DiscordEvent> {
    guild_id.map(|gid| DiscordEvent::PresenceUpdated {
        user_id,
        guild_id: gid,
        presence: map_online_status(status),
        username,
        display_name,
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
    pub(crate) user_id: u64,
    pub(crate) nick: Option<&'a str>,
    pub(crate) global_name: Option<&'a str>,
    pub(crate) username: &'a str,
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
    bot_user_id: u64,
) -> DiscordEvent {
    // Include all members so their names are cached for later introduction
    // when they come online via PRESENCE_UPDATE.  Only non-offline members
    // will actually be introduced as pseudoclients during the burst.
    let member_infos: Vec<MemberInfo> = members
        .iter()
        .map(|m| {
            let presence = presences
                .get(&m.user_id)
                .copied()
                .unwrap_or(DiscordPresence::Offline);
            MemberInfo {
                user_id: m.user_id,
                username: m.username.to_owned(),
                display_name: resolve_display_name(m.nick, m.global_name, m.username).to_owned(),
                presence,
            }
        })
        .collect();
    DiscordEvent::MemberSnapshot {
        guild_id,
        members: member_infos,
        channel_ids,
        channel_names,
        role_names,
        bot_user_id,
    }
}

/// Build the [`DiscordEvent::MemberSnapshot`] for a `GUILD_CREATE` payload.
///
/// Extracted from the `guild_create` `EventHandler` shim so that it is
/// reachable from unit tests and from mutation testing.  The shim itself
/// cannot be exercised without a live Gateway — constructing a serenity
/// [`Context`] requires a `ShardMessenger`, whose fields are `pub(crate)` to
/// serenity and whose only public constructor takes a `&ShardRunner`, which in
/// turn needs a `Shard` built by `Shard::new` over a real WebSocket.  Keeping
/// this marshalling in the shim would hide it from mutation testing entirely,
/// since cargo-mutants only emits a coarse "replace with `()`" for the shim.
///
/// `bridged` is the set of Discord channel IDs with an active bridge entry;
/// only the guild's own channels that appear in it are reported.
pub(crate) fn guild_create_event(
    guild: &Guild,
    bridged: &HashSet<u64>,
    bot_user_id: u64,
) -> DiscordEvent {
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
        })
        .collect();

    // Determine which bridged channel IDs belong to this guild.
    let guild_channel_ids: Vec<u64> = guild
        .channels
        .keys()
        .filter(|cid| bridged.contains(&cid.get()))
        .map(|cid| cid.get())
        .collect();

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
        bot_user_id,
    );

    if let DiscordEvent::MemberSnapshot { ref members, .. } = event {
        tracing::debug!(
            guild_id = guild.id.get(),
            online_members = members.len(),
            "emitting MemberSnapshot"
        );
    }

    event
}

/// Build the [`DiscordEvent::PresenceUpdated`] for a `PRESENCE_UPDATE` payload.
///
/// Extracted from the `presence_update` shim for the same reason as
/// [`guild_create_event`]: the shim itself is unreachable from unit tests, and
/// cargo-mutants only emits a coarse "replace with `()`" for it.
///
/// Returns `None` for presences without a guild ID (DM-only presences), which
/// the bridge has no channel to relay to.
pub(crate) fn presence_update_event(new_data: &Presence) -> Option<DiscordEvent> {
    // Extract display name from the presence payload's partial user/member.
    let nick = new_data
        .user
        .member
        .as_ref()
        .and_then(|m| m.nick.as_deref());
    let global_name = new_data.user.global_name.as_deref();
    let username = new_data.user.name.as_deref();
    let display_name = username.map(|u| resolve_display_name(nick, global_name, u).to_owned());

    tracing::debug!(
        user_id = new_data.user.id.get(),
        guild_id = ?new_data.guild_id.map(GuildId::get),
        status = ?new_data.status,
        ?display_name,
        "presence_update received"
    );

    presence_event(
        new_data.user.id.get(),
        new_data.guild_id.map(GuildId::get),
        new_data.status,
        username.map(str::to_owned),
        display_name,
    )
}

/// How an incoming `MESSAGE_CREATE` should be handled.
///
/// Produced by [`classify_message`] so that the routing decision and the field
/// marshalling behind it are testable independently of the `message` shim,
/// which cannot be called without a live Gateway `Context`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum IncomingMessage {
    /// A direct message. `referenced_message_id` is set when the DM is a reply,
    /// and the referenced content must be fetched over HTTP before relaying.
    Dm {
        author_id: u64,
        author_name: String,
        content: String,
        referenced_message_id: Option<MessageId>,
        timestamp: chrono::DateTime<chrono::Utc>,
    },
    /// A message in a guild channel.
    Guild {
        channel_id: u64,
        author_id: u64,
        author_name: String,
        display_name: String,
        content: String,
        attachments: Vec<String>,
        timestamp: chrono::DateTime<chrono::Utc>,
    },
}

/// Decide how to handle an incoming `MESSAGE_CREATE` and marshal its fields.
///
/// A message with no `guild_id` is a DM.  Note that this classifies only; the
/// self-filter and bridged-channel checks happen later, in `handle_dm_event`
/// and `handle_message_event`.
pub(crate) fn classify_message(msg: &Message) -> IncomingMessage {
    if msg.guild_id.is_none() {
        IncomingMessage::Dm {
            author_id: msg.author.id.get(),
            author_name: msg.author.name.clone(),
            content: msg.content.clone(),
            referenced_message_id: msg
                .message_reference
                .as_ref()
                .and_then(|msg_ref| msg_ref.message_id),
            timestamp: *msg.timestamp,
        }
    } else {
        let member_nick = msg.member.as_ref().and_then(|m| m.nick.as_deref());
        IncomingMessage::Guild {
            channel_id: msg.channel_id.get(),
            author_id: msg.author.id.get(),
            author_name: msg.author.name.clone(),
            display_name: resolve_display_name(
                member_nick,
                msg.author.global_name.as_deref(),
                &msg.author.name,
            )
            .to_owned(),
            content: msg.content.clone(),
            attachments: msg.attachments.iter().map(|a| a.url.clone()).collect(),
            timestamp: *msg.timestamp,
        }
    }
}

/// Fetch the content of a DM that is being replied to, for quote context.
///
/// A failure here is not fatal — the reply is still worth relaying without the
/// quoted text — so errors are logged and flattened to `None`.
pub(crate) async fn fetch_referenced_content(
    http: &Http,
    channel_id: ChannelId,
    ref_id: MessageId,
) -> Option<String> {
    match channel_id.message(http, ref_id).await {
        Ok(m) => Some(m.content),
        Err(e) => {
            tracing::warn!(
                error = %e,
                ref_id = ref_id.get(),
                channel_id = channel_id.get(),
                "Failed to fetch referenced DM message; relaying without quote context"
            );
            None
        }
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
        timestamp: chrono::DateTime<chrono::Utc>,
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
                timestamp,
            })
            .await;
    }

    /// Relay a `MESSAGE_CREATE` event to the processing task if it passes
    /// channel routing and self-message filtering.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn handle_message_event(
        &self,
        channel_id: u64,
        author_id: u64,
        author_name: String,
        author_display_name: String,
        content: String,
        attachments: Vec<String>,
        timestamp: chrono::DateTime<chrono::Utc>,
    ) {
        let channels = self.bridged_channel_ids.read().await;
        let filter = self.self_filter.read().await;
        if !should_relay_message(channel_id, author_id, &channels, &filter) {
            trace!(
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
                author_display_name,
                content,
                attachments,
                timestamp,
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

    async fn guild_create(&self, ctx: Context, guild: Guild, _is_new: Option<bool>) {
        let bot_user_id = ctx.cache.current_user().id.get();
        // Scope the read guard so it is released before the send await.
        let event = {
            let bridged = self.bridged_channel_ids.read().await;
            guild_create_event(&guild, &bridged, bot_user_id)
        };
        let _ = self.event_tx.send(event).await;
    }

    async fn message(&self, ctx: Context, msg: Message) {
        match classify_message(&msg) {
            IncomingMessage::Dm {
                author_id,
                author_name,
                content,
                referenced_message_id,
                timestamp,
            } => {
                let referenced_content = match referenced_message_id {
                    Some(ref_id) => {
                        fetch_referenced_content(&ctx.http, msg.channel_id, ref_id).await
                    }
                    None => None,
                };
                self.handle_dm_event(
                    author_id,
                    author_name,
                    content,
                    referenced_content,
                    timestamp,
                )
                .await;
            }
            IncomingMessage::Guild {
                channel_id,
                author_id,
                author_name,
                display_name,
                content,
                attachments,
                timestamp,
            } => {
                self.handle_message_event(
                    channel_id,
                    author_id,
                    author_name,
                    display_name,
                    content,
                    attachments,
                    timestamp,
                )
                .await;
            }
        }
    }

    async fn presence_update(&self, _ctx: Context, new_data: Presence) {
        if let Some(event) = presence_update_event(&new_data) {
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
    use tokio::sync::mpsc;

    // ---------------------------------------------------------------------------
    // guild_create_event
    // ---------------------------------------------------------------------------

    /// A `GUILD_CREATE` payload: guild 1, channels 10 (`general`) and 11
    /// (`other`), role 20 (`mods`), members 5 (`alice`, nicked "Ali", online)
    /// and 6 (`bob`, no nick, no presence entry → offline).
    const GUILD_CREATE_JSON: &str = r#"{
        "id": "1", "name": "guild", "icon": null, "icon_hash": null,
        "splash": null, "discovery_splash": null, "owner_id": "2",
        "verification_level": 0, "default_message_notifications": 0,
        "explicit_content_filter": 0, "emojis": [],
        "features": [], "mfa_level": 0, "application_id": null,
        "system_channel_id": null, "system_channel_flags": 0,
        "rules_channel_id": null, "max_presences": null, "max_members": null,
        "vanity_url_code": null, "description": null, "banner": null,
        "premium_tier": 0, "premium_subscription_count": 0,
        "preferred_locale": "en-US", "public_updates_channel_id": null,
        "max_video_channel_users": null, "max_stage_video_channel_users": null,
        "nsfw_level": 0, "stickers": [], "premium_progress_bar_enabled": false,
        "joined_at": "2025-01-01T00:00:00.000Z", "large": false,
        "unavailable": false, "member_count": 2, "voice_states": [],
        "threads": [], "stage_instances": [], "guild_scheduled_events": [],
        "roles": [
            {"id":"20","name":"mods","color":0,
             "colors":{"primary_color":0,"secondary_color":null,"tertiary_color":null},
             "hoist":false,"position":1,
             "permissions":"0","managed":false,"mentionable":true}
        ],
        "channels": [
            {"id":"10","type":0,"name":"general","guild_id":"1","position":0,"permission_overwrites":[]},
            {"id":"11","type":0,"name":"other","guild_id":"1","position":1,"permission_overwrites":[]}
        ],
        "members": [
            {"user":{"id":"5","username":"alice","discriminator":"0000","global_name":null,"avatar":null},
             "nick":"Ali","roles":[],"joined_at":"2025-01-01T00:00:00.000Z","deaf":false,"mute":false,"flags":0},
            {"user":{"id":"6","username":"bob","discriminator":"0000","global_name":"Bobby","avatar":null},
             "nick":null,"roles":[],"joined_at":"2025-01-01T00:00:00.000Z","deaf":false,"mute":false,"flags":0}
        ],
        "presences": [
            {"user":{"id":"5"},"status":"online","activities":[],"client_status":{}}
        ]
    }"#;

    fn fixture_guild() -> Guild {
        let event: serenity::model::event::GuildCreateEvent =
            serde_json::from_str(GUILD_CREATE_JSON).expect("guild fixture should deserialize");
        event.guild
    }

    #[test]
    fn guild_create_event_reports_only_bridged_channels() {
        let guild = fixture_guild();
        let bridged: HashSet<u64> = [10, 999].into_iter().collect();

        let DiscordEvent::MemberSnapshot { channel_ids, .. } =
            guild_create_event(&guild, &bridged, 99)
        else {
            panic!("expected a MemberSnapshot");
        };

        // 11 belongs to the guild but is not bridged; 999 is bridged but
        // belongs to a different guild.  Neither may appear.
        assert_eq!(channel_ids, vec![10]);
    }

    #[test]
    fn guild_create_event_with_no_bridged_channels_reports_none() {
        let guild = fixture_guild();

        let DiscordEvent::MemberSnapshot {
            members,
            channel_ids,
            ..
        } = guild_create_event(&guild, &HashSet::new(), 99)
        else {
            panic!("expected a MemberSnapshot");
        };

        assert!(channel_ids.is_empty());
        // Members are still reported so their names are cached for later
        // introduction via PRESENCE_UPDATE.
        assert_eq!(members.len(), 2);
    }

    #[test]
    fn guild_create_event_resolves_names_and_presence() {
        let guild = fixture_guild();
        let bridged: HashSet<u64> = [10].into_iter().collect();

        let DiscordEvent::MemberSnapshot {
            guild_id,
            members,
            bot_user_id,
            ..
        } = guild_create_event(&guild, &bridged, 99)
        else {
            panic!("expected a MemberSnapshot");
        };

        assert_eq!(guild_id, 1);
        assert_eq!(bot_user_id, 99);

        let alice = members
            .iter()
            .find(|m| m.user_id == 5)
            .expect("alice must be present");
        assert_eq!(alice.username, "alice");
        assert_eq!(alice.display_name, "Ali", "guild nick wins");
        assert_eq!(alice.presence, DiscordPresence::Online);

        let bob = members
            .iter()
            .find(|m| m.user_id == 6)
            .expect("bob must be present even though offline");
        assert_eq!(
            bob.display_name, "Bobby",
            "global_name wins when there is no nick"
        );
        assert_eq!(
            bob.presence,
            DiscordPresence::Offline,
            "a member with no presence entry defaults to offline"
        );
    }

    #[test]
    fn guild_create_event_extracts_all_channel_and_role_names() {
        let guild = fixture_guild();
        let bridged: HashSet<u64> = [10].into_iter().collect();

        let DiscordEvent::MemberSnapshot {
            channel_names,
            role_names,
            ..
        } = guild_create_event(&guild, &bridged, 99)
        else {
            panic!("expected a MemberSnapshot");
        };

        // Name maps cover the whole guild, not just bridged channels — they
        // back mention resolution, which can reference any channel or role.
        assert_eq!(channel_names.get(&10).map(String::as_str), Some("general"));
        assert_eq!(channel_names.get(&11).map(String::as_str), Some("other"));
        assert_eq!(role_names.get(&20).map(String::as_str), Some("mods"));
    }

    // ---------------------------------------------------------------------------
    // presence_update_event
    // ---------------------------------------------------------------------------

    /// A `PRESENCE_UPDATE` payload. `nick` and `global_name` are injected so
    /// display-name precedence can be exercised; `guild_id` may be omitted.
    fn presence_json(
        guild_id: Option<&str>,
        username: Option<&str>,
        global_name: Option<&str>,
        nick: Option<&str>,
    ) -> serde_json::Value {
        let mut user = serde_json::json!({ "id": "5" });
        if let Some(u) = username {
            user["username"] = serde_json::json!(u);
            user["discriminator"] = serde_json::json!("0000");
        }
        if let Some(g) = global_name {
            user["global_name"] = serde_json::json!(g);
        }
        if let Some(n) = nick {
            user["member"] = serde_json::json!({
                "nick": n,
                "roles": [],
                "joined_at": "2025-01-01T00:00:00.000Z",
                "deaf": false, "mute": false, "flags": 0
            });
        }
        let mut payload = serde_json::json!({
            "user": user,
            "status": "online",
            "activities": [],
            "client_status": {}
        });
        if let Some(g) = guild_id {
            payload["guild_id"] = serde_json::json!(g);
        }
        payload
    }

    fn parse_presence(value: &serde_json::Value) -> Presence {
        serde_json::from_value(value.clone()).expect("presence fixture should deserialize")
    }

    #[test]
    fn presence_update_event_prefers_nick_then_global_name() {
        let with_nick = parse_presence(&presence_json(
            Some("100"),
            Some("alice"),
            Some("Global"),
            Some("Nick"),
        ));
        let Some(DiscordEvent::PresenceUpdated { display_name, .. }) =
            presence_update_event(&with_nick)
        else {
            panic!("expected a PresenceUpdated");
        };
        assert_eq!(display_name.as_deref(), Some("Nick"));

        let no_nick = parse_presence(&presence_json(
            Some("100"),
            Some("alice"),
            Some("Global"),
            None,
        ));
        let Some(DiscordEvent::PresenceUpdated { display_name, .. }) =
            presence_update_event(&no_nick)
        else {
            panic!("expected a PresenceUpdated");
        };
        assert_eq!(display_name.as_deref(), Some("Global"));

        let bare = parse_presence(&presence_json(Some("100"), Some("alice"), None, None));
        let Some(DiscordEvent::PresenceUpdated { display_name, .. }) = presence_update_event(&bare)
        else {
            panic!("expected a PresenceUpdated");
        };
        assert_eq!(display_name.as_deref(), Some("alice"));
    }

    #[test]
    fn presence_update_event_without_guild_id_is_dropped() {
        let presence = parse_presence(&presence_json(None, Some("alice"), None, None));
        assert!(
            presence_update_event(&presence).is_none(),
            "a presence with no guild has no bridged channel to relay to"
        );
    }

    #[test]
    fn presence_update_event_without_username_has_no_display_name() {
        // PRESENCE_UPDATE may carry only a partial user (just the ID).
        let presence = parse_presence(&presence_json(Some("100"), None, None, None));
        let Some(DiscordEvent::PresenceUpdated {
            user_id,
            username,
            display_name,
            ..
        }) = presence_update_event(&presence)
        else {
            panic!("expected a PresenceUpdated");
        };
        assert_eq!(user_id, 5);
        assert_eq!(username, None);
        assert_eq!(
            display_name, None,
            "no username means no name to resolve against"
        );
    }

    // ---------------------------------------------------------------------------
    // classify_message
    // ---------------------------------------------------------------------------

    /// A `MESSAGE_CREATE` payload. `guild_id` absent means a DM.
    fn message_json(
        guild_id: Option<&str>,
        referenced_id: Option<&str>,
        nick: Option<&str>,
        global_name: Option<&str>,
        attachments: &[&str],
    ) -> serde_json::Value {
        let attachments: Vec<serde_json::Value> = attachments
            .iter()
            .enumerate()
            .map(|(i, url)| {
                serde_json::json!({
                    "id": (i + 1).to_string(), "filename": "f.png",
                    "size": 1, "url": url, "proxy_url": url
                })
            })
            .collect();
        let mut author = serde_json::json!({
            "id": "5", "username": "alice", "discriminator": "0000", "avatar": null
        });
        if let Some(g) = global_name {
            author["global_name"] = serde_json::json!(g);
        }
        let mut payload = serde_json::json!({
            "id": "1", "channel_id": "10", "author": author,
            "content": "hello", "timestamp": "2025-01-01T00:00:00.000Z",
            "edited_timestamp": null, "tts": false, "mention_everyone": false,
            "mentions": [], "mention_roles": [], "attachments": attachments,
            "embeds": [], "pinned": false, "type": 0
        });
        if let Some(g) = guild_id {
            payload["guild_id"] = serde_json::json!(g);
        }
        if let Some(r) = referenced_id {
            payload["message_reference"] =
                serde_json::json!({ "message_id": r, "channel_id": "10" });
        }
        if let Some(n) = nick {
            payload["member"] = serde_json::json!({
                "nick": n, "roles": [], "joined_at": "2025-01-01T00:00:00.000Z",
                "deaf": false, "mute": false, "flags": 0
            });
        }
        payload
    }

    fn parse_message(value: &serde_json::Value) -> Message {
        serde_json::from_value(value.clone()).expect("message fixture should deserialize")
    }

    #[test]
    fn classify_message_without_guild_id_is_a_dm() {
        let msg = parse_message(&message_json(None, None, None, None, &[]));

        let IncomingMessage::Dm {
            author_id,
            author_name,
            content,
            referenced_message_id,
            ..
        } = classify_message(&msg)
        else {
            panic!("expected a Dm");
        };

        assert_eq!(author_id, 5);
        assert_eq!(author_name, "alice");
        assert_eq!(content, "hello");
        assert_eq!(referenced_message_id, None, "not a reply");
    }

    #[test]
    fn classify_message_dm_reply_carries_referenced_id() {
        let msg = parse_message(&message_json(None, Some("77"), None, None, &[]));

        let IncomingMessage::Dm {
            referenced_message_id,
            ..
        } = classify_message(&msg)
        else {
            panic!("expected a Dm");
        };

        assert_eq!(referenced_message_id, Some(MessageId::new(77)));
    }

    #[test]
    fn classify_message_with_guild_id_is_a_guild_message() {
        let msg = parse_message(&message_json(
            Some("100"),
            None,
            Some("Ali"),
            Some("Global"),
            &["https://cdn.example/a.png", "https://cdn.example/b.png"],
        ));

        let IncomingMessage::Guild {
            channel_id,
            author_id,
            author_name,
            display_name,
            content,
            attachments,
            ..
        } = classify_message(&msg)
        else {
            panic!("expected a Guild message");
        };

        assert_eq!(channel_id, 10);
        assert_eq!(author_id, 5);
        assert_eq!(author_name, "alice");
        assert_eq!(display_name, "Ali", "member nick wins");
        assert_eq!(content, "hello");
        assert_eq!(
            attachments,
            vec![
                "https://cdn.example/a.png".to_string(),
                "https://cdn.example/b.png".to_string()
            ],
            "attachment URLs are collected in order"
        );
    }

    #[test]
    fn classify_message_guild_falls_back_through_display_names() {
        let with_global =
            parse_message(&message_json(Some("100"), None, None, Some("Global"), &[]));
        let IncomingMessage::Guild { display_name, .. } = classify_message(&with_global) else {
            panic!("expected a Guild message");
        };
        assert_eq!(display_name, "Global", "global_name wins when no nick");

        let bare = parse_message(&message_json(Some("100"), None, None, None, &[]));
        let IncomingMessage::Guild { display_name, .. } = classify_message(&bare) else {
            panic!("expected a Guild message");
        };
        assert_eq!(display_name, "alice", "username is the last resort");
    }

    #[test]
    fn classify_message_ignores_message_reference_on_guild_messages() {
        // A guild reply carries a message_reference too, but the guild path has
        // no quote-fetch step, so it must not be routed as a DM.
        let msg = parse_message(&message_json(Some("100"), Some("77"), None, None, &[]));
        assert!(matches!(
            classify_message(&msg),
            IncomingMessage::Guild { .. }
        ));
    }

    // ---------------------------------------------------------------------------
    // fetch_referenced_content
    // ---------------------------------------------------------------------------

    mod fetch_referenced {
        use super::*;
        use wiremock::matchers::{method, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        fn mock_http(server: &MockServer) -> Http {
            serenity::http::HttpBuilder::new("test-token")
                .proxy(server.uri())
                .ratelimiter_disabled(true)
                .build()
        }

        #[tokio::test]
        async fn returns_content_on_success() {
            let server = MockServer::start().await;
            let http = mock_http(&server);

            Mock::given(method("GET"))
                .and(path_regex(r"channels/\d+/messages/\d+"))
                .respond_with(ResponseTemplate::new(200).set_body_json(message_json(
                    None,
                    None,
                    None,
                    None,
                    &[],
                )))
                .mount(&server)
                .await;

            let content =
                fetch_referenced_content(&http, ChannelId::new(10), MessageId::new(77)).await;

            assert_eq!(content.as_deref(), Some("hello"));
        }

        #[tokio::test]
        async fn returns_none_on_http_error() {
            let server = MockServer::start().await;
            let http = mock_http(&server);

            Mock::given(method("GET"))
                .and(path_regex(r"channels/\d+/messages/\d+"))
                .respond_with(ResponseTemplate::new(403))
                .mount(&server)
                .await;

            // A missing or forbidden quote must not stop the reply relaying.
            let content =
                fetch_referenced_content(&http, ChannelId::new(10), MessageId::new(77)).await;

            assert_eq!(content, None);
        }
    }

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

    /// Fixed test timestamp; value is arbitrary — tests that care about
    /// propagation assert on exact equality with this constant.
    fn stock_ts() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::<chrono::Utc>::from_timestamp(1_700_000_000, 0).unwrap()
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
        h.handle_message_event(
            10,
            99,
            "alice".into(),
            "Alice".into(),
            "hello".into(),
            vec![],
            stock_ts(),
        )
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
        h.handle_message_event(
            10,
            99,
            "bot".into(),
            "Bot".into(),
            "echo".into(),
            vec![],
            stock_ts(),
        )
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
        h.handle_message_event(
            99,
            1,
            "user".into(),
            "User".into(),
            "hi".into(),
            vec![],
            stock_ts(),
        )
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
        h.handle_dm_event(42, "alice".into(), "hello".into(), None, stock_ts())
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
        h.handle_dm_event(42, "bot".into(), "echo".into(), None, stock_ts())
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
            stock_ts(),
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

    // ---------------------------------------------------------------------------
    // presence_event
    // ---------------------------------------------------------------------------

    #[test]
    fn presence_event_with_guild_id_emits_event() {
        let ev = presence_event(
            42,
            Some(100),
            OnlineStatus::Idle,
            Some("alice".into()),
            Some("Alice".into()),
        );
        assert_eq!(
            ev,
            Some(DiscordEvent::PresenceUpdated {
                user_id: 42,
                guild_id: 100,
                presence: DiscordPresence::Idle,
                username: Some("alice".into()),
                display_name: Some("Alice".into()),
            })
        );
    }

    #[test]
    fn presence_event_without_guild_id_returns_none() {
        assert_eq!(
            presence_event(
                42,
                None,
                OnlineStatus::Online,
                Some("alice".into()),
                Some("Alice".into())
            ),
            None
        );
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
    fn snapshot_includes_all_members_with_correct_presence() {
        let members = vec![
            RawMemberData {
                user_id: 1,
                nick: None,
                global_name: None,
                username: "alice",
            },
            RawMemberData {
                user_id: 2,
                nick: None,
                global_name: None,
                username: "bob",
            },
        ];
        let mut presences = HashMap::new();
        presences.insert(1u64, DiscordPresence::Online);
        // user 2 absent from presences → Offline

        let ev = build_member_snapshot_event(
            99,
            &members,
            &presences,
            vec![],
            HashMap::new(),
            HashMap::new(),
            0,
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
        assert_eq!(infos.len(), 2, "all members must be included");
        assert_eq!(infos[0].user_id, 1);
        assert_eq!(infos[0].presence, DiscordPresence::Online);
        assert_eq!(infos[1].user_id, 2);
        assert_eq!(infos[1].presence, DiscordPresence::Offline);
    }

    #[test]
    fn snapshot_offline_members_have_offline_presence() {
        let members = vec![RawMemberData {
            user_id: 5,
            nick: Some("N"),
            global_name: None,
            username: "u",
        }];
        let ev = build_member_snapshot_event(
            10,
            &members,
            &HashMap::new(),
            vec![],
            HashMap::new(),
            HashMap::new(),
            0,
        );
        let DiscordEvent::MemberSnapshot { members: infos, .. } = ev else {
            panic!()
        };
        assert_eq!(infos.len(), 1, "offline members must be included");
        assert_eq!(infos[0].presence, DiscordPresence::Offline);
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
            },
            RawMemberData {
                user_id: 11,
                nick: None,
                global_name: None,
                username: "busy",
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
            0,
        );
        let DiscordEvent::MemberSnapshot { members: infos, .. } = ev else {
            panic!()
        };
        assert_eq!(infos.len(), 2, "idle and dnd members must be included");
    }

    #[test]
    fn snapshot_members_without_presence_are_offline() {
        // Members absent from the presences map default to Offline,
        // regardless of whether they are bots or humans.  They will be
        // introduced when their PRESENCE_UPDATE arrives.
        let members = vec![
            RawMemberData {
                user_id: 20,
                nick: None,
                global_name: None,
                username: "bridgebot",
            },
            RawMemberData {
                user_id: 21,
                nick: None,
                global_name: None,
                username: "offlineuser",
            },
        ];
        let ev = build_member_snapshot_event(
            50,
            &members,
            &HashMap::new(),
            vec![],
            HashMap::new(),
            HashMap::new(),
            0,
        );
        let DiscordEvent::MemberSnapshot { members: infos, .. } = ev else {
            panic!()
        };
        assert_eq!(infos.len(), 2, "all members must be included");
        assert_eq!(infos[0].presence, DiscordPresence::Offline);
        assert_eq!(infos[1].presence, DiscordPresence::Offline);
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
