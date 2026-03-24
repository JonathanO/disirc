# Discord Markdown Parsing

## Summary

Discord uses a fork of Khan Academy's **simple-markdown** library as its parsing foundation. The parser is rule-based (regex match + parse + output), not spec-compliant with any standard markdown specification. Discord has heavily customized the rule set over the years, and parsing behaviour varies across surfaces (desktop, mobile, embeds). Neither serenity (Rust) nor discord.js/discord.py ship a markdown *parser*; they only provide *formatter* helpers for generating markdown strings.

## Findings

### 1. Parser foundation: simple-markdown

Discord forked Khan Academy's `simple-markdown` library and hosts it at `github.com/discord/simple-markdown`. The library uses a three-phase architecture:

1. **Match** — each rule has a regex (anchored with `^`) that attempts to match at the current position in the source string.
2. **Parse** — the matched text is converted into an AST node; the parser recurses into nested content.
3. **Output** — AST nodes are transformed into the target format (React elements on web, `Spannable` on Android, etc.).

Rules are evaluated in priority order. The parser consumes input left-to-right, greedily applying the first matching rule. This is fundamentally different from CommonMark/GFM which use a two-pass approach (block structure first, then inline parsing).

On Android, Discord built **SimpleAST**, a JVM reimplementation of the same rule system, because the JavaScript parser couldn't run natively. SimpleAST uses an explicit stack instead of recursion to prevent stack overflow on deeply nested content.

Discord's blog post on Android rendering confirms they achieved a 2.4x speedup by reusing `Matcher` instances via `.reset()` rather than creating new `Pattern.matcher()` calls per rule per position.

### 2. No formal specification

Discord does not implement any pre-existing markdown standard (CommonMark, GFM, etc.). There is no formal specification of Discord's markdown dialect. The official documentation is limited to a support article ("Markdown Text 101") that lists basic syntax but omits edge cases, nesting rules, and interaction between formatting types.

Community-maintained references exist (see References below) but are reverse-engineered and may become outdated as Discord changes its parser.

### 3. Discord's supported markdown syntax

| Syntax | Meaning |
|--------|---------|
| `*text*` | Italic |
| `_text_` | Italic |
| `**text**` | Bold |
| `__text__` | Underline (NOT bold, unlike standard markdown) |
| `~~text~~` | Strikethrough |
| `\|\|text\|\|` | Spoiler |
| `` `code` `` | Inline code |
| ` ```lang\ncode\n``` ` | Code block with optional syntax highlighting |
| `> text` | Single-line block quote |
| `>>> text` | Multi-line block quote (everything after) |
| `# heading` through `#### heading` | Headings (levels 1-4, must be at message start or after newline) |
| `-# text` | Subtext (small text, must start the line) |
| `[text](url)` | Masked link (limited contexts) |
| `<url>` | Suppress embed for URL |

Key divergences from standard markdown:
- `__text__` is **underline**, not bold/strong. This is the single biggest difference from CommonMark/GFM.
- `_text_` is italic, same as `*text*`.
- No support for `[link](url)` in all contexts (works in embeds and some surfaces, not reliably in plain messages).
- No images, tables, horizontal rules, or reference links.
- Headings must start at the beginning of a line; a heading mid-sentence does not render.
- Lists require a blank line before them or they may not render.
- Stacking/nesting block quotes is not allowed.

### 4. Parsing order and nesting

Discord resolves formatting markers in this priority order:

1. Code blocks (``` ``` ```) — highest priority, nothing inside is parsed
2. Inline code (`` ` ``) — nothing inside is parsed
3. Spoiler (`||`)
4. Underline (`__`)
5. Bold (`**`)
6. Italic (`*` or `_`)
7. Strikethrough (`~~`)

Practical consequences:
- Code blocks and inline code suppress ALL other formatting inside them. Spoiler markers inside code are literal.
- Spoilers suppress formatting inside them on some surfaces but not others (inconsistent).
- `***text***` renders as bold+italic (the `**` and `*` combine).
- `___text___` renders as underline+italic (the `__` and `_` combine).
- Formatting markers must be properly nested. `**bold _bold+italic**_` may not render correctly because the italic close is outside the bold close.

### 5. Edge cases relevant to a bridge

**Unmatched markers**: If a `*` or `**` has no matching close, Discord treats it as literal text. The entire marker is shown as-is. This means our parser must handle partial/unmatched markers gracefully.

**Intraword emphasis**: Discord does NOT treat `foo_bar_baz` as containing italic text. The `_` must be at a word boundary (or the start/end of the text). However, `*` is more permissive — `foo*bar*baz` does render the middle as italic.

**Code block language tag**: The language identifier after ``` is on the same line, no space required. Everything until the closing ``` is treated as preformatted text. If there is no closing ```, the entire rest of the message is treated as a code block.

**Newlines in formatting**: Formatting markers can span newlines in some cases (e.g., `**bold\nstill bold**`), but this is inconsistent across surfaces.

**Escape sequences**: Backslash (`\`) before a markdown character prevents it from being parsed as formatting. `\*not italic\*` renders as `*not italic*`.

**Empty formatting**: `****` (bold with empty content) renders as literal asterisks. Formatting markers with no content between them are treated as literal text.

**Nested code blocks**: Triple backticks inside triple backticks — the first closing ``` ends the block. There is no way to nest code blocks.

### 6. Client library formatting utilities

**serenity (Rust)**: Does NOT include a markdown parser. The `utils` module provides:
- `MessageBuilder` — ergonomic message construction (append text, mentions, formatting)
- `content_safe()` — strips mentions, resolving them to plaintext
- `parse_emoji()` — extracts emoji name/ID from mention syntax
- No AST-based markdown parsing or markdown-to-X conversion.

**discord.js**: The `@discordjs/formatters` package provides *generator* functions only:
- `bold()`, `italic()`, `underline()`, `strikethrough()`, `spoiler()`
- `inlineCode()`, `codeBlock(language, code)`
- `quote()`, `blockQuote()`, `subtext()`
- `hyperlink()`, `hideLinkEmbed()`
- `userMention()`, `channelMention()`, `roleMention()`
- `time()` with `TimestampStyles`
- No parsing of markdown text into structured data.

**discord.py**: No built-in markdown parser. Third-party package `discord-markdown-ast-parser` on PyPI provides an AST parser.

### 7. Rust crates for Discord markdown

**discord-md** (crates.io, v3.0.0, Nov 2023):
- Parser + builder for Discord markdown.
- Parses into an AST of `MarkdownElement` variants.
- Known limitations: block quotes not parsed (treated as plain text); nested emphasis may not parse correctly; intraword `_` incorrectly treated as emphasis; escape sequences treated as plain text.
- Most actively maintained of the Rust options.

**discord-markdown** (crates.io, v0.1.2):
- Parses Discord markdown into AST, can convert to HTML.
- Supports bold, italic, blockquotes, spoilers, code, mentions, custom emoji.
- Very early stage (7 commits, no published releases beyond 0.1.2).

**Neither crate is production-ready** for a bridge. Both have known parsing inaccuracies compared to Discord's actual client behaviour. For disirc, the existing hand-rolled regex approach in `src/formatting.rs` is likely more reliable since it can be tuned to match exactly the subset of transformations the spec requires, and it avoids adding a dependency with known bugs.

### 8. Discord-specific non-markdown syntax

These are not markdown but are parsed by Discord's message renderer and relevant to a bridge:

| Syntax | Meaning |
|--------|---------|
| `<@USER_ID>` | User mention |
| `<@!USER_ID>` | User mention (legacy nickname format) |
| `<#CHANNEL_ID>` | Channel mention |
| `<@&ROLE_ID>` | Role mention |
| `<:name:ID>` | Custom emoji |
| `<a:name:ID>` | Animated custom emoji |
| `<t:UNIX:STYLE>` | Timestamp (style: t, T, d, D, f, F, R) |

These use angle-bracket syntax and are NOT markdown — they are Discord-specific entity references that the client resolves at render time.

## References

- [discord/simple-markdown](https://github.com/discord/simple-markdown) — Discord's fork of Khan Academy's simple-markdown library
- [How Discord Renders Rich Messages on the Android App](https://discord.com/blog/how-discord-renders-rich-messages-on-the-android-app) — Discord engineering blog post on SimpleAST and Android rendering
- [Discord Markdown Part 1: Why It Sucks](https://wnelson.dev/blog/2024/12/discord-markdown-part-1/) — Detailed technical analysis of Discord's parser quirks
- [A (hopefully) complete guide to discord markdown](https://gist.github.com/Evitonative/7d1d1002ed3d597515261f341fac57b0) — Community-maintained comprehensive reference
- [Markdown Text 101](https://support.discord.com/hc/en-us/articles/210298617-Markdown-Text-101-Chat-Formatting-Bold-Italic-Underline) — Official Discord support article
- [discord-md crate](https://crates.io/crates/discord-md) — Rust parser/builder for Discord markdown (v3.0.0)
- [discord-markdown crate](https://github.com/cubetastic33/discord-markdown) — Rust parser for Discord markdown (v0.1.2)
- [serenity::utils](https://docs.rs/serenity/latest/serenity/utils/index.html) — Serenity formatting utilities documentation
- [discord.js Formatters](https://discordjs.guide/popular-topics/formatters) — discord.js built-in formatter functions
- [discord-markdown-parser (npm)](https://www.npmjs.com/package/discord-markdown-parser) — Node.js parser based on simple-markdown
