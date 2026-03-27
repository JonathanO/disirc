use std::sync::Arc;

use serenity::builder::{CreateAllowedMentions, CreateMessage, ExecuteWebhook};
use serenity::http::Http;
use serenity::model::id::ChannelId;
use serenity::model::webhook::Webhook;
use tokio::sync::mpsc;
use tracing::warn;

use crate::discord::types::DiscordCommand;

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

/// Drain [`DiscordCommand`]s from the bridging layer and dispatch them.
///
/// Runs until the sender side of `rx` is dropped.
pub(crate) async fn process_discord_commands(
    http: Arc<Http>,
    mut rx: mpsc::Receiver<DiscordCommand>,
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

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
