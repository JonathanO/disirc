//! Discord → IRC formatting: mention resolution, markdown conversion, and line splitting.

use super::{IRC_BOLD, IRC_ITALIC, IRC_UNDERLINE};

// ---------------------------------------------------------------------------
// Mention / emoji resolution
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
// Markdown to IRC control codes
// ---------------------------------------------------------------------------

/// Characters that can be backslash-escaped in Discord markdown.
const ESCAPABLE: &[char] = &['*', '_', '~', '`', '>', '#', '-', '\\', '|'];

/// Sentinel range for escaped characters. Each escapable char gets its own
/// unique PUA codepoint so `str::find()` never confuses it with real markers.
/// `ESCAPABLE[i]` maps to `char::from_u32(ESCAPE_BASE + i)`.
const ESCAPE_BASE: u32 = 0xF_0000;

/// Convert Discord markdown formatting to IRC control codes.
///
/// Processing order matches Discord's simple-markdown parser:
/// 1. Backslash escapes
/// 2. Code blocks / inline code (protected from further formatting)
/// 3. Underline `__` (before single `_`)
/// 4. Bold `**` (before single `*`)
/// 5. Italic `*` and word-boundary `_`
/// 6. Strikethrough `~~` — passed through unchanged
#[must_use]
pub fn markdown_to_irc(text: &str) -> String {
    // Step 1: Replace backslash escapes with sentinels
    let mut result = replace_backslash_escapes(text);

    // Step 2: Protect code spans (they suppress all formatting)
    let (protected, code_spans) = protect_code_spans(&result);
    result = protected;

    // Step 3: Underline __text__ → \x1ftext\x1f (before single _)
    result = replace_paired_marker(
        &result,
        "__",
        &IRC_UNDERLINE.to_string(),
        &IRC_UNDERLINE.to_string(),
    );

    // Step 4: Bold **text** → \x02text\x02
    result = replace_paired_marker(&result, "**", &IRC_BOLD.to_string(), &IRC_BOLD.to_string());

    // Step 5a: Italic *text* → \x1dtext\x1d
    result = replace_paired_marker(
        &result,
        "*",
        &IRC_ITALIC.to_string(),
        &IRC_ITALIC.to_string(),
    );

    // Step 5b: Italic _text_ → \x1dtext\x1d (word boundary only)
    result = replace_word_boundary_underscores(&result);

    // Step 6: Strikethrough ~~text~~ → pass through unchanged

    // Restore code spans and escaped characters
    result = restore_code_spans(&result, &code_spans);
    result = restore_escaped_chars(&result);

    result
}

/// Map an escapable character to its unique PUA sentinel.
fn escape_to_sentinel(ch: char) -> Option<char> {
    ESCAPABLE
        .iter()
        .position(|&c| c == ch)
        .and_then(|i| char::from_u32(ESCAPE_BASE + i as u32))
}

/// Map a PUA sentinel back to the original character.
fn sentinel_to_char(ch: char) -> Option<char> {
    let code = ch as u32;
    if code >= ESCAPE_BASE && (code - ESCAPE_BASE) < ESCAPABLE.len() as u32 {
        Some(ESCAPABLE[(code - ESCAPE_BASE) as usize])
    } else {
        None
    }
}

/// Replace `\X` with a unique sentinel for each escapable character.
fn replace_backslash_escapes(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\\'
            && let Some(&next) = chars.peek()
            && let Some(sentinel) = escape_to_sentinel(next)
        {
            chars.next();
            result.push(sentinel);
            continue;
        }
        result.push(ch);
    }

    result
}

/// Restore sentinel characters to their literal form.
fn restore_escaped_chars(text: &str) -> String {
    text.chars()
        .map(|ch| sentinel_to_char(ch).unwrap_or(ch))
        .collect()
}

/// Sentinel marking the start and end of a code placeholder.
/// Uses a PUA codepoint well above the escape sentinel range.
const CODE_SENTINEL: char = '\u{F_0100}';

/// Extract code blocks and inline code, replacing them with placeholders.
///
/// Returns the modified text and a vec of the extracted code spans.
fn protect_code_spans(text: &str) -> (String, Vec<String>) {
    let mut result = String::with_capacity(text.len());
    let mut spans: Vec<String> = Vec::new();
    let mut remaining = text;

    loop {
        // Look for ``` (code block) or ` (inline code)
        let triple = remaining.find("```");
        let single = remaining.find('`');

        let next_code = match (triple, single) {
            (Some(t), Some(s)) if t <= s => Some((t, true)),
            (_, Some(s)) => Some((s, false)),
            (Some(t), None) => Some((t, true)),
            (None, None) => None,
        };

        let Some((pos, is_block)) = next_code else {
            result.push_str(remaining);
            break;
        };

        let delimiter = if is_block { "```" } else { "`" };
        let after_open = pos + delimiter.len();

        // Look for closing delimiter
        let Some(close) = remaining[after_open..].find(delimiter) else {
            result.push_str(remaining);
            break;
        };

        let full_span_end = after_open + close + delimiter.len();
        let span = &remaining[pos..full_span_end];

        result.push_str(&remaining[..pos]);
        result.push(CODE_SENTINEL);
        let idx = spans.len();
        result.push_str(&idx.to_string());
        result.push(CODE_SENTINEL);
        spans.push(span.to_string());

        remaining = &remaining[full_span_end..];
    }

    (result, spans)
}

/// Restore protected code spans from their placeholders.
fn restore_code_spans(text: &str, spans: &[String]) -> String {
    let mut result = text.to_string();
    for (i, span) in spans.iter().enumerate() {
        let placeholder = format!("{CODE_SENTINEL}{i}{CODE_SENTINEL}");
        result = result.replacen(&placeholder, span, 1);
    }
    result
}

/// Replace `_text_` with italic only when underscores are at word boundaries.
///
/// Discord does not treat intraword underscores as italic markers.
/// E.g. `some_variable_name` is NOT rendered as italic.
fn replace_word_boundary_underscores(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        if chars[i] == '_' {
            // Check if this _ is at a word boundary (start of string or preceded by whitespace)
            let at_word_start = i == 0 || chars[i - 1].is_whitespace();

            if at_word_start {
                // Look for closing _ at a word boundary
                if let Some(close) = find_word_boundary_close(&chars, i + 1) {
                    let inner: String = chars[i + 1..close].iter().collect();
                    if !inner.is_empty() {
                        result.push(IRC_ITALIC);
                        result.push_str(&inner);
                        result.push(IRC_ITALIC);
                        i = close + 1;
                        continue;
                    }
                }
            }
        }
        result.push(chars[i]);
        i += 1;
    }

    result
}

/// Find the position of a closing `_` that is at a word boundary
/// (followed by whitespace or end of string).
fn find_word_boundary_close(chars: &[char], start: usize) -> Option<usize> {
    for j in start..chars.len() {
        if chars[j] == '_' {
            let at_word_end = j + 1 >= chars.len() || chars[j + 1].is_whitespace();
            if at_word_end {
                return Some(j);
            }
        }
    }
    None
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
// Newline splitting, code blocks, length splitting
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
// Full pipeline
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

    // -- Mentions & emoji ----------------------------------------------------

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

    // -- Markdown conversion -------------------------------------------------

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
    fn strikethrough_preserved() {
        assert_eq!(markdown_to_irc("~~gone~~"), "~~gone~~");
    }

    #[test]
    fn inline_code_unchanged() {
        assert_eq!(markdown_to_irc("`code`"), "`code`");
    }

    #[test]
    fn bold_italic_combined() {
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

    #[test]
    fn unclosed_italic_star() {
        assert_eq!(markdown_to_irc("*oops"), "*oops");
    }

    #[test]
    fn unclosed_italic_underscore() {
        assert_eq!(markdown_to_irc("_oops"), "_oops");
    }

    #[test]
    fn unclosed_underline() {
        assert_eq!(markdown_to_irc("__oops"), "__oops");
    }

    #[test]
    fn unclosed_strikethrough() {
        assert_eq!(markdown_to_irc("~~oops"), "~~oops");
    }

    #[test]
    fn nested_bold_inside_italic() {
        let r = markdown_to_irc("_**bold inside italic**_");
        assert!(r.contains("bold inside italic"));
        assert!(r.contains('\x02'));
        assert!(r.contains('\x1d'));
    }

    #[test]
    fn overlapping_markers_dont_panic() {
        let r = markdown_to_irc("**bold *and italic** end*");
        assert!(r.contains("bold"));
        assert!(r.contains("end"));
    }

    #[test]
    fn deeply_nested_markers() {
        let r = markdown_to_irc("**__~~text~~__**");
        assert!(r.contains("text"));
        assert!(r.contains('\x02'));
        assert!(r.contains('\x1f'));
    }

    #[test]
    fn multiple_unclosed_markers() {
        let r = markdown_to_irc("**bold *italic __underline ~~strike");
        assert!(r.contains("bold"));
        assert!(r.contains("italic"));
    }

    #[test]
    fn empty_markers() {
        assert_eq!(markdown_to_irc("****"), "****");
        assert_eq!(markdown_to_irc("~~  ~~"), "~~  ~~");
    }

    // -- Backslash escapes ---------------------------------------------------

    #[test]
    fn backslash_escape_both_bold_markers() {
        assert_eq!(markdown_to_irc("\\*\\*not bold\\*\\*"), "**not bold**");
    }

    #[test]
    fn backslash_escape_italic_star() {
        assert_eq!(markdown_to_irc("\\*not italic\\*"), "*not italic*");
    }

    #[test]
    fn backslash_escape_underscore() {
        assert_eq!(markdown_to_irc("\\_not italic\\_"), "_not italic_");
    }

    #[test]
    fn backslash_escape_tilde_both() {
        assert_eq!(markdown_to_irc("\\~\\~not strike\\~\\~"), "~~not strike~~");
    }

    #[test]
    fn backslash_escape_backslash() {
        assert_eq!(markdown_to_irc("\\\\literal"), "\\literal");
    }

    #[test]
    fn backslash_before_non_escapable() {
        assert_eq!(markdown_to_irc("\\hello"), "\\hello");
    }

    #[test]
    fn sentinel_to_char_rejects_one_past_end() {
        let one_past = char::from_u32(ESCAPE_BASE + ESCAPABLE.len() as u32).unwrap();
        assert_eq!(sentinel_to_char(one_past), None);
    }

    #[test]
    fn sentinel_to_char_accepts_last_valid() {
        let last_valid = char::from_u32(ESCAPE_BASE + ESCAPABLE.len() as u32 - 1).unwrap();
        assert_eq!(
            sentinel_to_char(last_valid),
            Some(*ESCAPABLE.last().unwrap())
        );
    }

    // -- Intraword underscores -----------------------------------------------

    #[test]
    fn intraword_underscores_preserved() {
        assert_eq!(markdown_to_irc("some_variable_name"), "some_variable_name");
    }

    #[test]
    fn intraword_double_underscores_converted() {
        assert_eq!(markdown_to_irc("foo__init__bar"), "foo\x1finit\x1fbar");
    }

    #[test]
    fn word_boundary_underscore_italic() {
        assert_eq!(markdown_to_irc("_hello_"), "\x1dhello\x1d");
    }

    #[test]
    fn word_boundary_underscore_after_space() {
        assert_eq!(markdown_to_irc("hello _world_"), "hello \x1dworld\x1d");
    }

    #[test]
    fn word_boundary_underscore_mid_sentence() {
        assert_eq!(markdown_to_irc("_hello_ world"), "\x1dhello\x1d world");
    }

    #[test]
    fn underscore_not_at_word_boundary_end() {
        assert_eq!(markdown_to_irc("_foo_bar"), "_foo_bar");
    }

    // -- Code span protection ------------------------------------------------

    #[test]
    fn inline_code_suppresses_formatting() {
        assert_eq!(markdown_to_irc("`**bold**`"), "`**bold**`");
    }

    #[test]
    fn code_block_suppresses_formatting() {
        assert_eq!(markdown_to_irc("```\n**bold**\n```"), "```\n**bold**\n```");
    }

    #[test]
    fn formatting_outside_code_still_works() {
        assert_eq!(
            markdown_to_irc("**bold** `code` **also bold**"),
            "\x02bold\x02 `code` \x02also bold\x02"
        );
    }

    #[test]
    fn inline_code_before_code_block() {
        assert_eq!(
            markdown_to_irc("`inline` then ```\nblock\n```"),
            "`inline` then ```\nblock\n```"
        );
    }

    #[test]
    fn code_block_before_inline_code() {
        assert_eq!(
            markdown_to_irc("```\nblock\n``` then `inline`"),
            "```\nblock\n``` then `inline`"
        );
    }

    #[test]
    fn unclosed_code_block_in_split() {
        let lines = split_for_irc("```rust\nfn main() {");
        assert!(!lines.is_empty());
    }

    #[test]
    fn unclosed_mention_angle_brackets() {
        let r = resolve_mentions("<@111", &StubResolver);
        assert_eq!(r, "<@111");
    }

    #[test]
    fn nested_angle_brackets() {
        let r = resolve_mentions("<<@111>>", &StubResolver);
        assert!(r.contains("Alice") || r.contains("<"));
    }

    #[test]
    fn empty_mention() {
        let r = resolve_mentions("<>", &StubResolver);
        assert_eq!(r, "<>");
    }

    // -- Splitting -----------------------------------------------------------

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
        assert_eq!(lines.len(), 6);
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
        let text: String = word.repeat(100);
        let lines = split_for_irc(&text);
        assert!(lines.len() > 1);
        assert!(lines[0].len() <= MAX_LINE_BYTES);
    }

    #[test]
    fn split_exactly_max_lines() {
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
        let line = "a".repeat(MAX_LINE_BYTES);
        let lines = split_for_irc(&line);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn split_long_line_boundary_plus_one() {
        let line = "a".repeat(MAX_LINE_BYTES + 1);
        let lines = split_for_irc(&line);
        assert!(lines.len() > 1);
    }

    #[test]
    fn code_block_not_in_block_skips_closing() {
        let text = "```\nhello\n```";
        let lines = split_for_irc(text);
        assert_eq!(lines, vec!["hello"]);
    }

    #[test]
    fn replace_paired_marker_empty_inner() {
        let r = replace_paired_marker("****", "**", "[", "]");
        assert_eq!(r, "****");
    }

    #[test]
    fn code_block_closing_not_emitted() {
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
        let word = "a".repeat(50);
        let line = vec![word.as_str(); 9].join(" ");
        let parts = split_long_line(&line, MAX_LINE_BYTES);
        assert!(parts.len() >= 2);
        assert!(parts[0].len() <= MAX_LINE_BYTES);
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
        let rejoined: String = parts.join("");
        assert_eq!(rejoined, line);
    }

    #[test]
    fn split_long_line_exactly_max_bytes() {
        let line = "x".repeat(MAX_LINE_BYTES);
        let parts = split_long_line(&line, MAX_LINE_BYTES);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0], line);
    }

    #[test]
    fn split_long_line_multibyte_boundary_search() {
        let mut line = "a".repeat(MAX_LINE_BYTES - 1);
        line.push('€');
        line.push('b');
        let parts = split_long_line(&line, MAX_LINE_BYTES);
        assert!(parts.len() >= 2);
        assert!(parts[0].len() <= MAX_LINE_BYTES);
        assert!(parts[0].is_char_boundary(parts[0].len()));
    }

    #[test]
    fn split_long_line_boundary_search_decrements() {
        let mut line = "a".repeat(MAX_LINE_BYTES - 2);
        line.push('é');
        line.push_str(&"b".repeat(10));
        let parts = split_long_line(&line, MAX_LINE_BYTES);
        assert!(parts.len() >= 2);
        for part in &parts {
            assert!(part.is_char_boundary(part.len()));
        }
    }

    #[test]
    fn split_long_line_remainder_exactly_max_bytes() {
        let first = "a".repeat(350);
        let second = "b".repeat(MAX_LINE_BYTES);
        let line = format!("{first} {second}");
        let parts = split_long_line(&line, MAX_LINE_BYTES);
        assert_eq!(parts.len(), 2, "should split into exactly 2 parts");
        assert_eq!(parts[0], first);
        assert_eq!(parts[1], second);
    }

    // -- Full pipeline -------------------------------------------------------

    #[test]
    fn discord_to_irc_full_pipeline() {
        let lines = discord_to_irc("**Hello** <@111>!", &StubResolver);
        assert_eq!(lines, vec!["\x02Hello\x02 @Alice!"]);
    }

    // -- Proptest ------------------------------------------------------------

    use proptest::prelude::*;

    /// Strategy that generates strings rich in Discord markdown syntax.
    fn discord_markdown_strategy() -> impl Strategy<Value = String> {
        let atoms = prop::sample::select(vec![
            "**", "*", "__", "_", "~~", "`", "```", "\n", "\r\n", "<@", "<@!", "<@&", "<#", "<:",
            "<a:", ">", "<", ":", "hello", "world", "nick", "12345", ":emoji:", " ", "  ", "",
        ]);
        prop::collection::vec(atoms, 0..20).prop_map(|parts| parts.join(""))
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

        /// Plain text (no markdown markers or backslashes) must pass through
        /// `markdown_to_irc` completely unchanged.
        #[test]
        fn markdown_to_irc_plain_text_is_identity(
            text in "[a-zA-Z0-9 ,.!?;:]{0,200}"
        ) {
            let result = markdown_to_irc(&text);
            prop_assert_eq!(
                &result, &text,
                "plain text must survive markdown_to_irc unchanged"
            );
        }

        #[test]
        fn discord_markdown_roundtrip_never_panics(text in discord_markdown_strategy()) {
            let lines = discord_to_irc(&text, &StubResolver);
            for line in &lines {
                assert!(line.len() <= MAX_LINE_BYTES || !line.contains(' '));
            }
        }

        #[test]
        fn markdown_to_irc_rich_syntax_never_panics(text in discord_markdown_strategy()) {
            let result = markdown_to_irc(&text);
            let _ = result;
        }

        #[test]
        fn split_for_irc_never_panics(text in ".{0,2000}") {
            let lines = split_for_irc(&text);
            assert!(!lines.is_empty() || text.trim().is_empty());
        }

        /// Every line from `split_for_irc` must respect `MAX_LINE_BYTES`,
        /// unless the line has no spaces (unsplittable word).
        #[test]
        fn split_for_irc_respects_line_length(text in ".{0,2000}") {
            let lines = split_for_irc(&text);
            for line in &lines {
                prop_assert!(
                    line.len() <= MAX_LINE_BYTES || !line.contains(' '),
                    "line exceeds {MAX_LINE_BYTES} bytes and has spaces (should have been split): {:?} ({} bytes)",
                    line, line.len()
                );
            }
        }

        /// `split_for_irc` must preserve all non-whitespace content from the
        /// input (up to the line truncation limit).
        #[test]
        fn split_for_irc_preserves_words(text in "[a-zA-Z0-9 ]{0,500}") {
            let lines = split_for_irc(&text);
            let joined = lines.join(" ");
            for word in text.split_whitespace().take(50) {
                prop_assert!(
                    joined.contains(word),
                    "word {word:?} lost in split.\n  input: {text:?}\n  output: {joined:?}"
                );
            }
        }

        /// `_word_` followed by space must always produce italic markers.
        #[test]
        fn underscore_word_boundary_mid_sentence_converts(
            word in "[a-zA-Z0-9]{1,20}",
            suffix in "[a-zA-Z0-9 ,.!?]{1,20}",
        ) {
            let input = format!("_{word}_ {suffix}");
            let result = markdown_to_irc(&input);
            let expected = format!("\x1d{word}\x1d {suffix}");
            prop_assert_eq!(
                &result, &expected,
                "_word_ followed by space must become italic"
            );
        }

        /// `resolve_mentions` must pass through text that contains no `<...>`
        /// patterns completely unchanged.
        #[test]
        fn resolve_mentions_no_angle_brackets_is_identity(
            text in "[^<>]{0,200}"
        ) {
            let result = resolve_mentions(&text, &StubResolver);
            prop_assert_eq!(
                &result, &text,
                "text without angle brackets must survive unchanged"
            );
        }
    }
}
