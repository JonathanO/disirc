use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serenity::builder::{CreateAllowedMentions, CreateMessage, ExecuteWebhook};
use serenity::cache::Cache;
use serenity::http::Http;
use serenity::model::id::{ChannelId, UserId};
use serenity::model::webhook::Webhook;
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, warn};

use crate::discord::handler::{map_online_status, resolve_display_name};
use crate::discord::types::{DiscordCommand, DiscordEvent, DiscordPresence, MemberInfo};

/// Enforce the Discord webhook username constraint of 2–32 Unicode scalar values.
///
/// - Characters beyond position 32 are dropped.
/// - If the result is shorter than 2 characters it is padded with `_`.
pub(crate) fn sanitize_webhook_username(nick: &str) -> String {
    let mut result: String = nick.chars().take(32).collect();
    while result.chars().count() < 2 {
        result.push('_');
    }
    result
}

/// Insert U+200B (zero-width space) after `@` in `@everyone` and `@here`
/// (case-insensitive) to prevent Discord pings on the plain-send fallback path.
///
/// The webhook path suppresses pings via `allowed_mentions` instead; this
/// function is only needed for the `channel.send()` fallback.
pub(crate) fn suppress_mentions(text: &str) -> String {
    let mut result = String::with_capacity(text.len() + 4);
    let mut remaining = text;
    while let Some(at_pos) = remaining.find('@') {
        // Push up to and including the '@'
        result.push_str(&remaining[..=at_pos]);
        let after = &remaining[at_pos + 1..];
        let after_lower = after.to_ascii_lowercase();
        if after_lower.starts_with("everyone") || after_lower.starts_with("here") {
            result.push('\u{200B}');
        }
        remaining = after;
    }
    result.push_str(remaining);
    result
}

/// Send a message to a Discord channel on behalf of an IRC user.
///
/// Uses the webhook if a `webhook_url` is provided; falls back to plain
/// `channel.send()` otherwise.  If webhook *resolution* fails (e.g. the
/// webhook was deleted or its token rotated), falls back to plain send so
/// the message still reaches Discord.  Execute failures after a successful
/// resolve are dropped — we don't know whether the message was actually
/// posted, so retrying risks duplicates.
pub(crate) async fn send_discord_message(
    http: &Http,
    channel_id: u64,
    webhook_url: Option<&str>,
    sender_nick: &str,
    text: &str,
) {
    if let Some(url) = webhook_url {
        let username = sanitize_webhook_username(sender_nick);
        match Webhook::from_url(http, url).await {
            Ok(wh) => {
                let execute = ExecuteWebhook::new()
                    .username(username)
                    .content(text)
                    // parse: [] — no @everyone or @here pings (mandatory safety rule)
                    .allowed_mentions(CreateAllowedMentions::new());
                if let Err(e) = wh.execute(http, false, execute).await {
                    warn!(error = %e, channel_id, "Webhook execute failed; dropping message");
                }
            }
            Err(e) => {
                // Log the webhook ID (not the URL — it contains the token).
                let webhook_id = crate::discord::webhook_id_from_url(url);
                warn!(
                    error = %e,
                    channel_id,
                    webhook_id = ?webhook_id,
                    "Webhook resolve failed; falling back to plain send"
                );
                let fallback_text = format!(
                    "**[{}]** {text}",
                    crate::formatting::ping_fix_nick(sender_nick)
                );
                send_plain(http, channel_id, &fallback_text).await;
            }
        }
    } else {
        // Plain send: the text already contains the "**[nick]** content" prefix
        // (formatted by relay.rs with ping-fixed nick).  Only suppress @everyone
        // / @here mentions — do NOT re-wrap with the nick.
        send_plain(http, channel_id, text).await;
    }
}

/// Plain (non-webhook) channel send.  Suppresses `@everyone` / `@here`
/// and logs failures.
async fn send_plain(http: &Http, channel_id: u64, text: &str) {
    let safe_text = suppress_mentions(text);
    let msg = CreateMessage::new().content(safe_text);
    if let Err(e) = ChannelId::new(channel_id).send_message(http, msg).await {
        warn!(error = %e, channel_id, "Channel send failed; dropping message");
    }
}

/// Send a DM to a Discord user.
///
/// Opens (or reuses) a DM channel with the recipient, then sends the message.
/// The `text` should already be formatted (e.g. `**[nick]** content`).
pub(crate) async fn send_dm(http: &Http, recipient_user_id: u64, text: &str) {
    let dm_channel = match UserId::new(recipient_user_id).create_dm_channel(http).await {
        Ok(ch) => ch,
        Err(e) => {
            warn!(
                error = %e,
                recipient_user_id,
                "Failed to open DM channel; dropping message"
            );
            return;
        }
    };
    let safe_text = suppress_mentions(text);
    let msg = CreateMessage::new().content(safe_text);
    if let Err(e) = dm_channel.id.send_message(http, msg).await {
        warn!(
            error = %e,
            recipient_user_id,
            "DM send failed; dropping message"
        );
    }
}

/// Apply a `ReloadBridges` command to the live routing tables.
///
/// - Adds/removes channel IDs from `channel_ids`.
/// - Adds/removes webhook IDs from `self_filter`.
pub(crate) fn apply_reload(
    channel_ids: &mut HashSet<u64>,
    self_filter: &mut HashSet<u64>,
    added_channel_ids: &[u64],
    removed_channel_ids: &[u64],
    added_webhook_ids: &[u64],
    removed_webhook_ids: &[u64],
) {
    for &id in added_channel_ids {
        channel_ids.insert(id);
    }
    for &id in removed_channel_ids {
        channel_ids.remove(&id);
    }
    for &id in added_webhook_ids {
        self_filter.insert(id);
    }
    for &id in removed_webhook_ids {
        self_filter.remove(&id);
    }
}

/// Raw member fields extracted from serenity `Member` values, in a shape
/// that isolates the pure filtering/mapping logic from serenity types.
pub(crate) struct RawMember {
    pub(crate) user_id: u64,
    pub(crate) username: String,
    pub(crate) nick: Option<String>,
    pub(crate) global_name: Option<String>,
}

/// Build the non-offline `Vec<MemberInfo>` from raw member data and a
/// presence map.  Members absent from the presence map are treated as
/// offline (and thus excluded).
///
/// Pure — no serenity types, no I/O; drives the interesting logic of
/// [`snapshot_from_cache`].
pub(crate) fn non_offline_member_infos(
    members: &[RawMember],
    presences: &HashMap<u64, DiscordPresence>,
) -> Vec<MemberInfo> {
    members
        .iter()
        .filter_map(|m| {
            let presence = presences
                .get(&m.user_id)
                .copied()
                .unwrap_or(DiscordPresence::Offline);
            if !presence.is_non_offline() {
                return None;
            }
            Some(MemberInfo {
                user_id: m.user_id,
                username: m.username.clone(),
                display_name: resolve_display_name(
                    m.nick.as_deref(),
                    m.global_name.as_deref(),
                    &m.username,
                )
                .to_owned(),
                presence,
            })
        })
        .collect()
}

/// Filter an iterator of channel IDs against the set of bridged channels.
///
/// Pure — separated so the filtering predicate can be mutation-tested.
pub(crate) fn filter_bridged_channels(
    channel_ids: impl Iterator<Item = u64>,
    bridged: &HashSet<u64>,
) -> Vec<u64> {
    channel_ids.filter(|cid| bridged.contains(cid)).collect()
}

/// Build a [`DiscordEvent::MemberSnapshot`] for `channel_id` from the serenity
/// cache.
///
/// Looks up the channel's owning guild in the cache, then reads the guild's
/// `members` and `presences` maps — both already populated by `GUILD_CREATE`
/// and `GUILD_MEMBERS_CHUNK` / `PRESENCE_UPDATE` events.  No REST call is made.
///
/// Returns `None` if the channel or its guild is not present in the cache
/// (should not happen in normal operation after startup).
pub(crate) fn snapshot_from_cache(
    cache: &Cache,
    channel_id: u64,
    all_bridged_channel_ids: &std::collections::HashSet<u64>,
) -> Option<DiscordEvent> {
    // Find the owning guild by checking each guild's channel map.
    // disirc connects to a small number of guilds so this iteration is cheap.
    let target = ChannelId::new(channel_id);
    let guild_id = cache.guilds().into_iter().find(|&gid| {
        cache
            .guild(gid)
            .is_some_and(|g| g.channels.contains_key(&target))
    })?;

    let guild = cache.guild(guild_id)?;

    let presences: HashMap<u64, DiscordPresence> = guild
        .presences
        .iter()
        .map(|(uid, p)| (uid.get(), map_online_status(p.status)))
        .collect();

    let raw_members: Vec<RawMember> = guild
        .members
        .values()
        .map(|m| RawMember {
            user_id: m.user.id.get(),
            username: m.user.name.clone(),
            nick: m.nick.clone(),
            global_name: m.user.global_name.clone(),
        })
        .collect();

    let members = non_offline_member_infos(&raw_members, &presences);
    let channel_ids = filter_bridged_channels(
        guild.channels.keys().map(|cid| cid.get()),
        all_bridged_channel_ids,
    );

    debug!(
        guild_id = guild_id.get(),
        count = members.len(),
        "built member snapshot from cache for new bridge channel"
    );

    Some(DiscordEvent::MemberSnapshot {
        guild_id: guild_id.get(),
        members,
        channel_ids,
        // ReloadBridges path: channel/role names are not available from the
        // cache lookup.  This is acceptable because the initial guild_create
        // already populated them; this snapshot only adds new members.
        channel_names: std::collections::HashMap::new(),
        role_names: std::collections::HashMap::new(),
        bot_user_id: cache.current_user().id.get(),
    })
}

/// Drain [`DiscordCommand`]s from the bridging layer and dispatch them.
///
/// `event_tx` is used to emit [`DiscordEvent::MemberSnapshot`] events when a
/// new bridge channel is added via [`DiscordCommand::ReloadBridges`].
///
/// Runs until the sender side of `rx` is dropped.
pub(crate) async fn process_discord_commands(
    http: Arc<Http>,
    cache: Arc<Cache>,
    mut rx: mpsc::Receiver<DiscordCommand>,
    event_tx: mpsc::Sender<DiscordEvent>,
    self_filter: Arc<RwLock<HashSet<u64>>>,
    channel_ids: Arc<RwLock<HashSet<u64>>>,
) {
    while let Some(cmd) = rx.recv().await {
        match cmd {
            DiscordCommand::SendMessage {
                channel_id,
                webhook_url,
                sender_nick,
                text,
            } => {
                send_discord_message(
                    &http,
                    channel_id,
                    webhook_url.as_deref(),
                    &sender_nick,
                    &text,
                )
                .await;
            }
            DiscordCommand::ReloadBridges {
                added_channel_ids,
                removed_channel_ids,
                added_webhook_ids,
                removed_webhook_ids,
            } => {
                // Update routing tables under lock.
                {
                    let mut cids = channel_ids.write().await;
                    let mut sf = self_filter.write().await;
                    apply_reload(
                        &mut cids,
                        &mut sf,
                        &added_channel_ids,
                        &removed_channel_ids,
                        &added_webhook_ids,
                        &removed_webhook_ids,
                    );
                }
                // Emit member snapshots for each newly added channel from cache.
                let all_channel_ids: std::collections::HashSet<u64> =
                    { channel_ids.read().await.clone() };
                for channel_id in added_channel_ids {
                    match snapshot_from_cache(&cache, channel_id, &all_channel_ids) {
                        Some(event) => {
                            let _ = event_tx.send(event).await;
                        }
                        None => {
                            warn!(
                                channel_id,
                                "channel or guild not found in cache; \
                                 skipping member snapshot for new bridge"
                            );
                        }
                    }
                }
            }
            DiscordCommand::SendDm {
                recipient_user_id,
                text,
            }
            | DiscordCommand::SendBotDm {
                recipient_user_id,
                text,
            } => {
                send_dm(&http, recipient_user_id, &text).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // --- apply_reload ---

    fn hset(ids: &[u64]) -> HashSet<u64> {
        ids.iter().copied().collect()
    }

    #[test]
    fn added_channel_ids_inserted() {
        let mut cids = hset(&[]);
        let mut sf = hset(&[]);
        apply_reload(&mut cids, &mut sf, &[10, 20], &[], &[], &[]);
        assert!(cids.contains(&10));
        assert!(cids.contains(&20));
    }

    #[test]
    fn removed_channel_ids_deleted() {
        let mut cids = hset(&[10, 20, 30]);
        let mut sf = hset(&[]);
        apply_reload(&mut cids, &mut sf, &[], &[20], &[], &[]);
        assert!(!cids.contains(&20));
        assert!(cids.contains(&10));
        assert!(cids.contains(&30));
    }

    #[test]
    fn added_webhook_ids_inserted_into_filter() {
        let mut cids = hset(&[]);
        let mut sf = hset(&[]);
        apply_reload(&mut cids, &mut sf, &[], &[], &[999], &[]);
        assert!(sf.contains(&999));
    }

    #[test]
    fn removed_webhook_ids_deleted_from_filter() {
        let mut cids = hset(&[]);
        let mut sf = hset(&[111, 222]);
        apply_reload(&mut cids, &mut sf, &[], &[], &[], &[111]);
        assert!(!sf.contains(&111));
        assert!(sf.contains(&222));
    }

    #[test]
    fn removing_nonexistent_id_is_noop() {
        let mut cids = hset(&[10]);
        let mut sf = hset(&[]);
        // Neither 99 (channel) nor 888 (webhook) exist — must not panic
        apply_reload(&mut cids, &mut sf, &[], &[99], &[], &[888]);
        assert!(cids.contains(&10));
    }

    // --- sanitize_webhook_username ---

    #[test]
    fn empty_nick_padded_to_two_underscores() {
        assert_eq!(sanitize_webhook_username(""), "__");
    }

    #[test]
    fn single_char_nick_padded_to_two() {
        assert_eq!(sanitize_webhook_username("x"), "x_");
    }

    #[test]
    fn two_char_nick_unchanged() {
        assert_eq!(sanitize_webhook_username("ab"), "ab");
    }

    #[test]
    fn thirty_two_char_nick_unchanged() {
        let nick = "a".repeat(32);
        assert_eq!(sanitize_webhook_username(&nick), nick);
    }

    #[test]
    fn thirty_three_char_nick_truncated_to_thirty_two() {
        let nick = "a".repeat(33);
        assert_eq!(sanitize_webhook_username(&nick).chars().count(), 32);
    }

    #[test]
    fn multibyte_unicode_truncated_by_char_count_not_bytes() {
        // "é" is 2 bytes; 32 of them is 64 bytes but only 32 chars — must be kept intact
        let nick: String = "é".repeat(33);
        let out = sanitize_webhook_username(&nick);
        assert_eq!(out.chars().count(), 32);
        // Must be valid UTF-8 (Rust guarantees this, but assert the length)
        assert_eq!(out, "é".repeat(32));
    }

    proptest! {
        #[test]
        /// Output length is the input length clamped to [2, 32], the input's
        /// leading characters survive, and any padding is underscores.
        ///
        /// The previous version asserted only the length bounds, which a
        /// constant `"__"` also satisfies.
        fn sanitize_clamps_length_and_keeps_prefix(nick in ".*") {
            let out = sanitize_webhook_username(&nick);
            let in_len = nick.chars().count();

            prop_assert_eq!(out.chars().count(), in_len.clamp(2, 32));

            let kept: String = nick.chars().take(32).collect();
            prop_assert!(
                out.starts_with(&kept),
                "output {out:?} dropped input prefix {kept:?}"
            );
            prop_assert!(
                out.chars().skip(in_len).all(|c| c == '_'),
                "padding in {out:?} must be underscores only"
            );
        }
    }

    // --- suppress_mentions ---

    #[test]
    fn at_everyone_gets_zwsp() {
        assert_eq!(
            suppress_mentions("hello @everyone!"),
            "hello @\u{200B}everyone!"
        );
    }

    #[test]
    fn at_here_gets_zwsp() {
        assert_eq!(suppress_mentions("hey @here"), "hey @\u{200B}here");
    }

    #[test]
    fn at_everyone_case_insensitive() {
        assert_eq!(suppress_mentions("@EVERYONE"), "@\u{200B}EVERYONE");
        assert_eq!(suppress_mentions("@Everyone"), "@\u{200B}Everyone");
    }

    #[test]
    fn at_here_case_insensitive() {
        assert_eq!(suppress_mentions("@HERE"), "@\u{200B}HERE");
        assert_eq!(suppress_mentions("@Here"), "@\u{200B}Here");
    }

    #[test]
    fn text_without_mentions_unchanged() {
        assert_eq!(suppress_mentions("hello world"), "hello world");
    }

    #[test]
    fn at_sign_not_followed_by_mention_unchanged() {
        assert_eq!(suppress_mentions("user@example.com"), "user@example.com");
    }

    #[test]
    fn multiple_mentions_all_suppressed() {
        let out = suppress_mentions("@everyone and @here");
        assert_eq!(out, "@\u{200B}everyone and @\u{200B}here");
    }

    #[test]
    fn at_sign_at_end_of_string_unchanged() {
        assert_eq!(suppress_mentions("end@"), "end@");
    }

    /// Text stitched from mention trigger words (in mixed case), bare `@`s,
    /// and plain fragments — the shapes most likely to defeat suppression.
    fn mentionish_text() -> impl Strategy<Value = String> {
        prop::collection::vec(
            prop_oneof![
                3 => Just("@everyone".to_string()),
                2 => Just("@here".to_string()),
                1 => Just("@EvErYoNe".to_string()),
                1 => Just("@HeRe".to_string()),
                2 => Just("@".to_string()),
                3 => "[a-zA-Z0-9 .!]{0,10}",
            ],
            0..8,
        )
        .prop_map(|parts| parts.concat())
    }

    proptest! {
        /// Text with no @everyone or @here must pass through unchanged.
        #[test]
        fn suppress_is_noop_without_trigger_words(
            s in "[^@]*" // no '@' at all
        ) {
            prop_assert_eq!(suppress_mentions(&s), s);
        }

        /// No case variant of `@everyone` / `@here` survives suppression —
        /// this is the mandatory IRC→Discord safety rule, so it must hold
        /// for every composition of trigger words and surrounding text.
        #[test]
        fn suppress_leaves_no_pingable_mention(text in mentionish_text()) {
            let out = suppress_mentions(&text).to_lowercase();
            prop_assert!(!out.contains("@everyone"), "output still pings: {out:?}");
            prop_assert!(!out.contains("@here"), "output still pings: {out:?}");
        }

        /// Suppression is idempotent: a second pass never inserts another
        /// zero-width space.
        #[test]
        fn suppress_is_idempotent(text in mentionish_text()) {
            let once = suppress_mentions(&text);
            prop_assert_eq!(suppress_mentions(&once), once.clone());
        }
    }

    // --- snapshot_from_cache ---

    #[test]
    fn snapshot_from_cache_returns_none_for_unknown_channel() {
        let cache = Cache::new();
        let empty = std::collections::HashSet::new();
        assert!(snapshot_from_cache(&cache, 99_999, &empty).is_none());
    }

    /// A `GUILD_CREATE` payload, as serenity would receive it over the Gateway.
    ///
    /// Guild 1 owns channels 10 and 11 and members 5 (online, nicked "Ali")
    /// and 6 (no presence entry, so offline). This is the minimal field set
    /// serenity's `GuildCreateEvent` deserializer accepts.
    const GUILD_CREATE_JSON: &str = r#"{
        "id": "1", "name": "guild", "icon": null, "icon_hash": null,
        "splash": null, "discovery_splash": null, "owner_id": "2",
        "verification_level": 0, "default_message_notifications": 0,
        "explicit_content_filter": 0, "roles": [], "emojis": [],
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
        "channels": [
            {"id":"10","type":0,"name":"general","guild_id":"1","position":0,"permission_overwrites":[]},
            {"id":"11","type":0,"name":"other","guild_id":"1","position":1,"permission_overwrites":[]}
        ],
        "members": [
            {"user":{"id":"5","username":"alice","discriminator":"0000","global_name":null,"avatar":null},
             "nick":"Ali","roles":[],"joined_at":"2025-01-01T00:00:00.000Z","deaf":false,"mute":false,"flags":0},
            {"user":{"id":"6","username":"bob","discriminator":"0000","global_name":null,"avatar":null},
             "nick":null,"roles":[],"joined_at":"2025-01-01T00:00:00.000Z","deaf":false,"mute":false,"flags":0}
        ],
        "presences": [
            {"user":{"id":"5"},"status":"online","activities":[],"client_status":{}}
        ]
    }"#;

    /// Build a cache populated from [`GUILD_CREATE_JSON`].
    fn populated_cache() -> Cache {
        let mut event: serenity::model::event::GuildCreateEvent =
            serde_json::from_str(GUILD_CREATE_JSON).expect("guild fixture should deserialize");
        let cache = Cache::new();
        cache.update(&mut event);
        cache
    }

    #[test]
    fn snapshot_from_cache_builds_event_for_cached_channel() {
        let cache = populated_cache();
        let bridged: std::collections::HashSet<u64> = [10].into_iter().collect();

        let event = snapshot_from_cache(&cache, 10, &bridged).expect("channel 10 is cached");

        let DiscordEvent::MemberSnapshot {
            guild_id,
            members,
            channel_ids,
            channel_names,
            role_names,
            ..
        } = event
        else {
            panic!("expected a MemberSnapshot");
        };

        assert_eq!(guild_id, 1, "must resolve the owning guild by channel");

        // Only the member with a non-offline presence is included, and the
        // guild nick wins over the username.
        assert_eq!(members.len(), 1, "offline member must be excluded");
        assert_eq!(members[0].user_id, 5);
        assert_eq!(members[0].display_name, "Ali");
        assert_eq!(members[0].presence, DiscordPresence::Online);

        // Channel 11 belongs to the guild but is not bridged.
        assert_eq!(channel_ids, vec![10], "only bridged channels are reported");

        // The reload path deliberately leaves these empty.
        assert!(channel_names.is_empty());
        assert!(role_names.is_empty());
    }

    /// The guild is cached, but the requested channel is not one of its
    /// channels — the guild scan must not match it.
    #[test]
    fn snapshot_from_cache_returns_none_for_channel_outside_cached_guild() {
        let cache = populated_cache();
        let bridged: std::collections::HashSet<u64> = [10].into_iter().collect();
        assert!(snapshot_from_cache(&cache, 12_345, &bridged).is_none());
    }

    /// Every bridged channel of the guild is reported, not just the one asked
    /// about — the snapshot seeds routing for the whole guild.
    #[test]
    fn snapshot_from_cache_reports_all_bridged_channels_of_the_guild() {
        let cache = populated_cache();
        let bridged: std::collections::HashSet<u64> = [10, 11].into_iter().collect();

        let event = snapshot_from_cache(&cache, 10, &bridged).expect("channel 10 is cached");
        let DiscordEvent::MemberSnapshot {
            mut channel_ids, ..
        } = event
        else {
            panic!("expected a MemberSnapshot");
        };
        channel_ids.sort_unstable();
        assert_eq!(channel_ids, vec![10, 11]);
    }

    // --- non_offline_member_infos ---

    fn raw(user_id: u64, username: &str, nick: Option<&str>, global: Option<&str>) -> RawMember {
        RawMember {
            user_id,
            username: username.to_owned(),
            nick: nick.map(str::to_owned),
            global_name: global.map(str::to_owned),
        }
    }

    fn presence_map(entries: &[(u64, DiscordPresence)]) -> HashMap<u64, DiscordPresence> {
        entries.iter().copied().collect()
    }

    #[test]
    fn member_absent_from_presence_map_is_excluded() {
        let members = vec![raw(1, "alice", None, None)];
        // Empty presence map — member defaults to Offline and is filtered out.
        let out = non_offline_member_infos(&members, &HashMap::new());
        assert!(out.is_empty(), "unknown presence must default to offline");
    }

    #[test]
    fn explicitly_offline_member_is_excluded() {
        let members = vec![raw(1, "alice", None, None)];
        let out =
            non_offline_member_infos(&members, &presence_map(&[(1, DiscordPresence::Offline)]));
        assert!(out.is_empty());
    }

    #[test]
    fn all_non_offline_presences_are_included() {
        let members = vec![
            raw(1, "alice", None, None),
            raw(2, "bob", None, None),
            raw(3, "carol", None, None),
        ];
        let out = non_offline_member_infos(
            &members,
            &presence_map(&[
                (1, DiscordPresence::Online),
                (2, DiscordPresence::Idle),
                (3, DiscordPresence::DoNotDisturb),
            ]),
        );
        assert_eq!(out.len(), 3);
        // Presence must be carried through, not defaulted.
        let by_id: HashMap<u64, DiscordPresence> =
            out.iter().map(|m| (m.user_id, m.presence)).collect();
        assert_eq!(by_id[&1], DiscordPresence::Online);
        assert_eq!(by_id[&2], DiscordPresence::Idle);
        assert_eq!(by_id[&3], DiscordPresence::DoNotDisturb);
    }

    #[test]
    fn mixed_presences_keep_only_non_offline() {
        let members = vec![
            raw(1, "online", None, None),
            raw(2, "offline", None, None),
            raw(3, "idle", None, None),
        ];
        let out = non_offline_member_infos(
            &members,
            &presence_map(&[
                (1, DiscordPresence::Online),
                (2, DiscordPresence::Offline),
                (3, DiscordPresence::Idle),
            ]),
        );
        let ids: Vec<u64> = out.iter().map(|m| m.user_id).collect();
        assert_eq!(ids, vec![1, 3], "offline member must be dropped");
    }

    #[test]
    fn display_name_prefers_nick_then_global_then_username() {
        let members = vec![
            raw(1, "uname1", Some("Nick"), Some("Global")),
            raw(2, "uname2", None, Some("Global")),
            raw(3, "uname3", None, None),
        ];
        let out = non_offline_member_infos(
            &members,
            &presence_map(&[
                (1, DiscordPresence::Online),
                (2, DiscordPresence::Online),
                (3, DiscordPresence::Online),
            ]),
        );
        let by_id: HashMap<u64, &str> = out
            .iter()
            .map(|m| (m.user_id, m.display_name.as_str()))
            .collect();
        assert_eq!(by_id[&1], "Nick");
        assert_eq!(by_id[&2], "Global");
        assert_eq!(by_id[&3], "uname3");
    }

    #[test]
    fn username_is_carried_through_unchanged() {
        let members = vec![raw(7, "real_username", Some("Displayed"), None)];
        let out =
            non_offline_member_infos(&members, &presence_map(&[(7, DiscordPresence::Online)]));
        assert_eq!(out[0].username, "real_username");
        assert_eq!(out[0].display_name, "Displayed");
    }

    #[test]
    fn empty_member_list_yields_empty_output() {
        assert!(non_offline_member_infos(&[], &HashMap::new()).is_empty());
    }

    // --- filter_bridged_channels ---

    #[test]
    fn only_bridged_channel_ids_are_kept() {
        let bridged = hset(&[10, 30]);
        let out = filter_bridged_channels([10, 20, 30, 40].into_iter(), &bridged);
        assert_eq!(out, vec![10, 30]);
    }

    #[test]
    fn empty_bridged_set_filters_everything_out() {
        let out = filter_bridged_channels([1, 2, 3].into_iter(), &hset(&[]));
        assert!(out.is_empty());
    }

    #[test]
    fn bridged_ids_not_present_in_guild_are_not_invented() {
        // 99 is bridged but not among the guild's channels — must not appear.
        let bridged = hset(&[10, 99]);
        let out = filter_bridged_channels([10, 20].into_iter(), &bridged);
        assert_eq!(out, vec![10]);
    }

    // --- wiremock integration tests for send_discord_message ---

    mod send_integration {
        use super::*;
        use wiremock::matchers::{method, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        /// Build a serenity `Http` client that routes all requests through
        /// the given wiremock server.
        fn mock_http(server: &MockServer) -> Http {
            serenity::http::HttpBuilder::new("test-token")
                .proxy(server.uri())
                .ratelimiter_disabled(true)
                .build()
        }

        // Webhook ID must be 17-20 digits; token must be 60-68 chars.
        const WEBHOOK_ID: &str = "12345678901234567";
        const WEBHOOK_TOKEN: &str =
            "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ01";

        /// Minimal JSON that serenity can deserialize as a `Webhook`.
        fn webhook_json() -> serde_json::Value {
            serde_json::json!({
                "id": WEBHOOK_ID,
                "type": 1,
                "channel_id": "999",
                "token": WEBHOOK_TOKEN
            })
        }

        fn webhook_url() -> String {
            format!("https://discord.com/api/webhooks/{WEBHOOK_ID}/{WEBHOOK_TOKEN}")
        }

        /// Minimal JSON that serenity can deserialize as a `Message`.
        fn message_json() -> serde_json::Value {
            serde_json::json!({
                "id": "1",
                "channel_id": "999",
                "author": {
                    "id": "1",
                    "username": "bot",
                    "discriminator": "0000",
                    "global_name": null,
                    "avatar": null
                },
                "content": "",
                "timestamp": "2025-01-01T00:00:00.000Z",
                "tts": false,
                "mention_everyone": false,
                "mentions": [],
                "mention_roles": [],
                "attachments": [],
                "embeds": [],
                "pinned": false,
                "type": 0
            })
        }

        #[tokio::test]
        async fn webhook_send_posts_correct_payload() {
            let server = MockServer::start().await;
            let http = mock_http(&server);

            // Mock GET (webhook resolve).
            Mock::given(method("GET"))
                .and(path_regex(r"webhooks/\d+/"))
                .respond_with(ResponseTemplate::new(200).set_body_json(webhook_json()))
                .mount(&server)
                .await;

            // Mock POST (webhook execute) — expect exactly 1.
            let post_mock = Mock::given(method("POST"))
                .and(path_regex(r"webhooks/\d+/"))
                .respond_with(ResponseTemplate::new(200).set_body_json(message_json()))
                .expect(1)
                .mount_as_scoped(&server)
                .await;

            send_discord_message(&http, 999, Some(&webhook_url()), "TestNick", "hello world").await;

            // Scoped mock asserts exactly 1 POST was received on drop.
            drop(post_mock);
        }

        #[tokio::test]
        async fn plain_send_posts_formatted_message() {
            let server = MockServer::start().await;
            let http = mock_http(&server);

            let post_mock = Mock::given(method("POST"))
                .and(path_regex(r"/api/v\d+/channels/999/messages"))
                .respond_with(ResponseTemplate::new(200).set_body_json(message_json()))
                .expect(1)
                .mount_as_scoped(&server)
                .await;

            send_discord_message(&http, 999, None, "TestNick", "hello world").await;

            drop(post_mock);
        }

        #[tokio::test]
        async fn plain_send_suppresses_at_mentions() {
            let server = MockServer::start().await;
            let http = mock_http(&server);

            Mock::given(method("POST"))
                .and(path_regex(r"/api/v\d+/channels/999/messages"))
                .respond_with(ResponseTemplate::new(200).set_body_json(message_json()))
                .mount(&server)
                .await;

            // send_discord_message uses suppress_mentions on plain path.
            // This test verifies the function doesn't panic and completes.
            // The actual mention suppression is tested in suppress_mentions unit tests.
            send_discord_message(&http, 999, None, "@everyone", "@here check this").await;
        }

        #[tokio::test]
        async fn webhook_resolve_failure_falls_back_to_plain_send() {
            let server = MockServer::start().await;
            let http = mock_http(&server);

            // Webhook resolve returns 404 (deleted webhook / rotated token).
            Mock::given(method("GET"))
                .and(path_regex(r"webhooks/\d+/"))
                .respond_with(ResponseTemplate::new(404))
                .mount(&server)
                .await;

            // Plain channel send must receive the fallback POST.
            let post_mock = Mock::given(method("POST"))
                .and(path_regex(r"/api/v\d+/channels/999/messages"))
                .respond_with(ResponseTemplate::new(200).set_body_json(message_json()))
                .expect(1)
                .mount_as_scoped(&server)
                .await;

            send_discord_message(&http, 999, Some(&webhook_url()), "TestNick", "hello world").await;

            // Asserts exactly 1 plain channel POST on drop.
            drop(post_mock);
        }

        // --- send_dm integration tests ---

        const DM_CHANNEL_ID: &str = "888";

        fn dm_channel_json() -> serde_json::Value {
            serde_json::json!({
                "id": DM_CHANNEL_ID,
                "type": 1,
                "last_message_id": null,
                "last_pin_timestamp": null,
                "recipients": [{
                    "id": "42",
                    "username": "recipient",
                    "discriminator": "0000",
                    "global_name": null,
                    "avatar": null
                }]
            })
        }

        #[tokio::test]
        async fn send_dm_opens_channel_and_posts_message() {
            let server = MockServer::start().await;
            let http = mock_http(&server);

            // POST /users/@me/channels — open DM channel
            let open_mock = Mock::given(method("POST"))
                .and(path_regex(r"/users/@me/channels"))
                .respond_with(ResponseTemplate::new(200).set_body_json(dm_channel_json()))
                .expect(1)
                .mount_as_scoped(&server)
                .await;

            // POST /channels/{DM_CHANNEL_ID}/messages — send message
            let send_mock = Mock::given(method("POST"))
                .and(path_regex(
                    format!(r"/channels/{DM_CHANNEL_ID}/messages").as_str(),
                ))
                .respond_with(ResponseTemplate::new(200).set_body_json(message_json()))
                .expect(1)
                .mount_as_scoped(&server)
                .await;

            send_dm(&http, 42, "**[nick]** hello").await;

            drop(open_mock);
            drop(send_mock);
        }

        #[tokio::test]
        async fn send_dm_suppresses_at_everyone_and_at_here() {
            use wiremock::matchers::body_string_contains;

            let server = MockServer::start().await;
            let http = mock_http(&server);

            Mock::given(method("POST"))
                .and(path_regex(r"/users/@me/channels"))
                .respond_with(ResponseTemplate::new(200).set_body_json(dm_channel_json()))
                .mount(&server)
                .await;

            // The message body must contain the ZWSP-suppressed forms, not the raw pings.
            let send_mock = Mock::given(method("POST"))
                .and(path_regex(
                    format!(r"/channels/{DM_CHANNEL_ID}/messages").as_str(),
                ))
                .and(body_string_contains("@\u{200B}everyone"))
                .and(body_string_contains("@\u{200B}here"))
                .respond_with(ResponseTemplate::new(200).set_body_json(message_json()))
                .expect(1)
                .mount_as_scoped(&server)
                .await;

            send_dm(&http, 42, "**[nick]** @everyone @here look").await;

            drop(send_mock);
        }

        #[tokio::test]
        async fn send_dm_dropped_when_open_channel_fails() {
            let server = MockServer::start().await;
            let http = mock_http(&server);

            // Open DM returns 403 — user has DMs disabled or blocked the bot.
            Mock::given(method("POST"))
                .and(path_regex(r"/users/@me/channels"))
                .respond_with(ResponseTemplate::new(403))
                .mount(&server)
                .await;

            // The send POST must NOT be attempted after open failure.
            let send_mock = Mock::given(method("POST"))
                .and(path_regex(r"/channels/\d+/messages"))
                .respond_with(ResponseTemplate::new(200).set_body_json(message_json()))
                .expect(0)
                .mount_as_scoped(&server)
                .await;

            send_dm(&http, 42, "hello").await;

            drop(send_mock);
        }

        #[tokio::test]
        async fn send_dm_swallows_send_failure() {
            let server = MockServer::start().await;
            let http = mock_http(&server);

            Mock::given(method("POST"))
                .and(path_regex(r"/users/@me/channels"))
                .respond_with(ResponseTemplate::new(200).set_body_json(dm_channel_json()))
                .mount(&server)
                .await;

            // Send returns 500; must not panic.  Function returns () either way.
            let send_mock = Mock::given(method("POST"))
                .and(path_regex(
                    format!(r"/channels/{DM_CHANNEL_ID}/messages").as_str(),
                ))
                .respond_with(ResponseTemplate::new(500))
                .expect(1)
                .mount_as_scoped(&server)
                .await;

            send_dm(&http, 42, "hello").await;

            drop(send_mock);
        }

        // --- process_discord_commands dispatch ---

        /// Spawn the command processor wired to a mock server, returning the
        /// command sender, the event receiver, and the shared routing tables.
        #[allow(clippy::type_complexity)]
        fn spawn_processor(
            server: &MockServer,
        ) -> (
            mpsc::Sender<DiscordCommand>,
            mpsc::Receiver<DiscordEvent>,
            Arc<RwLock<HashSet<u64>>>,
            Arc<RwLock<HashSet<u64>>>,
            tokio::task::JoinHandle<()>,
        ) {
            let http = Arc::new(mock_http(server));
            let cache = Arc::new(Cache::new());
            let (cmd_tx, cmd_rx) = mpsc::channel(8);
            let (event_tx, event_rx) = mpsc::channel(8);
            let self_filter: Arc<RwLock<HashSet<u64>>> = Arc::new(RwLock::new(HashSet::new()));
            let channel_ids: Arc<RwLock<HashSet<u64>>> = Arc::new(RwLock::new(HashSet::new()));

            let handle = tokio::spawn(process_discord_commands(
                http,
                cache,
                cmd_rx,
                event_tx,
                Arc::clone(&self_filter),
                Arc::clone(&channel_ids),
            ));

            (cmd_tx, event_rx, self_filter, channel_ids, handle)
        }

        #[tokio::test]
        async fn send_message_command_dispatches_to_channel_send() {
            let server = MockServer::start().await;

            let post_mock = Mock::given(method("POST"))
                .and(path_regex(r"/api/v\d+/channels/999/messages"))
                .respond_with(ResponseTemplate::new(200).set_body_json(message_json()))
                .expect(1)
                .mount_as_scoped(&server)
                .await;

            let (cmd_tx, _event_rx, _sf, _cids, handle) = spawn_processor(&server);

            cmd_tx
                .send(DiscordCommand::SendMessage {
                    channel_id: 999,
                    webhook_url: None,
                    sender_nick: "nick".into(),
                    text: "**[nick]** hello".into(),
                })
                .await
                .unwrap();

            // Dropping the sender ends the loop; awaiting the handle guarantees
            // the command was fully processed before we assert.
            drop(cmd_tx);
            handle.await.unwrap();

            drop(post_mock);
        }

        #[tokio::test]
        async fn send_message_command_uses_webhook_when_url_present() {
            let server = MockServer::start().await;

            Mock::given(method("GET"))
                .and(path_regex(r"webhooks/\d+/"))
                .respond_with(ResponseTemplate::new(200).set_body_json(webhook_json()))
                .mount(&server)
                .await;

            let post_mock = Mock::given(method("POST"))
                .and(path_regex(r"webhooks/\d+/"))
                .respond_with(ResponseTemplate::new(200).set_body_json(message_json()))
                .expect(1)
                .mount_as_scoped(&server)
                .await;

            let (cmd_tx, _event_rx, _sf, _cids, handle) = spawn_processor(&server);

            cmd_tx
                .send(DiscordCommand::SendMessage {
                    channel_id: 999,
                    webhook_url: Some(webhook_url()),
                    sender_nick: "nick".into(),
                    text: "hello".into(),
                })
                .await
                .unwrap();

            drop(cmd_tx);
            handle.await.unwrap();

            drop(post_mock);
        }

        #[tokio::test]
        async fn send_dm_command_dispatches_to_dm_send() {
            let server = MockServer::start().await;

            let open_mock = Mock::given(method("POST"))
                .and(path_regex(r"/users/@me/channels"))
                .respond_with(ResponseTemplate::new(200).set_body_json(dm_channel_json()))
                .expect(1)
                .mount_as_scoped(&server)
                .await;

            let send_mock = Mock::given(method("POST"))
                .and(path_regex(
                    format!(r"/channels/{DM_CHANNEL_ID}/messages").as_str(),
                ))
                .respond_with(ResponseTemplate::new(200).set_body_json(message_json()))
                .expect(1)
                .mount_as_scoped(&server)
                .await;

            let (cmd_tx, _event_rx, _sf, _cids, handle) = spawn_processor(&server);

            cmd_tx
                .send(DiscordCommand::SendDm {
                    recipient_user_id: 42,
                    text: "**[nick]** hi".into(),
                })
                .await
                .unwrap();

            drop(cmd_tx);
            handle.await.unwrap();

            drop(open_mock);
            drop(send_mock);
        }

        /// `SendBotDm` shares an arm with `SendDm` — it must take the same path.
        #[tokio::test]
        async fn send_bot_dm_command_dispatches_to_dm_send() {
            let server = MockServer::start().await;

            let open_mock = Mock::given(method("POST"))
                .and(path_regex(r"/users/@me/channels"))
                .respond_with(ResponseTemplate::new(200).set_body_json(dm_channel_json()))
                .expect(1)
                .mount_as_scoped(&server)
                .await;

            let send_mock = Mock::given(method("POST"))
                .and(path_regex(
                    format!(r"/channels/{DM_CHANNEL_ID}/messages").as_str(),
                ))
                .respond_with(ResponseTemplate::new(200).set_body_json(message_json()))
                .expect(1)
                .mount_as_scoped(&server)
                .await;

            let (cmd_tx, _event_rx, _sf, _cids, handle) = spawn_processor(&server);

            cmd_tx
                .send(DiscordCommand::SendBotDm {
                    recipient_user_id: 42,
                    text: "bot notice".into(),
                })
                .await
                .unwrap();

            drop(cmd_tx);
            handle.await.unwrap();

            drop(open_mock);
            drop(send_mock);
        }

        #[tokio::test]
        async fn reload_bridges_command_updates_routing_tables() {
            let server = MockServer::start().await;
            let (cmd_tx, _event_rx, self_filter, channel_ids, handle) = spawn_processor(&server);

            // Seed then mutate, so both add and remove paths are exercised.
            channel_ids.write().await.insert(555);
            self_filter.write().await.insert(777);

            cmd_tx
                .send(DiscordCommand::ReloadBridges {
                    added_channel_ids: vec![111],
                    removed_channel_ids: vec![555],
                    added_webhook_ids: vec![222],
                    removed_webhook_ids: vec![777],
                })
                .await
                .unwrap();

            drop(cmd_tx);
            handle.await.unwrap();

            let cids = channel_ids.read().await;
            assert!(cids.contains(&111), "added channel must be routed");
            assert!(!cids.contains(&555), "removed channel must be dropped");
            let sf = self_filter.read().await;
            assert!(sf.contains(&222), "added webhook must be filtered");
            assert!(!sf.contains(&777), "removed webhook must be unfiltered");
        }

        /// With an empty cache no snapshot can be built, so the added channel
        /// must produce a warning rather than an event.
        #[tokio::test]
        async fn reload_bridges_emits_no_event_when_channel_absent_from_cache() {
            let server = MockServer::start().await;
            let (cmd_tx, mut event_rx, _sf, _cids, handle) = spawn_processor(&server);

            cmd_tx
                .send(DiscordCommand::ReloadBridges {
                    added_channel_ids: vec![111],
                    removed_channel_ids: vec![],
                    added_webhook_ids: vec![],
                    removed_webhook_ids: vec![],
                })
                .await
                .unwrap();

            drop(cmd_tx);
            handle.await.unwrap();

            // The processor has exited and dropped its event_tx, so recv()
            // resolves to None only if nothing was ever emitted.
            assert!(
                event_rx.recv().await.is_none(),
                "no MemberSnapshot should be emitted for an uncached channel"
            );
        }

        /// The loop must drain every queued command, not just the first.
        #[tokio::test]
        async fn processor_drains_all_queued_commands() {
            let server = MockServer::start().await;

            let post_mock = Mock::given(method("POST"))
                .and(path_regex(r"/api/v\d+/channels/999/messages"))
                .respond_with(ResponseTemplate::new(200).set_body_json(message_json()))
                .expect(3)
                .mount_as_scoped(&server)
                .await;

            let (cmd_tx, _event_rx, _sf, _cids, handle) = spawn_processor(&server);

            for i in 0..3 {
                cmd_tx
                    .send(DiscordCommand::SendMessage {
                        channel_id: 999,
                        webhook_url: None,
                        sender_nick: "nick".into(),
                        text: format!("message {i}"),
                    })
                    .await
                    .unwrap();
            }

            drop(cmd_tx);
            handle.await.unwrap();

            drop(post_mock);
        }
    }
}
