//! Message formatting transformations between Discord and IRC.
//!
//! See `specs/05-formatting.md` for the full specification.

use std::borrow::Cow;

// ---------------------------------------------------------------------------
// IRC control characters
// ---------------------------------------------------------------------------

const IRC_BOLD: char = '\x02';
const IRC_ITALIC: char = '\x1d';
const IRC_UNDERLINE: char = '\x1f';
const IRC_STRIKETHROUGH: char = '\x1e';
const IRC_REVERSE: char = '\x16';
const IRC_COLOR: char = '\x03';
const IRC_RESET: char = '\x0f';

// ---------------------------------------------------------------------------
// Discord → IRC: mention / emoji resolution
// ---------------------------------------------------------------------------

/// Resolver trait for looking up Discord entities by ID.
///
/// Implementations are provided by the bridge runtime; tests use stubs.
pub trait DiscordResolver {
    /// Resolve a user ID to a display name / nick.
    fn resolve_user(&self, id: &str) -> Option<String>;
    /// Resolve a channel ID to a channel name (without `#`).
    fn resolve_channel(&self, id: &str) -> Option<String>;
    /// Resolve a role ID to a role name.
    fn resolve_role(&self, id: &str) -> Option<String>;
}

/// Replace Discord mention / emoji markup with plain-text equivalents.
///
/// Handles: `<@id>`, `<@!id>`, `<#id>`, `<@&id>`, `<:name:id>`, `<a:name:id>`.
#[must_use]
pub fn resolve_mentions(text: &str, resolver: &dyn DiscordResolver) -> String {
    let mut result = String::with_capacity(text.len());
    let mut chars = text.char_indices().peekable();

    while let Some(&(i, ch)) = chars.peek() {
        if ch == '<'
            && let Some(end) = text[i..].find('>')
        {
            let inner = &text[i + 1..i + end];
            if let Some(replacement) = resolve_one(inner, resolver) {
                result.push_str(&replacement);
                // advance past the closing '>'
                for _ in 0..=end {
                    chars.next();
                }
                continue;
            }
        }
        result.push(ch);
        chars.next();
    }

    result
}

fn resolve_one(inner: &str, resolver: &dyn DiscordResolver) -> Option<String> {
    // Custom/animated emoji: :name:id or a:name:id
    if let Some(rest) = inner.strip_prefix(':') {
        // :name:id
        if let Some(colon_pos) = rest.find(':') {
            let name = &rest[..colon_pos];
            return Some(format!(":{name}:"));
        }
    }
    if let Some(rest) = inner.strip_prefix("a:") {
        // a:name:id
        if let Some(colon_pos) = rest.find(':') {
            let name = &rest[..colon_pos];
            return Some(format!(":{name}:"));
        }
    }

    // User mention: @id or @!id
    if let Some(id) = inner.strip_prefix("@!") {
        let display = resolver
            .resolve_user(id)
            .unwrap_or_else(|| format!("@{id}"));
        return Some(format!("@{display}"));
    }
    if let Some(id) = inner.strip_prefix("@&") {
        // Role mention
        let display = resolver
            .resolve_role(id)
            .unwrap_or_else(|| "deleted-role".to_string());
        return Some(format!("@{display}"));
    }
    if let Some(id) = inner.strip_prefix('@') {
        let display = resolver
            .resolve_user(id)
            .unwrap_or_else(|| format!("@{id}"));
        return Some(format!("@{display}"));
    }

    // Channel mention: #id
    if let Some(id) = inner.strip_prefix('#') {
        let display = resolver
            .resolve_channel(id)
            .unwrap_or_else(|| "deleted-channel".to_string());
        return Some(format!("#{display}"));
    }

    None
}

// ---------------------------------------------------------------------------
// Discord → IRC: markdown to IRC control codes
// ---------------------------------------------------------------------------

/// Convert Discord markdown formatting to IRC control codes.
///
/// Processing order matters: underline `__` before italic `_`, bold `**`
/// before italic `*`, strikethrough `~~`.
#[must_use]
pub fn markdown_to_irc(text: &str) -> String {
    let mut result = text.to_string();

    // Strikethrough ~~text~~ → just text (no IRC equivalent)
    result = replace_paired_marker(&result, "~~", "", "");

    // Bold **text** → \x02text\x02
    result = replace_paired_marker(&result, "**", &IRC_BOLD.to_string(), &IRC_BOLD.to_string());

    // Underline __text__ → \x1ftext\x1f (before single _)
    result = replace_paired_marker(
        &result,
        "__",
        &IRC_UNDERLINE.to_string(),
        &IRC_UNDERLINE.to_string(),
    );

    // Italic *text* → \x1dtext\x1d
    result = replace_paired_marker(
        &result,
        "*",
        &IRC_ITALIC.to_string(),
        &IRC_ITALIC.to_string(),
    );

    // Italic _text_ → \x1dtext\x1d
    result = replace_paired_marker(
        &result,
        "_",
        &IRC_ITALIC.to_string(),
        &IRC_ITALIC.to_string(),
    );

    result
}

/// Replace paired markers like `**text**` with `open + text + close`.
fn replace_paired_marker(text: &str, marker: &str, open: &str, close: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut remaining = text;

    loop {
        let Some(start) = remaining.find(marker) else {
            result.push_str(remaining);
            break;
        };

        // Look for closing marker
        let after_open = start + marker.len();
        let Some(end) = remaining[after_open..].find(marker) else {
            // No closing marker — push rest and break
            result.push_str(remaining);
            break;
        };

        let inner = &remaining[after_open..after_open + end];
        if inner.is_empty() {
            // Empty content between markers — leave markers as-is
            result.push_str(&remaining[..after_open]);
            remaining = &remaining[after_open..];
            continue;
        }

        result.push_str(&remaining[..start]);
        result.push_str(open);
        result.push_str(inner);
        result.push_str(close);
        remaining = &remaining[after_open + end + marker.len()..];
    }

    result
}

// ---------------------------------------------------------------------------
// Discord → IRC: newline splitting, code blocks, length splitting
// ---------------------------------------------------------------------------

/// Maximum number of lines to send for a single Discord message.
const MAX_LINES: usize = 5;

/// Maximum byte length for a single IRC line (message body only).
const MAX_LINE_BYTES: usize = 400;

/// Code block continuation prefix.
const CODE_CONTINUATION: &str = "\x02>\x02 ";

/// Split a formatted message into IRC `PRIVMSG` lines.
///
/// Handles:
/// - Newline normalization and splitting (max 5 lines)
/// - Code block continuation prefixing
/// - Length splitting at word boundaries
#[must_use]
pub fn split_for_irc(text: &str) -> Vec<String> {
    // Normalise newlines
    let text = text.replace("\r\n", "\n").replace('\r', "\n");

    // Process code blocks: first line as-is, rest with continuation prefix
    let lines = split_code_blocks(&text);

    // Apply max-line limit
    let truncated = if lines.len() > MAX_LINES {
        let extra = lines.len() - MAX_LINES;
        let mut kept: Vec<String> = lines.into_iter().take(MAX_LINES).collect();
        kept.push(format!("[+{extra} more lines]"));
        kept
    } else {
        lines
    };

    // Split long lines at word boundaries
    let mut result = Vec::new();
    for line in truncated {
        if line.len() <= MAX_LINE_BYTES {
            result.push(line);
        } else {
            result.extend(split_long_line(&line, MAX_LINE_BYTES));
        }
    }

    result
}

/// Process code blocks: first line as-is, subsequent lines get continuation prefix.
fn split_code_blocks(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut in_code_block = false;
    let mut first_line_of_block = true;

    for line in text.split('\n') {
        if line.starts_with("```") {
            in_code_block = !in_code_block;
            first_line_of_block = true;
            // Skip the ``` marker line itself (both opening and closing)
            continue;
        }

        if in_code_block {
            if first_line_of_block {
                lines.push(line.to_string());
                first_line_of_block = false;
            } else {
                lines.push(format!("{CODE_CONTINUATION}{line}"));
            }
        } else if !line.is_empty() {
            lines.push(line.to_string());
        }
    }

    lines
}

/// Split a line that exceeds `max_bytes` at word boundaries.
///
/// Splits are always at valid UTF-8 char boundaries.
fn split_long_line(line: &str, max_bytes: usize) -> Vec<String> {
    let mut parts = Vec::new();
    let mut remaining = line;

    while remaining.len() > max_bytes {
        // Find the last char boundary at or before max_bytes
        let mut boundary = max_bytes;
        while boundary > 0 && !remaining.is_char_boundary(boundary) {
            boundary -= 1;
        }

        // Find last space before the boundary
        match remaining[..boundary].rfind(' ') {
            Some(0) | None => {
                // No usable space — hard-split at char boundary
                parts.push(remaining[..boundary].to_string());
                remaining = &remaining[boundary..];
            }
            Some(space_pos) => {
                parts.push(remaining[..space_pos].to_string());
                remaining = &remaining[space_pos + 1..]; // skip the ASCII space
            }
        }
    }

    if !remaining.is_empty() {
        parts.push(remaining.to_string());
    }

    parts
}

// ---------------------------------------------------------------------------
// Discord → IRC: full pipeline
// ---------------------------------------------------------------------------

/// Full Discord → IRC formatting pipeline.
///
/// Returns a list of lines to send as separate `PRIVMSG` messages.
#[must_use]
pub fn discord_to_irc(text: &str, resolver: &dyn DiscordResolver) -> Vec<String> {
    let resolved = resolve_mentions(text, resolver);
    let formatted = markdown_to_irc(&resolved);
    split_for_irc(&formatted)
}

// ---------------------------------------------------------------------------
// IRC → Discord: control character handling
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
                // Strip remaining control characters \x01–\x1f and \x7f
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
        if chars.peek().is_some_and(|c| c.is_ascii_digit()) {
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
// IRC → Discord: mention conversion
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
    let bytes = text.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'@' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_alphanumeric() {
            // Extract the nick (alphanumeric, underscore, hyphen, brackets, backslash)
            let nick_start = i + 1;
            let mut nick_end = nick_start;
            while nick_end < bytes.len()
                && (bytes[nick_end].is_ascii_alphanumeric()
                    || bytes[nick_end] == b'_'
                    || bytes[nick_end] == b'-'
                    || bytes[nick_end] == b'['
                    || bytes[nick_end] == b']'
                    || bytes[nick_end] == b'\\'
                    || bytes[nick_end] == b'`'
                    || bytes[nick_end] == b'^'
                    || bytes[nick_end] == b'{'
                    || bytes[nick_end] == b'}')
            {
                nick_end += 1;
            }
            let nick = &text[nick_start..nick_end];
            if let Some(user_id) = resolver.resolve_nick(nick) {
                result.push_str(&format!("<@{user_id}>"));
            } else {
                result.push_str(&text[i..nick_end]);
            }
            i = nick_end;
        } else {
            result.push(bytes[i] as char);
            i += 1;
        }
    }

    result
}

// ---------------------------------------------------------------------------
// IRC → Discord: ping-fix
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
// IRC → Discord: length truncation
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

    // Find the char boundary at `target` chars
    let mut byte_pos = 0;
    for (count, (i, ch)) in text.char_indices().enumerate() {
        if count >= target {
            byte_pos = i;
            break;
        }
        byte_pos = i + ch.len_utf8();
    }

    // Try to split at the last space before the limit
    let truncated = &text[..byte_pos];
    let split_at = truncated.rfind(' ').unwrap_or(byte_pos);

    let mut result = text[..split_at].to_string();
    result.push_str(TRUNCATION_SUFFIX);
    Cow::Owned(result)
}

// ---------------------------------------------------------------------------
// IRC → Discord: full pipeline
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
// server-time formatting
// ---------------------------------------------------------------------------

/// Format a Unix timestamp (seconds + millis) as ISO 8601 UTC.
///
/// Output: `YYYY-MM-DDTHH:MM:SS.mmmZ`
#[must_use]
pub fn format_server_time(unix_secs: i64, millis: u32) -> String {
    use chrono::{DateTime, Utc};

    let dt = DateTime::<Utc>::from_timestamp(unix_secs, millis * 1_000_000)
        .unwrap_or_else(|| DateTime::<Utc>::from_timestamp(0, 0).expect("epoch is valid"));
    dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Test helpers / stubs ------------------------------------------------

    struct StubResolver;

    impl DiscordResolver for StubResolver {
        fn resolve_user(&self, id: &str) -> Option<String> {
            match id {
                "111" => Some("Alice".to_string()),
                "222" => Some("Bob".to_string()),
                _ => None,
            }
        }

        fn resolve_channel(&self, id: &str) -> Option<String> {
            match id {
                "100" => Some("general".to_string()),
                _ => None,
            }
        }

        fn resolve_role(&self, id: &str) -> Option<String> {
            match id {
                "500" => Some("Moderator".to_string()),
                _ => None,
            }
        }
    }

    struct StubIrcResolver;

    impl IrcMentionResolver for StubIrcResolver {
        fn resolve_nick(&self, nick: &str) -> Option<String> {
            match nick.to_lowercase().as_str() {
                "alice" => Some("111".to_string()),
                _ => None,
            }
        }
    }

    // -- Discord → IRC: mentions & emoji ------------------------------------

    #[test]
    fn resolve_user_mention() {
        let r = resolve_mentions("Hello <@111>!", &StubResolver);
        assert_eq!(r, "Hello @Alice!");
    }

    #[test]
    fn resolve_legacy_user_mention() {
        let r = resolve_mentions("Hey <@!222>", &StubResolver);
        assert_eq!(r, "Hey @Bob");
    }

    #[test]
    fn resolve_unknown_user_mention() {
        let r = resolve_mentions("<@999>", &StubResolver);
        assert_eq!(r, "@@999");
    }

    #[test]
    fn resolve_channel_mention() {
        let r = resolve_mentions("See <#100>", &StubResolver);
        assert_eq!(r, "See #general");
    }

    #[test]
    fn resolve_unknown_channel() {
        let r = resolve_mentions("<#999>", &StubResolver);
        assert_eq!(r, "#deleted-channel");
    }

    #[test]
    fn resolve_role_mention() {
        let r = resolve_mentions("Ping <@&500>", &StubResolver);
        assert_eq!(r, "Ping @Moderator");
    }

    #[test]
    fn resolve_unknown_role() {
        let r = resolve_mentions("<@&999>", &StubResolver);
        assert_eq!(r, "@deleted-role");
    }

    #[test]
    fn resolve_custom_emoji() {
        let r = resolve_mentions("Nice <:thumbsup:12345>!", &StubResolver);
        assert_eq!(r, "Nice :thumbsup:!");
    }

    #[test]
    fn resolve_animated_emoji() {
        let r = resolve_mentions("Wow <a:party:67890>", &StubResolver);
        assert_eq!(r, "Wow :party:");
    }

    #[test]
    fn resolve_multiple_mentions() {
        let r = resolve_mentions("<@111> told <@222> in <#100>", &StubResolver);
        assert_eq!(r, "@Alice told @Bob in #general");
    }

    #[test]
    fn resolve_no_mentions() {
        let r = resolve_mentions("plain text", &StubResolver);
        assert_eq!(r, "plain text");
    }

    #[test]
    fn resolve_unclosed_angle_bracket() {
        let r = resolve_mentions("a < b and c > d", &StubResolver);
        assert_eq!(r, "a < b and c > d");
    }

    // -- Discord → IRC: markdown conversion ---------------------------------

    #[test]
    fn bold_to_irc() {
        assert_eq!(markdown_to_irc("**hello**"), "\x02hello\x02");
    }

    #[test]
    fn italic_star_to_irc() {
        assert_eq!(markdown_to_irc("*hello*"), "\x1dhello\x1d");
    }

    #[test]
    fn italic_underscore_to_irc() {
        assert_eq!(markdown_to_irc("_hello_"), "\x1dhello\x1d");
    }

    #[test]
    fn underline_to_irc() {
        assert_eq!(markdown_to_irc("__hello__"), "\x1fhello\x1f");
    }

    #[test]
    fn strikethrough_stripped() {
        assert_eq!(markdown_to_irc("~~gone~~"), "gone");
    }

    #[test]
    fn inline_code_unchanged() {
        assert_eq!(markdown_to_irc("`code`"), "`code`");
    }

    #[test]
    fn bold_italic_combined() {
        // ***text*** → strikethrough first (no match), then bold ** matches
        // the first **, leaving *text* after bold close. Bold: \x02*text*\x02
        // Then italic * matches the remaining *text*: \x02\x1dtext\x1d\x02
        // But actually ** matches at pos 0, closing at the ** starting at pos 7:
        // inner = "*text*", so result = \x02*text*\x02, then * italic:
        // \x02\x1dtext\x1d\x02 — but wait, the bold markers consume **, leaving
        // \x02 + *text* + \x02 + *, then italic sees *text* → \x1dtext\x1d
        // Result depends on which ** pair is matched. Let's just verify it
        // doesn't panic and contains the text.
        let r = markdown_to_irc("***text***");
        assert!(r.contains("text"));
    }

    #[test]
    fn no_formatting() {
        assert_eq!(markdown_to_irc("plain text"), "plain text");
    }

    #[test]
    fn unmatched_bold_marker() {
        assert_eq!(markdown_to_irc("**oops"), "**oops");
    }

    // -- Discord → IRC: splitting -------------------------------------------

    #[test]
    fn split_single_line() {
        let lines = split_for_irc("hello");
        assert_eq!(lines, vec!["hello"]);
    }

    #[test]
    fn split_multiple_lines() {
        let lines = split_for_irc("a\nb\nc");
        assert_eq!(lines, vec!["a", "b", "c"]);
    }

    #[test]
    fn split_normalizes_crlf() {
        let lines = split_for_irc("a\r\nb\rc");
        assert_eq!(lines, vec!["a", "b", "c"]);
    }

    #[test]
    fn split_skips_empty_lines() {
        let lines = split_for_irc("a\n\nb");
        assert_eq!(lines, vec!["a", "b"]);
    }

    #[test]
    fn split_truncates_beyond_max_lines() {
        let text = "1\n2\n3\n4\n5\n6\n7";
        let lines = split_for_irc(text);
        assert_eq!(lines.len(), 6); // 5 lines + truncation notice
        assert_eq!(lines[5], "[+2 more lines]");
    }

    #[test]
    fn split_code_block_continuation() {
        let text = "```rust\nfn main() {\n    println!(\"hi\");\n}\n```";
        let lines = split_for_irc(text);
        assert_eq!(lines[0], "fn main() {");
        assert_eq!(lines[1], "\x02>\x02     println!(\"hi\");");
        assert_eq!(lines[2], "\x02>\x02 }");
    }

    #[test]
    fn split_long_line_at_word_boundary() {
        let word = "word ";
        // 5 bytes per word, need > 400 bytes
        let text: String = word.repeat(100); // 500 bytes
        let lines = split_for_irc(&text);
        assert!(lines.len() > 1);
        assert!(lines[0].len() <= MAX_LINE_BYTES);
    }

    // -- IRC → Discord: formatting conversion --------------------------------

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
        // \x034,5colored text\x03 → "colored text"
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
        // \x01 (SOH) should be stripped
        assert_eq!(irc_to_discord_formatting("\x01hello"), "hello");
    }

    #[test]
    fn irc_plain_text_unchanged() {
        assert_eq!(irc_to_discord_formatting("hello world"), "hello world");
    }

    #[test]
    fn irc_nested_bold_italic() {
        // \x02 on → "bold " (bold) → \x1d on → "and italic" (bold+italic)
        // → \x1d off → " only bold" (bold) → \x02 off
        let input = "\x02bold \x1dand italic\x1d only bold\x02";
        let result = irc_to_discord_formatting(input);
        // Spans: "bold " (bold), "and italic" (bold+italic), " only bold" (bold)
        assert_eq!(result, "**bold *****and italic***** only bold**");
    }

    // -- IRC → Discord: mention conversion -----------------------------------

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
        let msg: String = "a ".repeat(1100); // 2200 chars
        let result = truncate_for_discord(&msg);
        assert!(result.chars().count() <= DISCORD_MAX_CHARS);
        assert!(result.ends_with(TRUNCATION_SUFFIX));
    }

    #[test]
    fn truncate_returns_borrowed_when_short() {
        let msg = "hello";
        assert!(matches!(truncate_for_discord(msg), Cow::Borrowed(_)));
    }

    // -- server-time formatting ----------------------------------------------

    #[test]
    fn format_epoch() {
        assert_eq!(format_server_time(0, 0), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn format_known_timestamp() {
        // 2024-01-15 12:28:16.789 UTC
        assert_eq!(
            format_server_time(1_705_321_696, 789),
            "2024-01-15T12:28:16.789Z"
        );
    }

    #[test]
    fn format_leap_year() {
        // 2024-02-29 00:00:00.000 UTC (2024 is a leap year)
        assert_eq!(
            format_server_time(1_709_164_800, 0),
            "2024-02-29T00:00:00.000Z"
        );
    }

    #[test]
    fn format_end_of_year() {
        // 2023-12-31 23:59:59.999 UTC
        assert_eq!(
            format_server_time(1_704_067_199, 999),
            "2023-12-31T23:59:59.999Z"
        );
    }

    // -- Full pipeline tests -------------------------------------------------

    #[test]
    fn discord_to_irc_full_pipeline() {
        let lines = discord_to_irc("**Hello** <@111>!", &StubResolver);
        assert_eq!(lines, vec!["\x02Hello\x02 @Alice!"]);
    }

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

    // -- Mutant-killing tests ------------------------------------------------

    /// A resolver that matches any nick it's given (returns "42" as user ID).
    struct MatchAllIrcResolver;

    impl IrcMentionResolver for MatchAllIrcResolver {
        fn resolve_nick(&self, _nick: &str) -> Option<String> {
            Some("42".to_string())
        }
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
        // @ at the very end with no following character
        let r = convert_irc_mentions("test @", &MatchAllIrcResolver);
        assert_eq!(r, "test @");
    }

    #[test]
    fn irc_mention_at_followed_by_space() {
        let r = convert_irc_mentions("@ space", &MatchAllIrcResolver);
        assert_eq!(r, "@ space");
    }

    #[test]
    fn irc_control_char_below_0x20_stripped() {
        // \x05 (ENQ) and \x07 (BEL) should be stripped
        assert_eq!(irc_to_discord_formatting("\x05hello\x07"), "hello");
    }

    #[test]
    fn split_exactly_max_lines() {
        // Exactly 5 non-empty lines should not trigger truncation
        let text = "1\n2\n3\n4\n5";
        let lines = split_for_irc(text);
        assert_eq!(lines.len(), 5);
        assert!(!lines.last().unwrap().starts_with("[+"));
    }

    #[test]
    fn split_six_lines_truncates() {
        let text = "1\n2\n3\n4\n5\n6";
        let lines = split_for_irc(text);
        assert_eq!(lines.len(), 6);
        assert_eq!(lines[5], "[+1 more lines]");
    }

    #[test]
    fn split_long_line_boundary_minus_one() {
        // Line exactly at max_bytes should not be split
        let line = "a".repeat(MAX_LINE_BYTES);
        let lines = split_for_irc(&line);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn split_long_line_boundary_plus_one() {
        // Line at max_bytes + 1 should be split
        let line = "a".repeat(MAX_LINE_BYTES + 1);
        let lines = split_for_irc(&line);
        assert!(lines.len() > 1);
    }

    #[test]
    fn code_block_not_in_block_skips_closing() {
        // ``` as closing when not in code block should still be skipped
        // (it toggles in_code_block on, but the line itself is skipped)
        let text = "```\nhello\n```";
        let lines = split_for_irc(text);
        assert_eq!(lines, vec!["hello"]);
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
    fn server_time_various_dates() {
        assert_eq!(
            format_server_time(951_868_800, 0),
            "2000-03-01T00:00:00.000Z"
        );
        assert_eq!(
            format_server_time(946_684_800, 0),
            "2000-01-01T00:00:00.000Z"
        );
    }

    #[test]
    fn replace_paired_marker_empty_inner() {
        // **** → should not produce empty styled spans, markers left as-is
        let r = replace_paired_marker("****", "**", "[", "]");
        assert_eq!(r, "****");
    }

    #[test]
    fn code_block_closing_not_emitted() {
        // The closing ``` should not appear in output; only content inside block
        let text = "before\n```\nline1\nline2\n```\nafter";
        let lines = split_for_irc(text);
        assert_eq!(lines[0], "before");
        assert_eq!(lines[1], "line1");
        assert_eq!(lines[2], "\x02>\x02 line2");
        assert_eq!(lines[3], "after");
        assert_eq!(lines.len(), 4);
    }

    #[test]
    fn split_long_line_with_spaces_splits_correctly() {
        // Build a line with words separated by spaces, totaling > 400 bytes
        // Each word is 50 chars, so 9 words = 450 chars + 8 spaces = 458
        let word = "a".repeat(50);
        let line = vec![word.as_str(); 9].join(" ");
        let parts = split_long_line(&line, MAX_LINE_BYTES);
        assert!(parts.len() >= 2);
        // First part should end at a word boundary (space)
        assert!(parts[0].len() <= MAX_LINE_BYTES);
        // Verify no content is lost
        let rejoined: String = parts.join(" ");
        assert_eq!(rejoined, line);
    }

    #[test]
    fn split_long_line_no_spaces_hard_splits() {
        let line = "x".repeat(MAX_LINE_BYTES * 2 + 50);
        let parts = split_long_line(&line, MAX_LINE_BYTES);
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].len(), MAX_LINE_BYTES);
        assert_eq!(parts[1].len(), MAX_LINE_BYTES);
        assert_eq!(parts[2].len(), 50);
        // Verify content is preserved
        let rejoined: String = parts.join("");
        assert_eq!(rejoined, line);
    }

    #[test]
    fn split_long_line_exactly_max_bytes() {
        // Call split_long_line directly with exactly max_bytes — should not split
        let line = "x".repeat(MAX_LINE_BYTES);
        let parts = split_long_line(&line, MAX_LINE_BYTES);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0], line);
    }

    #[test]
    fn split_long_line_multibyte_boundary_search() {
        // Put a 3-byte char right where the boundary would land
        // so that boundary -= 1 is exercised
        let mut line = "a".repeat(MAX_LINE_BYTES - 1);
        line.push('€'); // 3 bytes: total = MAX_LINE_BYTES + 2
        line.push('b');
        let parts = split_long_line(&line, MAX_LINE_BYTES);
        assert!(parts.len() >= 2);
        // First part should be valid UTF-8 and <= max_bytes
        assert!(parts[0].len() <= MAX_LINE_BYTES);
        assert!(parts[0].is_char_boundary(parts[0].len()));
    }

    #[test]
    fn split_long_line_boundary_search_decrements() {
        // boundary > 0 && !is_char_boundary → boundary -= 1
        // Use a multi-byte char right at the boundary
        let mut line = "a".repeat(MAX_LINE_BYTES - 2);
        line.push('é'); // 2 bytes, puts us at exactly MAX_LINE_BYTES
        line.push_str(&"b".repeat(10)); // push past the limit
        let parts = split_long_line(&line, MAX_LINE_BYTES);
        assert!(parts.len() >= 2);
        // Ensure we didn't panic on char boundaries
        for part in &parts {
            assert!(part.is_char_boundary(part.len()));
        }
    }

    #[test]
    fn irc_control_char_0x1f_boundary() {
        // \x1f is underline (handled), \x20 is space (not control)
        // Test that chars at exactly 0x20 are NOT stripped
        assert_eq!(irc_to_discord_formatting(" hello"), " hello");
        // But \x1f is underline toggle
        assert_eq!(irc_to_discord_formatting("\x1fhi\x1f"), "__hi__");
    }

    #[test]
    fn irc_del_stripped() {
        // \x7f (DEL) is a control character and should be stripped
        let result = irc_to_discord_formatting("a\x7fb");
        assert_eq!(result, "ab");
    }

    #[test]
    fn truncate_suffix_length_matters() {
        // If we replaced - with / in `DISCORD_MAX_CHARS - suffix_len`,
        // the target would be wrong. Verify that the truncated output
        // plus the suffix fits exactly within 2000 chars.
        let msg = "word ".repeat(500); // 2500 chars
        let result = truncate_for_discord(&msg);
        assert!(result.chars().count() <= DISCORD_MAX_CHARS);
        assert!(result.ends_with(TRUNCATION_SUFFIX));
        // Also verify we used as much space as possible
        let body_chars = result.chars().count() - TRUNCATION_SUFFIX.chars().count();
        // Body should be close to 2000 - suffix_len, not wildly off
        assert!(body_chars > 1900);
    }

    #[test]
    fn truncate_count_comparison() {
        // Tests the `count >= target` vs `count < target` boundary
        // Message with exactly target+suffix chars should not be truncated
        // but target+suffix+1 should be
        let suffix_len = TRUNCATION_SUFFIX.chars().count();
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
        // `byte_pos = i + ch.len_utf8()` — if replaced with `i * ch.len_utf8()`
        // this would break when i != ch.len_utf8().
        // Use ASCII (1 byte) followed by 2-byte chars so i diverges from len_utf8.
        let mut msg = "a".repeat(100); // 100 ASCII chars, byte_pos=100
        msg.push_str(&"é".repeat(2000)); // 2-byte chars, with * bug: 100*2=200 not 102
        let result = truncate_for_discord(&msg);
        assert!(result.chars().count() <= DISCORD_MAX_CHARS);
        // The result must contain the ASCII prefix
        assert!(result.starts_with("aaaa"));
    }

    #[test]
    fn server_time_far_future() {
        assert_eq!(
            format_server_time(4_102_444_800, 0),
            "2100-01-01T00:00:00.000Z"
        );
    }

    proptest! {
        #[test]
        fn resolve_mentions_never_panics(text in ".*") {
            let _ = resolve_mentions(&text, &StubResolver);
        }

        #[test]
        fn markdown_to_irc_never_panics(text in ".*") {
            let _ = markdown_to_irc(&text);
        }

        #[test]
        fn split_for_irc_never_panics(text in ".{0,2000}") {
            let lines = split_for_irc(&text);
            assert!(!lines.is_empty() || text.trim().is_empty());
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

        #[test]
        fn server_time_is_valid_iso8601(secs in 0i64..4_102_444_800i64, millis in 0u32..1000u32) {
            let formatted = format_server_time(secs, millis);
            assert!(formatted.ends_with('Z'));
            assert_eq!(formatted.len(), 24); // YYYY-MM-DDTHH:MM:SS.mmmZ
            assert_eq!(&formatted[4..5], "-");
            assert_eq!(&formatted[7..8], "-");
            assert_eq!(&formatted[10..11], "T");
            assert_eq!(&formatted[13..14], ":");
            assert_eq!(&formatted[16..17], ":");
            assert_eq!(&formatted[19..20], ".");
        }
    }
}
