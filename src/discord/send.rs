use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serenity::builder::{CreateAllowedMentions, CreateMessage, ExecuteWebhook};
use serenity::cache::Cache;
use serenity::http::Http;
use serenity::model::id::ChannelId;
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
/// `channel.send()` otherwise. Failures are logged at `WARN` and dropped —
/// no retry is attempted.
pub(crate) async fn send_discord_message(
    http: &Http,
    channel_id: u64,
    webhook_url: Option<&str>,
    sender_nick: &str,
    text: &str,
) {
    if let Some(url) = webhook_url {
        let username = sanitize_webhook_username(sender_nick);
        let webhook = match Webhook::from_url(http, url).await {
            Ok(wh) => wh,
            Err(e) => {
                warn!(error = %e, url, "Failed to resolve webhook; dropping message");
                return;
            }
        };
        let execute = ExecuteWebhook::new()
            .username(username)
            .content(text)
            // parse: [] — no @everyone or @here pings (mandatory safety rule)
            .allowed_mentions(CreateAllowedMentions::new());
        if let Err(e) = webhook.execute(http, false, execute).await {
            warn!(error = %e, channel_id, "Webhook execute failed; dropping message");
        }
    } else {
        // Plain send: the text already contains the "**[nick]** content" prefix
        // (formatted by relay.rs with ping-fixed nick).  Only suppress @everyone
        // / @here mentions — do NOT re-wrap with the nick.
        let safe_text = suppress_mentions(text);
        let msg = CreateMessage::new().content(safe_text);
        if let Err(e) = ChannelId::new(channel_id).send_message(http, msg).await {
            warn!(error = %e, channel_id, "Channel send failed; dropping message");
        }
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

/// Build a [`DiscordEvent::MemberSnapshot`] for `channel_id` from the serenity
/// cache.
///
/// Looks up the channel's owning guild in the cache, then reads the guild's
/// `members` and `presences` maps — both already populated by `GUILD_CREATE`
/// and `GUILD_MEMBERS_CHUNK` / `PRESENCE_UPDATE` events.  No REST call is made.
///
/// Returns `None` if the channel or its guild is not present in the cache
/// (should not happen in normal operation after startup).
// mutants::skip — requires populated Serenity cache from live Discord connection
#[mutants::skip]
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

    // Only include non-offline members, consistent with build_member_snapshot_event.
    let members: Vec<MemberInfo> = guild
        .members
        .values()
        .filter_map(|m| {
            let presence = presences
                .get(&m.user.id.get())
                .copied()
                .unwrap_or(DiscordPresence::Offline);
            if !presence.is_non_offline() {
                return None;
            }
            Some(MemberInfo {
                user_id: m.user.id.get(),
                display_name: resolve_display_name(
                    m.nick.as_deref(),
                    m.user.global_name.as_deref(),
                    &m.user.name,
                )
                .to_owned(),
                presence,
            })
        })
        .collect();

    // Bridged Discord channel IDs that belong to this guild.
    let channel_ids: Vec<u64> = guild
        .channels
        .keys()
        .filter(|cid| all_bridged_channel_ids.contains(&cid.get()))
        .map(|cid| cid.get())
        .collect();

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
    })
}

/// Drain [`DiscordCommand`]s from the bridging layer and dispatch them.
///
/// `event_tx` is used to emit [`DiscordEvent::MemberSnapshot`] events when a
/// new bridge channel is added via [`DiscordCommand::ReloadBridges`].
///
/// Runs until the sender side of `rx` is dropped.
// mutants::skip — async event loop requiring live Discord HTTP + cache
#[mutants::skip]
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
        fn sanitize_always_produces_valid_length(nick in ".*") {
            let out = sanitize_webhook_username(&nick);
            let len = out.chars().count();
            prop_assert!(len >= 2, "output too short: {len}");
            prop_assert!(len <= 32, "output too long: {len}");
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

    proptest! {
        /// Text with no @everyone or @here must pass through unchanged.
        #[test]
        fn suppress_is_noop_without_trigger_words(
            s in "[^@]*" // no '@' at all
        ) {
            prop_assert_eq!(suppress_mentions(&s), s);
        }
    }

    // --- plain-send nick sanitization ---

    #[test]
    fn plain_send_format_suppresses_at_everyone_in_nick() {
        // Simulates the plain-send format string used in send_discord_message.
        let nick = "@everyone";
        let text = "hello";
        let safe_text = format!(
            "**[{}]** {}",
            suppress_mentions(nick),
            suppress_mentions(text)
        );
        assert!(
            !safe_text.contains("@everyone"),
            "plain-send must suppress @everyone in nick: {safe_text:?}"
        );
        assert!(safe_text.contains("@\u{200B}everyone"));
    }

    #[test]
    fn plain_send_format_suppresses_at_here_in_nick() {
        let nick = "@here";
        let text = "world";
        let safe_text = format!(
            "**[{}]** {}",
            suppress_mentions(nick),
            suppress_mentions(text)
        );
        assert!(
            !safe_text.contains("@here"),
            "plain-send must suppress @here in nick: {safe_text:?}"
        );
    }

    // --- snapshot_from_cache ---

    #[test]
    fn snapshot_from_cache_returns_none_for_unknown_channel() {
        let cache = Cache::new();
        let empty = std::collections::HashSet::new();
        assert!(snapshot_from_cache(&cache, 99_999, &empty).is_none());
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
        async fn webhook_resolve_failure_does_not_panic() {
            let server = MockServer::start().await;
            let http = mock_http(&server);

            // Return 404 for webhook resolve — send_discord_message should
            // log a warning and return without panicking.
            Mock::given(method("GET"))
                .and(path_regex(r"webhooks/\d+/"))
                .respond_with(ResponseTemplate::new(404))
                .mount(&server)
                .await;

            // No POST mock — if a POST is attempted, wiremock returns 404,
            // which is fine (we just verify no panic).
            send_discord_message(
                &http,
                999,
                Some(&webhook_url()),
                "TestNick",
                "this should be silently dropped",
            )
            .await;

            // If we reach here without panicking, the test passes.
        }
    }
}
