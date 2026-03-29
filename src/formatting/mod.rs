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

// ---------------------------------------------------------------------------
// Shared: server-time formatting
// ---------------------------------------------------------------------------

/// Format a Unix timestamp (seconds + millis) as ISO 8601 UTC.
///
/// Output: `YYYY-MM-DDTHH:MM:SS.mmmZ`
#[must_use]
pub fn format_server_time(unix_secs: i64, millis: u32) -> String {
    use chrono::{DateTime, Utc};

    // Clamp to valid range to prevent overflow in the nanosecond conversion.
    let nanos = millis.min(999) * 1_000_000;
    let dt = DateTime::<Utc>::from_timestamp(unix_secs, nanos)
        .unwrap_or_else(|| DateTime::<Utc>::from_timestamp(0, 0).expect("epoch is valid"));
    dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

// ---------------------------------------------------------------------------
// Re-exports — preserve the public API of `crate::formatting::*`
// ---------------------------------------------------------------------------

pub use discord_to_irc::{
    DiscordResolver, discord_to_irc, markdown_to_irc, resolve_mentions, split_for_irc,
};
pub use irc_to_discord::{
    IrcMentionResolver, convert_irc_mentions, irc_to_discord_formatting, irc_to_discord_plain,
    irc_to_discord_webhook, ping_fix_nick, truncate_for_discord,
};

// ---------------------------------------------------------------------------
// Tests (shared / server-time)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
    fn server_time_far_future() {
        assert_eq!(
            format_server_time(4_102_444_800, 0),
            "2100-01-01T00:00:00.000Z"
        );
    }

    use proptest::prelude::*;

    proptest! {
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

        /// Out-of-range millis values must not panic (clamped to 999).
        #[test]
        fn server_time_out_of_range_millis_does_not_panic(
            secs in 0i64..4_102_444_800i64,
            millis in 1000u32..=u32::MAX,
        ) {
            let formatted = format_server_time(secs, millis);
            assert!(formatted.ends_with('Z'));
            assert_eq!(formatted.len(), 24);
        }
    }

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
