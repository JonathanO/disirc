//! IRC → Discord formatting: control code conversion, mention conversion,
//! ping-fix, and truncation.

use std::borrow::Cow;
use std::fmt::Write as _;

use super::{
    IRC_BOLD, IRC_COLOR, IRC_ITALIC, IRC_RESET, IRC_REVERSE, IRC_STRIKETHROUGH, IRC_UNDERLINE,
};

// ---------------------------------------------------------------------------
// Control character handling
// ---------------------------------------------------------------------------

/// Represents a styled span of IRC text.
#[derive(Debug, Clone, Default)]
struct IrcStyle {
    bold: bool,
    italic: bool,
    underline: bool,
    strikethrough: bool,
}

/// Parse IRC formatted text into styled spans, then emit Discord markdown.
#[must_use]
pub fn irc_to_discord_formatting(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut style = IrcStyle::default();
    let mut current_text = String::new();
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            IRC_BOLD => {
                flush_span(&mut result, &style, &current_text);
                current_text.clear();
                style.bold = !style.bold;
            }
            IRC_ITALIC | IRC_REVERSE => {
                // Spec: reverse is treated as italic for best-effort rendering
                flush_span(&mut result, &style, &current_text);
                current_text.clear();
                style.italic = !style.italic;
            }
            IRC_UNDERLINE => {
                flush_span(&mut result, &style, &current_text);
                current_text.clear();
                style.underline = !style.underline;
            }
            IRC_STRIKETHROUGH => {
                flush_span(&mut result, &style, &current_text);
                current_text.clear();
                style.strikethrough = !style.strikethrough;
            }
            IRC_COLOR => {
                // Strip color codes: \x03[N[,M]]
                // Consume up to 2 digits for foreground
                consume_color_digits(&mut chars);
                if chars.peek() == Some(&',') {
                    chars.next();
                    consume_color_digits(&mut chars);
                }
            }
            IRC_RESET => {
                flush_span(&mut result, &style, &current_text);
                current_text.clear();
                style = IrcStyle::default();
            }
            c if c.is_control() => {
                // Strip remaining Unicode Cc (control) characters
            }
            _ => {
                current_text.push(ch);
            }
        }
    }

    flush_span(&mut result, &style, &current_text);
    result
}

/// Consume up to 2 color digits from the iterator.
fn consume_color_digits(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    for _ in 0..2 {
        if chars.peek().is_some_and(char::is_ascii_digit) {
            chars.next();
        } else {
            break;
        }
    }
}

/// Flush a styled span into the result as Discord markdown.
fn flush_span(result: &mut String, style: &IrcStyle, text: &str) {
    if text.is_empty() {
        return;
    }

    // Apply formatting markers in a consistent order
    if style.bold {
        result.push_str("**");
    }
    if style.italic {
        result.push('*');
    }
    if style.underline {
        result.push_str("__");
    }
    if style.strikethrough {
        result.push_str("~~");
    }

    result.push_str(text);

    // Close in reverse order
    if style.strikethrough {
        result.push_str("~~");
    }
    if style.underline {
        result.push_str("__");
    }
    if style.italic {
        result.push('*');
    }
    if style.bold {
        result.push_str("**");
    }
}

// ---------------------------------------------------------------------------
// Mention conversion
// ---------------------------------------------------------------------------

/// Resolver trait for looking up IRC nicks → Discord user IDs.
pub trait IrcMentionResolver {
    /// Look up a Discord user ID from an IRC nick (case-insensitive).
    fn resolve_nick(&self, nick: &str) -> Option<String>;
}

/// Convert `@nick` in IRC text to Discord `<@user_id>` mentions.
#[must_use]
pub fn convert_irc_mentions(text: &str, resolver: &dyn IrcMentionResolver) -> String {
    let mut result = String::with_capacity(text.len());
    let mut chars = text.char_indices().peekable();

    while let Some((i, ch)) = chars.next() {
        if ch == '@'
            && let Some(&(nick_start, next_ch)) = chars.peek()
            && next_ch.is_ascii_alphanumeric()
        {
            // Extract the nick (alphanumeric, underscore, hyphen, brackets, etc.)
            let mut nick_end = nick_start;
            for &(j, c) in &chars.clone().collect::<Vec<_>>() {
                if c.is_ascii_alphanumeric()
                    || matches!(
                        c,
                        '_' | '-' | '[' | ']' | '\\' | '`' | '^' | '{' | '}' | '|'
                    )
                {
                    nick_end = j + c.len_utf8();
                } else {
                    break;
                }
            }
            let nick = &text[nick_start..nick_end];
            if let Some(user_id) = resolver.resolve_nick(nick) {
                write!(result, "<@{user_id}>").unwrap();
            } else {
                result.push_str(&text[i..nick_end]);
            }
            // Advance chars past the nick.
            while let Some(&(j, _)) = chars.peek() {
                if j >= nick_end {
                    break;
                }
                chars.next();
            }
        } else if ch == '@' {
            result.push('@');
        } else {
            result.push(ch);
        }
    }

    result
}

// ---------------------------------------------------------------------------
// Ping-fix
// ---------------------------------------------------------------------------

/// Insert a zero-width space after the first character of a nick.
///
/// This prevents Discord from pinging users whose display name matches.
/// Applied only to the nick field (webhook username or `[nick]` prefix).
#[must_use]
pub fn ping_fix_nick(nick: &str) -> String {
    let mut chars = nick.chars();
    match chars.next() {
        Some(first) => {
            let mut result = String::with_capacity(nick.len() + 3);
            result.push(first);
            result.push('\u{200B}');
            result.push_str(chars.as_str());
            result
        }
        None => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Length truncation
// ---------------------------------------------------------------------------

/// Discord's maximum message length.
const DISCORD_MAX_CHARS: usize = 2000;

/// Truncation suffix.
const TRUNCATION_SUFFIX: &str = "\u{2026} [truncated]";

/// Truncate a message to Discord's 2000 character limit at a word boundary.
#[must_use]
pub fn truncate_for_discord(text: &str) -> Cow<'_, str> {
    if text.chars().count() <= DISCORD_MAX_CHARS {
        return Cow::Borrowed(text);
    }

    let suffix_len = TRUNCATION_SUFFIX.chars().count();
    let target = DISCORD_MAX_CHARS - suffix_len;

    // Find the byte offset of the `target`-th char.
    // We know text has > DISCORD_MAX_CHARS chars and target < DISCORD_MAX_CHARS,
    // so the loop always breaks.
    let byte_pos = text
        .char_indices()
        .nth(target)
        .map_or(text.len(), |(i, _)| i);

    // Try to split at the last space before the limit
    let truncated = &text[..byte_pos];
    let split_at = truncated.rfind(' ').unwrap_or(byte_pos);

    let mut result = text[..split_at].to_string();
    result.push_str(TRUNCATION_SUFFIX);
    Cow::Owned(result)
}

// ---------------------------------------------------------------------------
// Full pipeline
// ---------------------------------------------------------------------------

/// Format an IRC message for Discord using the webhook path.
///
/// Returns `(username, body)` where username has ping-fix applied.
#[must_use]
pub fn irc_to_discord_webhook(
    nick: &str,
    text: &str,
    mention_resolver: &dyn IrcMentionResolver,
) -> (String, String) {
    let formatted = irc_to_discord_formatting(text);
    let with_mentions = convert_irc_mentions(&formatted, mention_resolver);
    let body = truncate_for_discord(&with_mentions).into_owned();
    let username = ping_fix_nick(nick);
    (username, body)
}

/// Format an IRC message for Discord using the plain path.
///
/// Returns a single string: `**[nick]** message text`.
#[must_use]
pub fn irc_to_discord_plain(
    nick: &str,
    text: &str,
    mention_resolver: &dyn IrcMentionResolver,
) -> String {
    let formatted = irc_to_discord_formatting(text);
    let with_mentions = convert_irc_mentions(&formatted, mention_resolver);
    let fixed_nick = ping_fix_nick(nick);
    let full = format!("**[{fixed_nick}]** {with_mentions}");
    truncate_for_discord(&full).into_owned()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;

    // -- Test helpers / stubs ------------------------------------------------

    struct StubIrcResolver;

    impl IrcMentionResolver for StubIrcResolver {
        fn resolve_nick(&self, nick: &str) -> Option<String> {
            match nick.to_lowercase().as_str() {
                "alice" => Some("111".to_string()),
                _ => None,
            }
        }
    }

    /// A resolver that matches any nick it's given (returns "42" as user ID).
    struct MatchAllIrcResolver;

    impl IrcMentionResolver for MatchAllIrcResolver {
        fn resolve_nick(&self, _nick: &str) -> Option<String> {
            Some("42".to_string())
        }
    }

    // -- Formatting conversion -----------------------------------------------

    #[test]
    fn irc_bold_to_discord() {
        assert_eq!(irc_to_discord_formatting("\x02hello\x02"), "**hello**");
    }

    #[test]
    fn irc_italic_to_discord() {
        assert_eq!(irc_to_discord_formatting("\x1dhello\x1d"), "*hello*");
    }

    #[test]
    fn irc_underline_to_discord() {
        assert_eq!(irc_to_discord_formatting("\x1fhello\x1f"), "__hello__");
    }

    #[test]
    fn irc_strikethrough_to_discord() {
        assert_eq!(irc_to_discord_formatting("\x1ehello\x1e"), "~~hello~~");
    }

    #[test]
    fn irc_reverse_treated_as_italic() {
        assert_eq!(irc_to_discord_formatting("\x16hello\x16"), "*hello*");
    }

    #[test]
    fn irc_color_stripped() {
        assert_eq!(irc_to_discord_formatting("\x034,5colored\x03"), "colored");
    }

    #[test]
    fn irc_color_single_digit() {
        assert_eq!(irc_to_discord_formatting("\x034text\x03"), "text");
    }

    #[test]
    fn irc_reset_clears_styles() {
        let input = "\x02bold\x0f normal";
        assert_eq!(irc_to_discord_formatting(input), "**bold** normal");
    }

    #[test]
    fn irc_control_chars_stripped() {
        assert_eq!(irc_to_discord_formatting("\x01hello"), "hello");
    }

    #[test]
    fn irc_plain_text_unchanged() {
        assert_eq!(irc_to_discord_formatting("hello world"), "hello world");
    }

    #[test]
    fn irc_nested_bold_italic() {
        let input = "\x02bold \x1dand italic\x1d only bold\x02";
        let result = irc_to_discord_formatting(input);
        assert_eq!(result, "**bold *****and italic***** only bold**");
    }

    #[test]
    fn irc_control_char_below_0x20_stripped() {
        assert_eq!(irc_to_discord_formatting("\x05hello\x07"), "hello");
    }

    #[test]
    fn irc_control_char_0x1f_boundary() {
        assert_eq!(irc_to_discord_formatting(" hello"), " hello");
        assert_eq!(irc_to_discord_formatting("\x1fhi\x1f"), "__hi__");
    }

    #[test]
    fn irc_del_stripped() {
        let result = irc_to_discord_formatting("a\x7fb");
        assert_eq!(result, "ab");
    }

    // -- Mention conversion --------------------------------------------------

    #[test]
    fn irc_mention_converts_known_nick() {
        let r = convert_irc_mentions("hello @alice world", &StubIrcResolver);
        assert_eq!(r, "hello <@111> world");
    }

    #[test]
    fn irc_mention_with_trailing_punctuation() {
        let r = convert_irc_mentions("hello @alice!", &StubIrcResolver);
        assert_eq!(r, "hello <@111>!");
    }

    #[test]
    fn irc_mention_leaves_unknown_nick() {
        let r = convert_irc_mentions("@unknown test", &StubIrcResolver);
        assert_eq!(r, "@unknown test");
    }

    #[test]
    fn irc_mention_at_end_of_string() {
        let r = convert_irc_mentions("hey @alice", &StubIrcResolver);
        assert_eq!(r, "hey <@111>");
    }

    #[test]
    fn irc_no_mention_bare_at() {
        let r = convert_irc_mentions("email@ test", &StubIrcResolver);
        assert_eq!(r, "email@ test");
    }

    #[test]
    fn irc_mention_nick_with_underscore() {
        let r = convert_irc_mentions("@foo_bar end", &MatchAllIrcResolver);
        assert_eq!(r, "<@42> end");
    }

    #[test]
    fn irc_mention_nick_with_hyphen() {
        let r = convert_irc_mentions("@foo-bar end", &MatchAllIrcResolver);
        assert_eq!(r, "<@42> end");
    }

    #[test]
    fn irc_mention_nick_with_brackets() {
        let r = convert_irc_mentions("@foo[bar] end", &MatchAllIrcResolver);
        assert_eq!(r, "<@42> end");
    }

    #[test]
    fn irc_mention_nick_with_backslash() {
        let r = convert_irc_mentions("@foo\\bar end", &MatchAllIrcResolver);
        assert_eq!(r, "<@42> end");
    }

    #[test]
    fn irc_mention_nick_with_backtick() {
        let r = convert_irc_mentions("@foo`bar end", &MatchAllIrcResolver);
        assert_eq!(r, "<@42> end");
    }

    #[test]
    fn irc_mention_nick_with_caret() {
        let r = convert_irc_mentions("@foo^bar end", &MatchAllIrcResolver);
        assert_eq!(r, "<@42> end");
    }

    #[test]
    fn irc_mention_nick_with_braces() {
        let r = convert_irc_mentions("@foo{bar} end", &MatchAllIrcResolver);
        assert_eq!(r, "<@42> end");
    }

    #[test]
    fn irc_mention_at_end() {
        let r = convert_irc_mentions("test @", &MatchAllIrcResolver);
        assert_eq!(r, "test @");
    }

    #[test]
    fn irc_mention_at_followed_by_space() {
        let r = convert_irc_mentions("@ space", &MatchAllIrcResolver);
        assert_eq!(r, "@ space");
    }

    #[test]
    fn irc_mention_preserves_multibyte_utf8() {
        let r = convert_irc_mentions("café @alice résumé", &StubIrcResolver);
        assert_eq!(r, "café <@111> résumé");
    }

    #[test]
    fn irc_mention_preserves_emoji() {
        let r = convert_irc_mentions("hello @alice 🎉🌍", &StubIrcResolver);
        assert_eq!(r, "hello <@111> 🎉🌍");
    }

    #[test]
    fn irc_mention_preserves_cjk() {
        let r = convert_irc_mentions("こんにちは @alice 世界", &StubIrcResolver);
        assert_eq!(r, "こんにちは <@111> 世界");
    }

    // -- Ping-fix ------------------------------------------------------------

    #[test]
    fn ping_fix_inserts_zwsp() {
        assert_eq!(ping_fix_nick("alice"), "a\u{200B}lice");
    }

    #[test]
    fn ping_fix_single_char() {
        assert_eq!(ping_fix_nick("a"), "a\u{200B}");
    }

    #[test]
    fn ping_fix_empty() {
        assert_eq!(ping_fix_nick(""), "");
    }

    // -- Truncation ----------------------------------------------------------

    #[test]
    fn truncate_short_message() {
        let msg = "short";
        assert_eq!(truncate_for_discord(msg).as_ref(), "short");
    }

    #[test]
    fn truncate_long_message() {
        let msg: String = "a ".repeat(1100);
        let result = truncate_for_discord(&msg);
        assert!(result.chars().count() <= DISCORD_MAX_CHARS);
        assert!(result.ends_with(TRUNCATION_SUFFIX));
    }

    #[test]
    fn truncate_returns_borrowed_when_short() {
        let msg = "hello";
        assert!(matches!(truncate_for_discord(msg), Cow::Borrowed(_)));
    }

    #[test]
    fn truncate_exactly_2000_chars() {
        let msg: String = "a".repeat(2000);
        let result = truncate_for_discord(&msg);
        assert!(matches!(result, Cow::Borrowed(_)));
    }

    #[test]
    fn truncate_2001_chars() {
        let msg: String = "a".repeat(2001);
        let result = truncate_for_discord(&msg);
        assert!(result.ends_with(TRUNCATION_SUFFIX));
        assert!(result.chars().count() <= DISCORD_MAX_CHARS);
    }

    #[test]
    fn truncate_suffix_length_matters() {
        let msg = "word ".repeat(500);
        let result = truncate_for_discord(&msg);
        assert!(result.chars().count() <= DISCORD_MAX_CHARS);
        assert!(result.ends_with(TRUNCATION_SUFFIX));
        let body_chars = result.chars().count() - TRUNCATION_SUFFIX.chars().count();
        assert!(body_chars > 1900);
    }

    #[test]
    fn truncate_count_comparison() {
        let exact = "a".repeat(DISCORD_MAX_CHARS);
        let result = truncate_for_discord(&exact);
        assert!(matches!(result, Cow::Borrowed(_)));

        let over = "a".repeat(DISCORD_MAX_CHARS + 1);
        let result = truncate_for_discord(&over);
        assert!(result.ends_with(TRUNCATION_SUFFIX));
        assert!(result.chars().count() <= DISCORD_MAX_CHARS);
    }

    #[test]
    fn truncate_byte_pos_advances_correctly() {
        let msg: String = "é".repeat(2100);
        let result = truncate_for_discord(&msg);
        assert!(result.chars().count() <= DISCORD_MAX_CHARS);
        assert!(result.ends_with(TRUNCATION_SUFFIX));
        let body_len = result.chars().count() - TRUNCATION_SUFFIX.chars().count();
        assert!(body_len > 1900, "body too short: {body_len}");
    }

    // -- Full pipeline -------------------------------------------------------

    #[test]
    fn irc_to_discord_webhook_pipeline() {
        let (username, body) =
            irc_to_discord_webhook("alice", "\x02hello\x02 @alice", &StubIrcResolver);
        assert_eq!(username, "a\u{200B}lice");
        assert_eq!(body, "**hello** <@111>");
    }

    #[test]
    fn irc_to_discord_plain_pipeline() {
        let result = irc_to_discord_plain("bob", "hello", &StubIrcResolver);
        assert_eq!(result, "**[b\u{200B}ob]** hello");
    }

    // -- Proptest ------------------------------------------------------------

    use proptest::prelude::*;

    /// Strategy that generates strings rich in IRC control codes.
    fn irc_control_strategy() -> impl Strategy<Value = String> {
        let atoms = prop::sample::select(vec![
            "\x02",
            "\x1d",
            "\x1f",
            "\x1e",
            "\x16",
            "\x03",
            "\x0f",
            "\x034",
            "\x034,5",
            "\x0312,13",
            "hello",
            "world",
            "@nick",
            " ",
            "\x7f",
            "\x01",
        ]);
        prop::collection::vec(atoms, 0..20).prop_map(|parts| parts.join(""))
    }

    proptest! {
        /// Arbitrary Unicode text without `@` must pass through `convert_irc_mentions`
        /// completely unchanged.
        #[test]
        fn irc_mentions_no_at_sign_is_identity(text in "[^@]{0,200}") {
            let result = convert_irc_mentions(&text, &MatchAllIrcResolver);
            prop_assert_eq!(
                &result, &text,
                "text without @ must survive unchanged"
            );
        }

        /// Text with frequent `@` signs must never corrupt surrounding Unicode.
        #[test]
        fn irc_mentions_at_heavy_unicode_never_corrupts(
            parts in proptest::collection::vec(
                proptest::prop_oneof![
                    3 => Just("@".to_string()),
                    2 => "[a-zA-Z_]{1,8}".prop_map(|s| format!("@{s}")),
                    3 => "\\PC{1,20}",  // arbitrary non-control Unicode
                ],
                1..=10,
            )
        ) {
            let text = parts.join("");
            let result = convert_irc_mentions(&text, &StubIrcResolver);
            for ch in text.chars() {
                if !ch.is_ascii_alphanumeric() && ch != '@' {
                    prop_assert!(
                        result.contains(ch),
                        "character {ch:?} (U+{:04X}) was lost from output.\n  input:  {text:?}\n  output: {result:?}",
                        ch as u32
                    );
                }
            }
        }

        #[test]
        fn irc_control_roundtrip_never_panics(text in irc_control_strategy()) {
            let result = irc_to_discord_formatting(&text);
            assert!(!result.chars().any(|c|
                matches!(c, '\x02' | '\x1d' | '\x1f' | '\x1e' | '\x16' | '\x03' | '\x0f')
            ));
            assert!(!result.chars().any(|c| c.is_control()));
        }

        /// `irc_to_discord_formatting` on full Unicode input: control chars
        /// must be stripped, all other text must survive intact.
        #[test]
        fn irc_to_discord_preserves_unicode_text(text in "\\PC{0,200}") {
            let result = irc_to_discord_formatting(&text);
            prop_assert_eq!(
                &result, &text,
                "text without control characters must pass through unchanged"
            );
        }

        /// `irc_to_discord_formatting` must strip all control characters and
        /// preserve all non-control, non-digit-after-color characters.
        #[test]
        fn irc_to_discord_strips_controls_keeps_text(
            parts in proptest::collection::vec(
                proptest::prop_oneof![
                    3 => "[a-zA-Z\u{00C0}-\u{024F}\u{4E00}-\u{4E10} ,.!?]{1,20}",
                    2 => proptest::strategy::Just("\x02".to_string()),
                    1 => proptest::strategy::Just("\x1d".to_string()),
                    1 => proptest::strategy::Just("\x1f".to_string()),
                    1 => proptest::strategy::Just("\x03 ".to_string()),
                    1 => proptest::strategy::Just("\x0f".to_string()),
                ],
                1..=10,
            )
        ) {
            let text = parts.join("");
            let result = irc_to_discord_formatting(&text);
            prop_assert!(
                !result.chars().any(|c| c.is_control()),
                "control characters must be stripped.\n  input:  {text:?}\n  output: {result:?}"
            );
            for ch in text.chars() {
                if !ch.is_control() {
                    prop_assert!(
                        result.contains(ch),
                        "non-control char {ch:?} was lost.\n  input:  {text:?}\n  output: {result:?}"
                    );
                }
            }
        }

        #[test]
        fn irc_to_discord_never_panics(text in "[\x00-\x7f]{0,200}") {
            let _ = irc_to_discord_formatting(&text);
        }

        #[test]
        fn truncate_respects_limit(text in ".{0,5000}") {
            let result = truncate_for_discord(&text);
            assert!(result.chars().count() <= DISCORD_MAX_CHARS);
        }

        #[test]
        fn ping_fix_preserves_content(nick in "[a-zA-Z0-9_]{1,30}") {
            let fixed = ping_fix_nick(&nick);
            let without_zwsp: String = fixed.replace('\u{200B}', "");
            assert_eq!(without_zwsp, nick);
        }
    }
}
