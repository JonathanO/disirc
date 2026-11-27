//! Discord → IRC formatting: mention resolution, markdown conversion, and line splitting.

use super::{IRC_BOLD, IRC_ITALIC, IRC_UNDERLINE};

// ---------------------------------------------------------------------------
// Mention / emoji resolution
// ---------------------------------------------------------------------------

/// Resolver trait for looking up Discord entities by ID.
///
/// Implementations are provided by the bridge runtime; tests use stubs.
pub(crate) trait DiscordResolver {
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
pub(crate) fn resolve_mentions(text: &str, resolver: &dyn DiscordResolver) -> String {
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
    if let Some(rest) = inner.strip_prefix("a:").or_else(|| inner.strip_prefix(':'))
        && let Some(colon_pos) = rest.find(':')
    {
        return Some(format!(":{}:", &rest[..colon_pos]));
    }

    // Role mention: @&id
    if let Some(id) = inner.strip_prefix("@&") {
        let display = resolver
            .resolve_role(id)
            .unwrap_or_else(|| "deleted-role".to_string());
        return Some(format!("@{display}"));
    }

    // User mention: @!id (legacy) or @id
    if let Some(id) = inner.strip_prefix("@!").or_else(|| inner.strip_prefix('@')) {
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

/// [`markdown_to_irc_keeping_escapes`] with the escaped characters restored:
/// the whole markdown transform, for tests.
#[cfg(test)]
#[must_use]
pub(crate) fn markdown_to_irc(text: &str) -> String {
    restore_escaped_chars(&markdown_to_irc_keeping_escapes(text))
}

/// Convert Discord markdown formatting to IRC control codes.
///
/// Processing order matches Discord's simple-markdown parser:
/// 1. Backslash escapes
/// 2. Code blocks / inline code (protected from further formatting)
/// 3. Underline `__` (before single `_`)
/// 4. Bold `**` (before single `*`)
/// 5. Italic `*` and word-boundary `_`
/// 6. Strikethrough `~~` — passed through unchanged
///
/// Escaped characters stay as sentinels. [`split_for_irc`] restores them
/// after it finds the code blocks, so that an escaped backtick is never read
/// as a code block fence.
fn markdown_to_irc_keeping_escapes(text: &str) -> String {
    // Step 1: Replace backslash escapes with sentinels
    let mut result = replace_backslash_escapes(text);

    // Step 2: Protect code spans (they suppress all formatting)
    let (protected, code_spans) = protect_code_spans(&result);
    result = protected;

    // Step 3: Underline __text__ → \x1ftext\x1f (before single _)
    result = replace_paired_marker(&result, "__", IRC_UNDERLINE);

    // Step 4: Bold **text** → \x02text\x02
    result = replace_paired_marker(&result, "**", IRC_BOLD);

    // Step 5a: Italic *text* → \x1dtext\x1d
    result = replace_paired_marker(&result, "*", IRC_ITALIC);

    // Step 5b: Italic _text_ → \x1dtext\x1d (word boundary only)
    result = replace_word_boundary_underscores(&result);

    // Step 6: Strikethrough ~~text~~ → pass through unchanged

    restore_code_spans(&result, &code_spans)
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

/// One code span, as Discord reads it, with the text around it.
struct CodeSpan<'a> {
    /// The text before the opening delimiter.
    before: &'a str,
    /// `` ` `` for inline code, ` ``` ` for a block.
    delimiter: &'static str,
    /// The text between the delimiters.
    code: &'a str,
    /// The text after the closing delimiter.
    rest: &'a str,
}

impl CodeSpan<'_> {
    /// The length of the span from `before` to the closing delimiter.
    fn len(&self) -> usize {
        self.before.len() + self.code.len() + 2 * self.delimiter.len()
    }
}

/// Find the first code span in `text`.
///
/// - A run of three or more backticks begins a block. The block ends at the
///   next ` ``` `. If there is none, the scan stops and returns `None`.
/// - A run of one or two backticks begins inline code. It ends at the next
///   run of exactly the same length, as in simple-markdown's `inlineCode`
///   rule. A run of two with no such partner ends at the next run of one,
///   because the rule's regex backtracks. If there is none, the run is
///   plain text and the scan goes on.
///
/// `protect_code_spans` and `split_code_blocks` both read spans with this
/// function, so they agree on where every block starts and ends.
fn next_code_span(text: &str) -> Option<CodeSpan<'_>> {
    let runs = backtick_runs(text);
    for (i, &(pos, run)) in runs.iter().enumerate() {
        if run >= FENCE.len() {
            let (code, rest) = text[pos + FENCE.len()..].split_once(FENCE)?;
            return Some(CodeSpan {
                before: &text[..pos],
                delimiter: FENCE,
                code,
                rest,
            });
        }
        // The regex backtracks: a run of two with no partner of two can
        // still open a span of one, whose code starts with a backtick.
        for delimiter in ["``", "`"] {
            let len = delimiter.len();
            if len > run {
                continue;
            }
            if let Some(&(close, _)) = runs[i + 1..].iter().find(|&&(_, l)| l == len) {
                return Some(CodeSpan {
                    before: &text[..pos],
                    delimiter,
                    code: &text[pos + len..close],
                    rest: &text[close + len..],
                });
            }
        }
    }
    None
}

/// The runs of backticks in `text`, as (byte offset, length), in order.
fn backtick_runs(text: &str) -> Vec<(usize, usize)> {
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for (pos, _) in text.match_indices('`') {
        match runs.last_mut() {
            Some((start, len)) if *start + *len == pos => *len += 1,
            _ => runs.push((pos, 1)),
        }
    }
    runs
}

/// Extract code blocks and inline code, replacing them with placeholders.
///
/// Returns the modified text and a vec of the extracted code spans.
fn protect_code_spans(text: &str) -> (String, Vec<String>) {
    let mut result = String::with_capacity(text.len());
    let mut spans: Vec<String> = Vec::new();
    let mut remaining = text;

    // `rest` starts after the closing delimiter, so every pass makes
    // `remaining` shorter and the loop always ends.
    while let Some(span) = next_code_span(remaining) {
        result.push_str(span.before);
        result.push(CODE_SENTINEL);
        let idx = spans.len();
        result.push_str(&idx.to_string());
        result.push(CODE_SENTINEL);
        spans.push(remaining[span.before.len()..span.len()].to_string());
        remaining = span.rest;
    }
    result.push_str(remaining);

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
    // Characters before this index were already emitted inside an italic span.
    // A `for` loop visits each index once, so the loop always ends.
    let mut skip_to = 0;

    for (i, &ch) in chars.iter().enumerate() {
        if i < skip_to {
            continue;
        }
        if ch == '_' {
            // Check if this _ is at a word boundary (start of string or preceded by whitespace)
            let at_word_start = i == 0 || chars[i - 1].is_whitespace();

            if at_word_start {
                // Look for closing _ at a word boundary
                if let Some(close) = find_word_boundary_close(&chars, i + 1) {
                    let inner = &chars[i + 1..close];
                    if !inner.is_empty() {
                        result.push(IRC_ITALIC);
                        result.extend(inner);
                        result.push(IRC_ITALIC);
                        skip_to = close + 1;
                        continue;
                    }
                }
            }
        }
        result.push(ch);
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

/// Replace paired markers like `**text**` with `code + text + code`.
fn replace_paired_marker(text: &str, marker: &str, code: char) -> String {
    let mut result = String::with_capacity(text.len());
    let mut remaining = text;

    // Every pass continues after at least one whole marker, and markers are
    // non-empty, so `remaining` always gets shorter and the loop always ends.
    loop {
        let Some((before, after_open)) = remaining.split_once(marker) else {
            result.push_str(remaining);
            break;
        };

        // Look for closing marker
        let Some((inner, rest)) = after_open.split_once(marker) else {
            // No closing marker — push rest and break
            result.push_str(remaining);
            break;
        };

        if inner.is_empty() {
            // Empty content between markers — leave the opening marker as-is
            result.push_str(before);
            result.push_str(marker);
            remaining = after_open;
            continue;
        }

        result.push_str(before);
        result.push(code);
        result.push_str(inner);
        result.push(code);
        remaining = rest;
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
pub(crate) fn split_for_irc(text: &str) -> Vec<String> {
    // Normalise newlines, and remove NUL.  IRC lines cannot carry CR, LF, or
    // NUL; each line break becomes a separate line, and NUL has no meaning.
    let text = text
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\0', "");

    // Process code blocks: first line as-is, rest with continuation prefix.
    // Then restore escaped characters: after the code blocks, so that an
    // escaped backtick is never a fence, and before the length split, so
    // that the split measures the real characters.
    let lines: Vec<String> = split_code_blocks(&text)
        .iter()
        .map(|line| restore_escaped_chars(line))
        .collect();

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

/// The fence that opens and closes a Discord code block.
const FENCE: &str = "```";

/// Process code blocks: first line as-is, subsequent lines get continuation prefix.
///
/// Spans are read with [`next_code_span`], the same as `protect_code_spans`.
/// Spec 05 "Code block fences" gives the rules.
fn split_code_blocks(text: &str) -> Vec<String> {
    let mut out = CodeBlockLines::default();
    let mut remaining = text;
    let mut at_line_start = true;
    while let Some(span) = next_code_span(remaining) {
        if span.delimiter == FENCE && span.code.contains('\n') {
            // Drop whitespace next to the fences, but only on the fence lines.
            out.push_text(span.before.trim_end_matches(is_inline_space));
            out.end_line();
            out.push_block(span.code);
            remaining = span.rest.trim_start_matches(is_inline_space);
        } else {
            // Inline code, or a block on one line: text, unchanged.
            out.push_text(&remaining[..span.len()]);
            remaining = span.rest;
        }
        at_line_start = false;
    }
    out.push_unclosed_tail(remaining, at_line_start);
    out.end_line();
    out.lines
}

/// The lines that [`split_code_blocks`] builds.
#[derive(Default)]
struct CodeBlockLines {
    lines: Vec<String>,
    /// The normal line that is not complete yet.
    line: String,
}

impl CodeBlockLines {
    /// Add normal text. Each line break in it ends the current line.
    fn push_text(&mut self, text: &str) {
        let mut pieces = text.split('\n');
        if let Some(first) = pieces.next() {
            self.line.push_str(first);
        }
        for piece in pieces {
            self.end_line();
            self.line.push_str(piece);
        }
    }

    /// End the current normal line. An empty line is not sent.
    fn end_line(&mut self) {
        let line = std::mem::take(&mut self.line);
        if !line.is_empty() {
            self.lines.push(line);
        }
    }

    /// Push the code of a block that has a line break. The text on the
    /// opening fence line is dropped if it is empty or a language tag. The
    /// text on the closing fence line is dropped if it is only whitespace.
    fn push_block(&mut self, code: &str) {
        let mut pieces: Vec<&str> = code.split('\n').collect();
        let last = pieces.pop().filter(|last| !last.trim().is_empty());
        let mut pieces = pieces.into_iter();
        let first = pieces
            .next()
            .filter(|first| !is_fence_info(first.trim_end()));
        self.push_code_lines(first.into_iter().chain(pieces).chain(last));
    }

    /// Push the code lines of one block: the first as it is, later lines
    /// with the continuation prefix.
    fn push_code_lines<'a>(&mut self, code: impl IntoIterator<Item = &'a str>) {
        for (i, line) in code.into_iter().enumerate() {
            if i == 0 {
                self.lines.push(line.to_string());
            } else {
                self.lines.push(format!("{CODE_CONTINUATION}{line}"));
            }
        }
    }

    /// Push the text after the last span that closes. `protect_code_spans`
    /// protects none of it. A fence at the start of a source line in this
    /// text still opens a block to the end of the message. `at_line_start`
    /// says whether the text starts a source line.
    fn push_unclosed_tail(&mut self, text: &str, at_line_start: bool) {
        let mut pieces = text.split('\n');
        let mut starts_line = at_line_start;
        while let Some(piece) = pieces.next() {
            if starts_line && let Some(rest) = piece.strip_prefix(FENCE) {
                let first = Some(rest).filter(|rest| !is_fence_info(rest.trim_end()));
                self.push_code_lines(first.into_iter().chain(pieces.by_ref()));
                return;
            }
            self.line.push_str(piece);
            self.end_line();
            starts_line = true;
        }
    }
}

/// True for whitespace that does not end a line.
fn is_inline_space(c: char) -> bool {
    c.is_whitespace() && c != '\n'
}

/// True if the text after an opening fence carries no code: it is empty,
/// or it is one language tag such as `rust` or `c++`.
fn is_fence_info(rest: &str) -> bool {
    rest.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-' | '.' | '#'))
}

/// Split a line that exceeds `max_bytes` at word boundaries.
///
/// Splits are always at valid UTF-8 char boundaries.
fn split_long_line(line: &str, max_bytes: usize) -> Vec<String> {
    use unicode_segmentation::UnicodeSegmentation;

    let mut parts = Vec::new();
    let mut remaining = line;

    while remaining.len() > max_bytes {
        // Find the last grapheme boundary at or before max_bytes.  A
        // grapheme boundary is always a char boundary too, so this gives
        // us safe splitting even for ZWJ sequences, skin-tone modifiers,
        // and flag pairs.  Falls back to 0 if the first grapheme is
        // itself longer than max_bytes, causing a hard split.
        let boundary = remaining
            .grapheme_indices(true)
            .take_while(|(i, _)| *i <= max_bytes)
            .last()
            .map_or(0, |(i, _)| i);
        let boundary = if boundary == 0 {
            // The first grapheme is larger than max_bytes. A ZWJ sequence has
            // no length limit, so one cluster can exceed any budget.
            //
            // Split inside the cluster at the last char boundary that fits.
            // This breaks the cluster, and the emoji does not render. Emitting
            // the whole cluster is worse: the IRC server truncates the line at
            // its own limit, and it can cut the line in the middle of a
            // character.
            let last_char_start = remaining
                .char_indices()
                .take_while(|(i, _)| *i <= max_bytes)
                .last()
                .map_or(0, |(i, _)| i);
            if last_char_start == 0 {
                // Even the first char is larger than max_bytes. Emit that char
                // alone. This exceeds the budget, but it guarantees progress.
                remaining
                    .chars()
                    .next()
                    .map_or(remaining.len(), char::len_utf8)
            } else {
                last_char_start
            }
        } else {
            boundary
        };

        // Every branch above must move past at least one byte. A boundary of 0
        // pushes an empty string and leaves `remaining` unchanged, so the loop
        // never ends.
        debug_assert!(boundary > 0, "split_long_line must make progress");

        // Find last space before the grapheme boundary.  ASCII space is
        // always its own grapheme, so a space position is already on a
        // grapheme boundary.
        match remaining[..boundary].rfind(' ') {
            Some(0) | None => {
                // No usable space — hard-split at grapheme boundary.
                parts.push(remaining[..boundary].to_string());
                remaining = &remaining[boundary..];
            }
            Some(space_pos) => {
                // `rest` starts with the space, and `space_pos` is not 0, so
                // `line` is not empty and `remaining` always gets shorter.
                let (line, rest) = remaining.split_at(space_pos);
                parts.push(line.to_string());
                remaining = &rest[1..]; // skip the ASCII space
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
pub(crate) fn discord_to_irc(text: &str, resolver: &dyn DiscordResolver) -> Vec<String> {
    let resolved = resolve_mentions(text, resolver);
    let formatted = markdown_to_irc_keeping_escapes(&resolved);
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
        assert_eq!(markdown_to_irc("***text***"), "\x02\x1dtext\x02\x1d");
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
        assert_eq!(
            markdown_to_irc("_**bold inside italic**_"),
            "\x1d\x02bold inside italic\x02\x1d"
        );
    }

    /// Overlapping markers convert best-effort: the bold pair closes first,
    /// then the leftover single stars pair across the bold close.
    #[test]
    fn overlapping_markers_convert_best_effort() {
        assert_eq!(
            markdown_to_irc("**bold *and italic** end*"),
            "\x02bold \x1dand italic\x02 end\x1d"
        );
    }

    #[test]
    fn deeply_nested_markers() {
        assert_eq!(
            markdown_to_irc("**__~~text~~__**"),
            "\x02\x1f~~text~~\x1f\x02"
        );
    }

    /// With no closing markers, the two adjacent stars of `**` pair as an
    /// empty single-star match (passed through), the next single-star pair
    /// converts, and everything else passes through literally.
    #[test]
    fn multiple_unclosed_markers() {
        assert_eq!(
            markdown_to_irc("**bold *italic __underline ~~strike"),
            "*\x1dbold \x1ditalic __underline ~~strike"
        );
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
    fn inline_code_with_formatting_before_code_block() {
        // The inline code `*not_italic*` must be picked up first (position 0)
        // so its content is protected from markdown conversion.  If delimiter
        // selection picked the later ``` first, the `*` would be converted to
        // IRC italic control codes.
        assert_eq!(
            markdown_to_irc("`*protected*` then ```block```"),
            "`*protected*` then ```block```"
        );
    }

    #[test]
    fn code_block_with_formatting_before_inline() {
        // The ``` block appears first (position 0) — its content must be
        // protected.  If delimiter selection wrongly chose single backtick
        // first, the `**bold**` would be converted.
        assert_eq!(
            markdown_to_irc("```**protected**``` then `inline`"),
            "```**protected**``` then `inline`"
        );
    }

    // ---- protect_code_spans: delimiter selection and round-trip -----------
    //
    // `protect_code_spans` finds the first backtick and treats it as a block
    // delimiter iff it begins a "```".  These tests pin that decision and the
    // span-boundary arithmetic.  The equivalence they establish is the one the
    // old `t <= s` match-guard exclusion used to assert by comment alone:
    // whichever delimiter is chosen, *all* code content ends up protected,
    // because any leftover backticks are re-parsed on the next iteration.

    /// A triple backtick at position 0 must be taken as one block span,
    /// not as an inline span containing two stray backticks.
    #[test]
    fn triple_backtick_is_protected_as_a_single_block_span() {
        let (out, spans) = protect_code_spans("```code```");
        assert_eq!(spans, vec!["```code```".to_string()]);
        assert!(!out.contains('`'), "no backticks may leak: {out:?}");
    }

    /// A lone backtick pair is an inline span.
    #[test]
    fn single_backtick_is_protected_as_an_inline_span() {
        let (_out, spans) = protect_code_spans("`code`");
        assert_eq!(spans, vec!["`code`".to_string()]);
    }

    /// Block and inline spans in the same text are extracted separately and
    /// in source order.
    #[test]
    fn block_and_inline_spans_extracted_in_order() {
        let (_out, spans) = protect_code_spans("```blk``` mid `inl`");
        assert_eq!(
            spans,
            vec!["```blk```".to_string(), "`inl`".to_string()],
            "spans must be captured in source order"
        );
    }

    // Four backticks: the first three open a block whose closing delimiter is
    // the next triple-backtick run.  Whatever the split, every backtick must
    // end up inside a captured span or in the trailing remainder — never
    // silently dropped.
    #[test]
    fn adjacent_backtick_runs_lose_no_content() {
        for input in [
            "````x````",
            "```a``` `b`",
            "`a` ```b```",
            "``````",
            "```` ````",
        ] {
            let (out, spans) = protect_code_spans(input);
            let restored = restore_code_spans(&out, &spans);
            assert_eq!(restored, input, "round-trip must be lossless for {input:?}");
        }
    }

    /// Unclosed delimiters are passed through verbatim rather than swallowed.
    #[test]
    fn unclosed_delimiter_is_passed_through() {
        let (out, spans) = protect_code_spans("`unclosed");
        assert!(spans.is_empty(), "nothing should be extracted");
        assert_eq!(out, "`unclosed");
    }

    #[test]
    fn text_without_backticks_is_unchanged_and_extracts_nothing() {
        let (out, spans) = protect_code_spans("plain **text** here");
        assert_eq!(spans, Vec::<String>::new());
        assert_eq!(out, "plain **text** here");
    }

    /// Protect-then-restore is the identity for arbitrary text.  This is
    /// the core equivalence property: no matter which delimiter the
    /// selection logic picks, no content is lost or duplicated.  It also
    /// pins the span-boundary arithmetic — an off-by-one in `after_open`
    /// or `full_span_end` breaks the round-trip.
    #[hegel::test]
    fn protect_restore_round_trip_is_identity(tc: TestCase) {
        let s = tc.draw(gs::from_regex(r"[a-z *_~`\n]{0,60}"));
        let (out, spans) = protect_code_spans(&s);
        assert_eq!(restore_code_spans(&out, &spans), s);
    }

    /// Every extracted span must be a substring of the input that both
    /// starts and ends with a backtick — i.e. the boundaries are real
    /// delimiter positions, not arbitrary offsets.
    #[hegel::test]
    fn extracted_spans_are_well_formed(tc: TestCase) {
        let s = tc.draw(gs::from_regex(
            r"[a-z ]{0,10}(`{1,3}[a-z ]{0,10}`{1,3}[a-z ]{0,10}){0,3}",
        ));
        let (_out, spans) = protect_code_spans(&s);
        for span in &spans {
            assert!(s.contains(span.as_str()), "span not a substring: {span:?}");
            assert!(
                span.starts_with('`'),
                "span must open with a backtick: {span:?}"
            );
            assert!(
                span.ends_with('`'),
                "span must close with a backtick: {span:?}"
            );
        }
    }

    /// Every line `split_for_irc` emits fits the IRC byte budget, for any
    /// input.  This is the function's entire contract, and until now only
    /// one example asserted it.  The adversarial strategy supplies no
    /// spaces, so the word-boundary path cannot be used to satisfy it.
    #[hegel::test]
    fn split_for_irc_lines_never_exceed_limit(tc: TestCase) {
        let text = tc.draw(hegel::one_of!(
            crate::formatting::test_support::adversarial_unicode_no_spaces(600),
            crate::formatting::test_support::oversized_grapheme_cluster(MAX_LINE_BYTES),
        ));
        for line in split_for_irc(&text) {
            assert!(
                line.len() <= MAX_LINE_BYTES,
                "emitted a {}-byte line, over the {MAX_LINE_BYTES}-byte budget: {line:?}",
                line.len()
            );
        }
    }

    /// Content inside a code span is never markdown-converted: whatever
    /// the delimiter choice, no IRC control codes appear in the output for
    /// text that is entirely inside backticks.
    #[hegel::test]
    fn code_content_is_never_markdown_converted(tc: TestCase) {
        let inner = tc.draw(gs::from_regex(r"[a-z]{0,8}(\*\*|__|~~|\*)[a-z]{0,8}"));
        let fence = tc.draw(gs::from_regex(r"`{1,3}"));
        let input = format!("{fence}{inner}{fence}");
        let out = markdown_to_irc(&input);
        assert!(
            !out.contains(IRC_BOLD) && !out.contains(IRC_ITALIC),
            "markdown inside code was converted: {input:?} -> {out:?}"
        );
    }

    #[test]
    fn unclosed_code_block_in_split() {
        assert_eq!(split_for_irc("```rust\nfn main() {"), vec!["fn main() {"]);
    }

    #[test]
    fn unclosed_mention_angle_brackets() {
        let r = resolve_mentions("<@111", &StubResolver);
        assert_eq!(r, "<@111");
    }

    #[test]
    fn nested_angle_brackets() {
        // The outer "<" finds "<@111" as its span (unresolvable), passes it
        // through, then the inner "<@111>" resolves normally.
        assert_eq!(resolve_mentions("<<@111>>", &StubResolver), "<@Alice>");
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

    /// A single grapheme cluster larger than the budget must still be split.
    ///
    /// The splitter looks for a grapheme boundary at or before the budget. One
    /// oversized cluster has no such boundary. The old code then emitted the
    /// whole cluster on one line, which was over the budget.
    #[test]
    fn split_breaks_a_grapheme_cluster_larger_than_the_budget() {
        // One ZWJ sequence of 58 joined emoji is one grapheme cluster.
        let cluster: String = "\u{1F468}\u{200D}".repeat(57) + "\u{1F468}";
        assert!(
            cluster.len() > MAX_LINE_BYTES,
            "test input must exceed the budget"
        );

        let lines = split_for_irc(&cluster);

        for line in &lines {
            assert!(
                line.len() <= MAX_LINE_BYTES,
                "emitted a {}-byte line, over the {MAX_LINE_BYTES}-byte budget",
                line.len()
            );
        }

        // The split must land on the last char boundary that fits, not on an
        // arbitrary earlier one. Each emoji plus ZWJ pair is 7 bytes, so the
        // last char that starts at or before byte 400 starts at byte 399
        // (7 x 57). That leaves one 4-byte emoji on the second line.
        assert_eq!(lines.len(), 2, "expected one split");
        assert_eq!(lines[0].len(), 399, "split did not use the full budget");
        assert_eq!(lines[1].len(), 4);

        // A hard split must not lose or duplicate content.
        assert_eq!(lines.concat(), cluster, "content changed across the split");
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
    fn code_on_the_opening_fence_line_is_the_first_code_line() {
        let text = "```fn main() {\n    run();\n}\n```";
        assert_eq!(
            split_for_irc(text),
            vec!["fn main() {", "\x02>\x02     run();", "\x02>\x02 }"]
        );
    }

    #[test]
    fn language_tag_with_symbols_is_dropped() {
        assert_eq!(split_for_irc("```c++\nint x;\n```"), vec!["int x;"]);
    }

    #[test]
    fn text_after_the_closing_fence_is_a_normal_line() {
        let text = "```\ncode\n``` see above";
        assert_eq!(split_for_irc(text), vec!["code", "see above"]);
    }

    #[test]
    fn closing_fence_at_the_end_of_a_code_line_closes_the_block() {
        let text = "```\nlet x = 1;\nx += 1;```\nafter";
        assert_eq!(
            split_for_irc(text),
            vec!["let x = 1;", "\x02>\x02 x += 1;", "after"]
        );
    }

    #[test]
    fn one_line_block_at_line_start_is_kept_and_opens_no_block() {
        let text = "```ls -la``` then run\nnext";
        assert_eq!(split_for_irc(text), vec!["```ls -la``` then run", "next"]);
    }

    #[test]
    fn block_after_a_one_line_block_on_the_same_line() {
        let text = "```a``` b ```\ncode\n```\nafter\nmore";
        assert_eq!(
            split_for_irc(text),
            vec!["```a``` b", "code", "after", "more"]
        );
    }

    #[test]
    fn fence_in_the_middle_of_a_line_opens_a_block() {
        let text = "see ```\ncode\nmore code\n```\nafter";
        assert_eq!(
            split_for_irc(text),
            vec!["see", "code", "\x02>\x02 more code", "after"]
        );
    }

    #[test]
    fn whitespace_after_a_fence_is_not_a_code_line() {
        assert_eq!(split_for_irc("``` \ncode\n  ```"), vec!["code"]);
        assert_eq!(split_for_irc("```rust \ncode\n```"), vec!["code"]);
    }

    #[test]
    fn three_backticks_in_inline_code_open_no_block() {
        let text = "use `` ``` `` to start a block\nok\nthanks";
        assert_eq!(
            split_for_irc(text),
            vec!["use `` ``` `` to start a block", "ok", "thanks"]
        );
        assert_eq!(split_for_irc("`a```b`\nx\ny"), vec!["`a```b`", "x", "y"]);
    }

    /// An unpaired backtick (here an apostrophe) is plain text. It must not
    /// pair with the first backtick of a later fence.
    #[test]
    fn unpaired_backtick_before_a_block_is_plain_text() {
        let text = "I don`t get this:\n```\nfoo\nbar\n```\nany ideas?";
        assert_eq!(
            split_for_irc(text),
            vec!["I don`t get this:", "foo", "\x02>\x02 bar", "any ideas?"]
        );
        // The protection agrees, so no formatting applies inside the block.
        let text = "I don`t get it:\n```\n**foo**\n```";
        assert_eq!(markdown_to_irc(text), text);
    }

    /// Inline code ends at a run of the same length (simple-markdown's
    /// `inlineCode` rule), so a double-backtick span can hold one backtick.
    #[test]
    fn inline_code_ends_at_a_run_of_the_same_length() {
        let (_out, spans) = protect_code_spans("a ``x ` y`` **b**");
        assert_eq!(spans, vec!["``x ` y``".to_string()]);
        assert_eq!(
            markdown_to_irc("a ``x ` *y*`` **b**"),
            "a ``x ` *y*`` \x02b\x02"
        );
    }

    /// A run of two backticks with no partner of two ends at the next single
    /// backtick, because simple-markdown's regex backtracks.
    #[test]
    fn double_backtick_with_no_partner_ends_at_a_single_backtick() {
        let (_out, spans) = protect_code_spans("``*`*```");
        assert_eq!(spans, vec!["``*`".to_string()]);
        assert_eq!(markdown_to_irc("``*`*```"), "``*`*```");
    }

    /// In the text after an unclosed fence, only a fence at the start of a
    /// source line opens a block. A fence after a closing fence on the same
    /// line is plain text.
    #[test]
    fn unclosed_fence_after_a_closing_fence_is_plain_text() {
        let text = "```\na\n```  ```\nb\nc";
        assert_eq!(split_for_irc(text), vec!["a", "```", "b", "c"]);
    }

    /// Four backticks: the fence is the first three, as in
    /// `protect_code_spans`, so a backtick is left on each fence line.
    #[test]
    fn four_backticks_read_as_a_fence_and_a_backtick() {
        assert_eq!(
            split_for_irc("````\ncode\n````"),
            vec!["`", "\x02>\x02 code", "`"]
        );
    }

    #[test]
    fn whitespace_on_fence_lines_does_not_touch_other_lines() {
        let text = "x  \n ```\ncode\n``` \n  y `z`";
        assert_eq!(split_for_irc(text), vec!["x  ", "code", "  y `z`"]);
    }

    /// The length split measures the restored characters. A sentinel is
    /// longer than the character, so measuring it would split too early.
    #[test]
    fn escaped_characters_do_not_make_lines_split_early() {
        let text = format!("{}x\\*", "ab ".repeat(132));
        let lines = discord_to_irc(&text, &StubResolver);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0].len(), 398);
        let stars = discord_to_irc(&"\\*".repeat(200), &StubResolver);
        assert_eq!(stars, vec!["*".repeat(200)]);
    }

    /// An escaped backtick is plain text in Discord. The split must not
    /// read it as a fence.
    #[test]
    fn escaped_backticks_are_not_a_fence() {
        let text = "```md\nUse \\`\\`\\` here\n```\nafter\nmore";
        assert_eq!(
            discord_to_irc(text, &StubResolver),
            vec!["Use ``` here", "after", "more"]
        );
        assert_eq!(
            discord_to_irc("\\`\\`\\`rust\ncode", &StubResolver),
            vec!["```rust", "code"]
        );
    }

    #[test]
    fn replace_paired_marker_empty_inner() {
        let r = replace_paired_marker("****", "**", '[');
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
        let line = [word.as_str(); 9].join(" ");
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
    fn split_long_line_exactly_max_bytes_with_space_stays_whole() {
        // A string of exactly max_bytes with an internal space must NOT be
        // split — it fits in one line.  Catches > vs >= in the outer loop.
        let mut line = "a".repeat(MAX_LINE_BYTES - 5);
        line.push_str(" bbbb");
        assert_eq!(line.len(), MAX_LINE_BYTES);
        let parts = split_long_line(&line, MAX_LINE_BYTES);
        assert_eq!(parts.len(), 1, "should not split at exactly max_bytes");
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

    // -- Property tests ------------------------------------------------------

    use hegel::prelude::*;

    /// Regression: a NUL in Discord text reached the serializer, which
    /// rejects it, so the whole message was dropped.
    #[test]
    fn discord_to_irc_removes_nul() {
        assert_eq!(discord_to_irc("a\0b", &StubResolver), ["ab"]);
    }

    /// IRC lines cannot carry CR, LF, or NUL, so no emitted line may contain
    /// them, whatever the Discord text holds.  The serializer rejects such a
    /// line, and the whole message would be lost.
    #[hegel::test]
    fn discord_to_irc_lines_have_no_line_breaks_or_nul(tc: TestCase) {
        let text = tc.draw(
            gs::vecs(hegel::one_of!(
                gs::sampled_from(vec!["\0", "\r", "\n", "\r\n"]).map(str::to_string),
                gs::text().max_size(20),
            ))
            .max_size(8)
            .map(|parts: Vec<String>| parts.concat()),
        );
        for line in discord_to_irc(&text, &StubResolver) {
            assert!(!line.contains(['\r', '\n', '\0']), "line {line:?}");
        }
    }

    /// Generator for strings rich in Discord markdown syntax.
    fn discord_markdown_strategy() -> impl PrintableGenerator<String> {
        let atoms = gs::sampled_from(vec![
            "**", "*", "__", "_", "~~", "`", "```", "\n", "\r\n", "<@", "<@!", "<@&", "<#", "<:",
            "<a:", ">", "<", ":", "hello", "world", "nick", "12345", ":emoji:", " ", "  ", "",
        ]);
        gs::vecs(atoms)
            .max_size(19)
            .map(|parts: Vec<&str>| parts.join(""))
    }

    #[hegel::test]
    fn resolve_mentions_never_panics(tc: TestCase) {
        let text = tc.draw(gs::from_regex(".*"));
        let _ = resolve_mentions(&text, &StubResolver);
    }

    #[hegel::test]
    fn markdown_to_irc_never_panics(tc: TestCase) {
        let text = tc.draw(gs::from_regex(".*"));
        let _ = markdown_to_irc(&text);
    }

    /// Plain text (no markdown markers or backslashes) must pass through
    /// `markdown_to_irc` completely unchanged.
    #[hegel::test]
    fn markdown_to_irc_plain_text_is_identity(tc: TestCase) {
        let text = tc.draw(gs::from_regex("[a-zA-Z0-9 ,.!?;:]{0,200}"));
        let result = markdown_to_irc(&text);
        assert_eq!(
            &result, &text,
            "plain text must survive markdown_to_irc unchanged"
        );
    }

    #[hegel::test]
    fn discord_markdown_roundtrip_never_panics(tc: TestCase) {
        let text = tc.draw(discord_markdown_strategy());
        let lines = discord_to_irc(&text, &StubResolver);
        for line in &lines {
            assert!(line.len() <= MAX_LINE_BYTES || !line.contains(' '));
        }
    }

    #[hegel::test]
    fn split_for_irc_never_panics(tc: TestCase) {
        let text = tc.draw(gs::from_regex(".{0,2000}"));
        let lines = split_for_irc(&text);
        // split_for_irc removes NUL, so text that is only NUL and whitespace
        // gives no lines.
        assert!(!lines.is_empty() || text.replace('\0', "").trim().is_empty());
    }

    /// Every line from `split_for_irc` must respect `MAX_LINE_BYTES`,
    /// unless the line has no spaces (unsplittable word).
    #[hegel::test]
    fn split_for_irc_respects_line_length(tc: TestCase) {
        let text = tc.draw(gs::from_regex(".{0,2000}"));
        let lines = split_for_irc(&text);
        for line in &lines {
            assert!(
                line.len() <= MAX_LINE_BYTES || !line.contains(' '),
                "line exceeds {MAX_LINE_BYTES} bytes and has spaces (should have been split): {:?} ({} bytes)",
                line,
                line.len()
            );
        }
    }

    /// Text on a fence line is code or normal text, so it reaches the
    /// output. Only fences, a language tag right after an opening fence, and
    /// whitespace on the fence lines are dropped. The generator follows the
    /// block state, so it knows which fence opens and which closes, and for
    /// each output line its words and whether it has the code prefix (each
    /// input line gives 0 or 1 lines). Each word has a unique counter suffix.
    /// One word can hold an unpaired backtick. The test calls
    /// `split_code_blocks` directly, so the line limit does not cap the
    /// number of parts.
    #[hegel::test]
    fn split_code_blocks_keeps_text_on_fence_lines(tc: TestCase) {
        /// One expected output line: whether it has the code prefix, and
        /// its words.
        type Expected = (bool, Vec<String>);

        /// Add a code line: it has the prefix if the block has a line already.
        fn code_line(expected: &mut Vec<Expected>, block_has_line: &mut bool, words: Vec<String>) {
            expected.push((*block_has_line, words));
            *block_has_line = true;
        }

        let mut counter = 0;
        let mut tick_used = false;
        let mut in_block = false;
        // Whether the open block already has a code line.
        let mut block_has_line = false;
        // If the open block was opened in the middle of a line: where its
        // lines start in `expected`, and whether its fence line is a line of
        // its own when the block never closes. Such a block is plain text.
        let mut opened_mid_line: Option<(usize, bool)> = None;
        let mut input = Vec::new();
        let mut expected: Vec<Expected> = Vec::new();
        let parts = tc.draw(gs::integers::<usize>().min_value(1).max_value(12));
        for _ in 0..parts {
            let word_count = tc.draw(gs::integers::<usize>().min_value(1).max_value(3));
            let words: Vec<String> = (0..word_count)
                .map(|_| {
                    counter += 1;
                    // One word can hold one backtick, used as an
                    // apostrophe. It is the only one, so it never pairs.
                    let pattern = if !tick_used && tc.draw(gs::booleans()) {
                        tick_used = true;
                        "[a-z]{1,3}`[a-z]{1,3}"
                    } else {
                        "[a-z]{1,6}"
                    };
                    format!("{}{counter}", tc.draw(gs::from_regex(pattern)))
                })
                .collect();
            let text = words.join(" ");
            let line = match (tc.draw(gs::integers::<u8>().max_value(4)), in_block) {
                // A plain line: code inside a block, normal text outside.
                (0, true) => {
                    code_line(&mut expected, &mut block_has_line, words);
                    text
                }
                (0, false) => {
                    expected.push((false, words));
                    text
                }
                // An opening fence with an optional language tag, or a
                // closing fence, each with optional trailing whitespace.
                (1, false) => {
                    in_block = true;
                    block_has_line = false;
                    opened_mid_line = None;
                    let tag = tc.draw(gs::from_regex(r"[A-Za-z0-9_+.#-]{0,6}[ \t]{0,2}"));
                    format!("```{tag}")
                }
                (1, true) => {
                    let space = tc.draw(gs::from_regex(r"[ \t]{0,2}"));
                    if tc.draw(gs::booleans()) {
                        // A new opening fence after the closing fence, on the
                        // same line: not at the start of a source line.
                        block_has_line = false;
                        opened_mid_line = Some((expected.len(), true));
                        format!("{space}```{space}```")
                    } else {
                        in_block = false;
                        format!("{space}```{space}")
                    }
                }
                // A fence, then text: the first code line of a new block, or
                // normal text after a closing fence.
                (2, false) => {
                    in_block = true;
                    block_has_line = false;
                    opened_mid_line = None;
                    code_line(&mut expected, &mut block_has_line, words);
                    format!("``` {text}")
                }
                (2, true) => {
                    in_block = false;
                    expected.push((false, words));
                    format!("``` {text}")
                }
                // A one-line block, which stays one line.
                (3, false) => {
                    expected.push((false, words));
                    format!("```{text}```")
                }
                // A last code line that ends with the closing fence.
                (3, true) => {
                    in_block = false;
                    code_line(&mut expected, &mut block_has_line, words);
                    format!("{text}```")
                }
                // A fence at the end of a normal line opens a block.
                (_, false) => {
                    expected.push((false, words));
                    in_block = true;
                    block_has_line = false;
                    opened_mid_line = Some((expected.len(), false));
                    format!("{text} ```")
                }
                // A closing fence, then text that opens a new block.
                (_, true) => {
                    expected.push((false, words));
                    block_has_line = false;
                    opened_mid_line = Some((expected.len(), false));
                    format!("```{text} ```")
                }
            };
            input.push(line);
        }
        // A block opened in the middle of a line that never closes is plain
        // text: its lines have no code prefix.
        if in_block && let Some((first, fence_line)) = opened_mid_line {
            for line in &mut expected[first..] {
                line.0 = false;
            }
            if fence_line {
                expected.insert(first, (false, Vec::new()));
            }
        }
        let input = input.join("\n");

        let output = split_code_blocks(&input);
        let actual: Vec<Expected> = output
            .iter()
            .map(|line| {
                let (prefixed, body) = match line.strip_prefix(CODE_CONTINUATION) {
                    Some(body) => (true, body),
                    None => (false, line.as_str()),
                };
                // Fences touch words only at their ends.
                let words = body
                    .split_whitespace()
                    .map(|t| t.trim_matches('`'))
                    .filter(|t| !t.is_empty())
                    .map(str::to_string)
                    .collect();
                (prefixed, words)
            })
            .collect();
        assert_eq!(
            actual, expected,
            "(prefix, words) per line.\n  input: {input:?}\n  output: {output:?}"
        );
    }

    /// A line outside every code block, as `protect_code_spans` reads the
    /// blocks, reaches IRC unchanged and without the code prefix. The small
    /// alphabet makes inline code, one-line blocks, runs of four backticks,
    /// and spans that do not close common. Lines after the first span that
    /// does not close are not checked; that text has its own rule.
    #[hegel::test]
    fn split_code_blocks_keeps_lines_outside_blocks(tc: TestCase) {
        let text = tc.draw(gs::from_regex("[`a \n]{0,60}"));

        // The byte ranges of the blocks that have a line break, and the end
        // of the text that the scan reads.
        let mut blocks = Vec::new();
        let mut scanned_end = 0;
        let mut remaining = text.as_str();
        while let Some(span) = next_code_span(remaining) {
            if span.delimiter == FENCE && span.code.contains('\n') {
                blocks.push(scanned_end + span.before.len()..scanned_end + span.len());
            }
            scanned_end += span.len();
            remaining = span.rest;
        }

        let mut expected = Vec::new();
        let mut line_start = 0;
        for line in text.split('\n') {
            let line_end = line_start + line.len();
            let in_block = blocks
                .iter()
                .any(|block| block.start < line_end && line_start < block.end);
            if !line.is_empty() && !in_block && line_end <= scanned_end {
                expected.push(line);
            }
            line_start = line_end + 1;
        }

        // `split_code_blocks` directly: the line limit is not part of this.
        let output = split_code_blocks(&text);
        let mut rest = output.iter();
        for line in &expected {
            assert!(
                rest.any(|out| out == line),
                "line {line:?} changed or lost.\n  input: {text:?}\n  output: {output:?}"
            );
        }
    }

    /// `split_for_irc` must preserve all non-whitespace content from the
    /// input (up to the line truncation limit).
    #[hegel::test]
    fn split_for_irc_preserves_words(tc: TestCase) {
        let text = tc.draw(gs::from_regex("[a-zA-Z0-9 ]{0,500}"));
        let lines = split_for_irc(&text);
        let joined = lines.join(" ");
        for word in text.split_whitespace().take(50) {
            assert!(
                joined.contains(word),
                "word {word:?} lost in split.\n  input: {text:?}\n  output: {joined:?}"
            );
        }
    }

    /// `split_long_line` must never split an extended grapheme cluster.
    /// Use a no-spaces adversarial strategy so the word-boundary fallback
    /// can't avoid the grapheme-rich region.
    #[hegel::test]
    fn split_long_line_preserves_grapheme_clusters(tc: TestCase) {
        let text = tc.draw(crate::formatting::test_support::adversarial_unicode_no_spaces(200));
        use unicode_segmentation::UnicodeSegmentation;
        let parts = split_long_line(&text, MAX_LINE_BYTES);
        // Rejoining the parts must yield the original text (catches
        // byte-level loss, but not grapheme splitting on its own).
        let rejoined: String = parts.join("");
        assert_eq!(&rejoined, &text);
        // If splits landed inside a grapheme cluster, the total
        // grapheme count of the parts would be greater than the
        // grapheme count of the input — one cluster "became" two.
        let input_graphemes = text.graphemes(true).count();
        let part_graphemes: usize = parts.iter().map(|p| p.graphemes(true).count()).sum();
        assert_eq!(
            input_graphemes, part_graphemes,
            "split increased grapheme count (split inside a cluster); \
             input: {text:?}, parts: {parts:?}"
        );
    }

    /// `_word_` followed by space must always produce italic markers.
    #[hegel::test]
    fn underscore_word_boundary_mid_sentence_converts(tc: TestCase) {
        let word = tc.draw(gs::from_regex("[a-zA-Z0-9]{1,20}"));
        let suffix = tc.draw(gs::from_regex("[a-zA-Z0-9 ,.!?]{1,20}"));
        let input = format!("_{word}_ {suffix}");
        let result = markdown_to_irc(&input);
        let expected = format!("\x1d{word}\x1d {suffix}");
        assert_eq!(
            &result, &expected,
            "_word_ followed by space must become italic"
        );
    }

    /// `resolve_mentions` must pass through text that contains no `<...>`
    /// patterns completely unchanged.
    #[hegel::test]
    fn resolve_mentions_no_angle_brackets_is_identity(tc: TestCase) {
        let text = tc.draw(gs::from_regex("[^<>]{0,200}"));
        let result = resolve_mentions(&text, &StubResolver);
        assert_eq!(
            &result, &text,
            "text without angle brackets must survive unchanged"
        );
    }
}
