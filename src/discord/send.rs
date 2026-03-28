use std::collections::HashSet;
use std::sync::Arc;

use serenity::builder::{CreateAllowedMentions, CreateMessage, ExecuteWebhook};
use serenity::http::Http;
use serenity::model::channel::Channel;
use serenity::model::id::{ChannelId, GuildId};
use serenity::model::webhook::Webhook;
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, warn};

use crate::discord::handler::resolve_display_name;
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
        // Plain send: prepend nick and apply mention suppression to the text body
        let safe_text = format!("**[{}]** {}", sender_nick, suppress_mentions(text));
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

/// Fetch the guild ID that owns `channel_id` via REST.
///
/// Returns `None` and logs a warning if the channel cannot be resolved or is
/// not a guild channel.
async fn guild_id_for_channel(http: &Http, channel_id: u64) -> Option<GuildId> {
    match ChannelId::new(channel_id).to_channel(http).await {
        Ok(Channel::Guild(gc)) => Some(gc.guild_id),
        Ok(_) => {
            warn!(
                channel_id,
                "channel is not a guild channel; skipping member fetch"
            );
            None
        }
        Err(e) => {
            warn!(error = %e, channel_id, "failed to resolve channel; skipping member fetch");
            None
        }
    }
}

/// Fetch all guild members for `guild_id` via the REST API and build a
/// [`MemberSnapshot`] event.
///
/// Presences are not available via REST; all members are marked as offline.
/// Correct presence will arrive through subsequent `PRESENCE_UPDATE` events.
async fn fetch_member_snapshot(
    http: &Http,
    channel_id: u64,
    guild_id: GuildId,
    event_tx: &mpsc::Sender<DiscordEvent>,
) {
    let members_result = guild_id.members(http, None, None).await;
    match members_result {
        Err(e) => {
            warn!(error = %e, guild_id = guild_id.get(), "failed to fetch members for new channel");
        }
        Ok(members) => {
            let infos: Vec<MemberInfo> = members
                .iter()
                .map(|m| MemberInfo {
                    user_id: m.user.id.get(),
                    display_name: resolve_display_name(
                        m.nick.as_deref(),
                        m.user.global_name.as_deref(),
                        &m.user.name,
                    )
                    .to_owned(),
                    presence: DiscordPresence::Offline, // REST has no presence data
                })
                .collect();
            debug!(
                guild_id = guild_id.get(),
                count = infos.len(),
                "fetched member snapshot for new bridge channel"
            );
            let _ = event_tx
                .send(DiscordEvent::MemberSnapshot {
                    guild_id: guild_id.get(),
                    members: infos,
                })
                .await;
        }
    }
    let _ = channel_id; // channel_id is used for logging context by the caller
}

/// Drain [`DiscordCommand`]s from the bridging layer and dispatch them.
///
/// `event_tx` is used to emit [`DiscordEvent::MemberSnapshot`] events when a
/// new bridge channel is added via [`DiscordCommand::ReloadBridges`].
///
/// Runs until the sender side of `rx` is dropped.
pub(crate) async fn process_discord_commands(
    http: Arc<Http>,
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
                // Fetch and emit member snapshots for each newly added channel.
                for channel_id in added_channel_ids {
                    if let Some(guild_id) = guild_id_for_channel(&http, channel_id).await {
                        fetch_member_snapshot(&http, channel_id, guild_id, &event_tx).await;
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
}
