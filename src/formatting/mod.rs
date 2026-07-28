//! Message formatting transformations between Discord and IRC.
//!
//! See `specs/05-formatting.md` for the full specification.
//!
//! This module is split by direction:
//! - [`discord_to_irc`] — Discord markdown → IRC control codes, mention
//!   resolution, and line splitting.
//! - [`irc_to_discord`] — IRC control codes → Discord markdown, mention
//!   conversion, ping-fix, and truncation.

mod discord_to_irc;
mod irc_to_discord;

#[cfg(test)]
pub(crate) mod test_support;

// ---------------------------------------------------------------------------
// Shared IRC control characters
// ---------------------------------------------------------------------------

pub(crate) const IRC_BOLD: char = '\x02';
pub(crate) const IRC_ITALIC: char = '\x1d';
pub(crate) const IRC_UNDERLINE: char = '\x1f';
pub(crate) const IRC_STRIKETHROUGH: char = '\x1e';
pub(crate) const IRC_REVERSE: char = '\x16';
pub(crate) const IRC_COLOR: char = '\x03';
pub(crate) const IRC_RESET: char = '\x0f';

pub(crate) use discord_to_irc::{DiscordResolver, discord_to_irc};
pub(crate) use irc_to_discord::{
    IrcMentionResolver, convert_irc_mentions, convert_nick_colon_mention,
    irc_to_discord_formatting, ping_fix_nick, truncate_for_discord,
};

// ---------------------------------------------------------------------------
// Tests (cross-direction roundtrip)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::discord_to_irc::markdown_to_irc;
    use super::*;

    use proptest::prelude::*;

    // -- Cross-direction roundtrip tests -------------------------------------

    /// A segment of text that can be either plain or formatted.
    #[derive(Debug, Clone)]
    enum FormattedSegment {
        Plain(String),
        Bold(String),
        Italic(String),
        Underline(String),
    }

    /// Strategy for plain text that won't be misinterpreted by either
    /// conversion direction: no markdown markers, no IRC control chars.
    fn safe_plain_text() -> impl Strategy<Value = String> {
        prop::string::string_regex("[a-zA-Z0-9 ,.!?;:()+=&%-]{1,20}").expect("valid regex")
    }

    /// Strategy generating Discord markdown text that losslessly round-trips
    /// through `markdown_to_irc` → `irc_to_discord_formatting`.
    fn roundtrip_discord_segments() -> impl Strategy<Value = Vec<FormattedSegment>> {
        prop::collection::vec(
            prop::strategy::Union::new(vec![
                safe_plain_text().prop_map(FormattedSegment::Plain).boxed(),
                safe_plain_text().prop_map(FormattedSegment::Bold).boxed(),
                safe_plain_text().prop_map(FormattedSegment::Italic).boxed(),
                safe_plain_text()
                    .prop_map(FormattedSegment::Underline)
                    .boxed(),
            ]),
            1..8,
        )
    }

    fn segments_to_discord(segments: &[FormattedSegment]) -> String {
        let mut parts = Vec::new();
        for seg in segments {
            match seg {
                FormattedSegment::Plain(t) => parts.push(t.clone()),
                FormattedSegment::Bold(t) => parts.push(format!("**{t}**")),
                FormattedSegment::Italic(t) => parts.push(format!("*{t}*")),
                FormattedSegment::Underline(t) => parts.push(format!("__{t}__")),
            }
        }
        parts.join(" ")
    }

    fn segments_to_irc(segments: &[FormattedSegment]) -> String {
        let mut parts = Vec::new();
        for seg in segments {
            match seg {
                FormattedSegment::Plain(t) => parts.push(t.clone()),
                FormattedSegment::Bold(t) => {
                    parts.push(format!("{IRC_BOLD}{t}{IRC_BOLD}"));
                }
                FormattedSegment::Italic(t) => {
                    parts.push(format!("{IRC_ITALIC}{t}{IRC_ITALIC}"));
                }
                FormattedSegment::Underline(t) => {
                    parts.push(format!("{IRC_UNDERLINE}{t}{IRC_UNDERLINE}"));
                }
            }
        }
        parts.join(" ")
    }

    proptest! {
        /// Discord → IRC → Discord round-trip: formatting should survive
        /// losslessly when using only bijective markers (**, *, __).
        #[test]
        fn discord_irc_discord_roundtrip(segments in roundtrip_discord_segments()) {
            let discord_text = segments_to_discord(&segments);
            let irc_text = markdown_to_irc(&discord_text);
            let back_to_discord = irc_to_discord_formatting(&irc_text);
            assert_eq!(
                back_to_discord, discord_text,
                "Round-trip failed:\n  discord: {discord_text:?}\n  irc:     {irc_text:?}\n  back:    {back_to_discord:?}"
            );
        }

        /// IRC → Discord → IRC round-trip: formatting should survive
        /// losslessly when using only bijective control codes (\x02, \x1d, \x1f).
        #[test]
        fn irc_discord_irc_roundtrip(segments in roundtrip_discord_segments()) {
            let irc_text = segments_to_irc(&segments);
            let discord_text = irc_to_discord_formatting(&irc_text);
            let back_to_irc = markdown_to_irc(&discord_text);
            assert_eq!(
                back_to_irc, irc_text,
                "Round-trip failed:\n  irc:     {irc_text:?}\n  discord: {discord_text:?}\n  back:    {back_to_irc:?}"
            );
        }
    }
}
