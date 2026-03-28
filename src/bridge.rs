use chrono::{DateTime, Utc};

use crate::config::BridgeEntry;
use crate::discord::DiscordCommand;
use crate::formatting::{DiscordResolver, IrcMentionResolver};
use crate::irc::{S2SCommand, S2SEvent};
use crate::pseudoclients::PseudoclientManager;

// ---------------------------------------------------------------------------
// BridgeMap
// ---------------------------------------------------------------------------

/// Immutable snapshot of one bridge entry as seen by the routing layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeInfo {
    /// Discord channel ID (numeric).
    pub discord_channel_id: u64,
    /// IRC channel name (e.g. `#general`).
    pub irc_channel: String,
    /// Optional webhook URL for the preferred send path.
    pub webhook_url: Option<String>,
}

/// Bidirectional channel routing table.
///
/// Built from `[[bridge]]` config entries.  Provides O(1) lookups in both
/// directions.  Replaced atomically on config reload — the processing task
/// swaps the whole map rather than mutating it in place.
#[derive(Debug, Default, Clone)]
pub struct BridgeMap {
    /// Discord channel ID → bridge info.
    by_discord: std::collections::HashMap<u64, BridgeInfo>,
    /// IRC channel name (lowercased) → discord channel ID (for reverse lookup).
    by_irc: std::collections::HashMap<String, u64>,
}

impl BridgeMap {
    /// Build a `BridgeMap` from a slice of config bridge entries.
    ///
    /// Entries with an unparseable `discord_channel_id` are silently skipped
    /// (config validation should have already rejected them).
    #[must_use]
    pub fn from_config(bridges: &[BridgeEntry]) -> Self {
        let mut map = Self::default();
        for entry in bridges {
            let Ok(discord_id) = entry.discord_channel_id.parse::<u64>() else {
                continue;
            };
            let info = BridgeInfo {
                discord_channel_id: discord_id,
                irc_channel: entry.irc_channel.clone(),
                webhook_url: entry.webhook_url.clone(),
            };
            map.by_irc
                .insert(entry.irc_channel.to_lowercase(), discord_id);
            map.by_discord.insert(discord_id, info);
        }
        map
    }

    /// Look up a bridge by Discord channel ID.
    #[must_use]
    pub fn by_discord_id(&self, id: u64) -> Option<&BridgeInfo> {
        self.by_discord.get(&id)
    }

    /// Look up a bridge by IRC channel name (case-insensitive).
    #[must_use]
    pub fn by_irc_channel(&self, channel: &str) -> Option<&BridgeInfo> {
        self.by_irc
            .get(&channel.to_lowercase())
            .and_then(|id| self.by_discord.get(id))
    }
}

// ---------------------------------------------------------------------------
// Discord → IRC relay
// ---------------------------------------------------------------------------

/// Build the `S2SCommand`s needed to relay a Discord message to IRC.
///
/// Returns an empty vec if the message should be skipped:
/// - Content is whitespace-only **and** there are no attachments.
///
/// Each formatted text line becomes one `S2SCommand::SendMessage`.
/// Attachment URLs follow as additional `SendMessage` commands, in order.
pub fn discord_to_irc_commands(
    uid: &str,
    irc_channel: &str,
    content: &str,
    attachments: &[String],
    timestamp: Option<DateTime<Utc>>,
    resolver: &dyn DiscordResolver,
) -> Vec<S2SCommand> {
    let trimmed = content.trim();
    if trimmed.is_empty() && attachments.is_empty() {
        return vec![];
    }

    let mut commands = Vec::new();

    // Text lines (skipped if content is whitespace-only)
    if !trimmed.is_empty() {
        let lines = crate::formatting::discord_to_irc(content, resolver);
        for line in lines {
            commands.push(S2SCommand::SendMessage {
                from_uid: uid.to_string(),
                target: irc_channel.to_string(),
                text: line,
                timestamp,
            });
        }
    }

    // Attachment URLs (one PRIVMSG each)
    for url in attachments {
        commands.push(S2SCommand::SendMessage {
            from_uid: uid.to_string(),
            target: irc_channel.to_string(),
            text: url.clone(),
            timestamp,
        });
    }

    commands
}

// ---------------------------------------------------------------------------
// IRC → Discord relay
// ---------------------------------------------------------------------------

/// Extract the CTCP ACTION body from a PRIVMSG text, if present.
///
/// Returns `Some(body)` for text of the form `\x01ACTION <body>[\x01]`.
fn extract_action(text: &str) -> Option<&str> {
    text.strip_prefix("\x01ACTION ")
        .map(|s| s.strip_suffix('\x01').unwrap_or(s))
}

/// Build the `DiscordCommand` needed to relay an IRC message to Discord.
///
/// Handles three IRC event types:
/// - `PRIVMSG` (`is_notice = false`, no CTCP prefix) — normal message via
///   webhook or plain channel send.
/// - `NOTICE` (`is_notice = true`) — text wrapped in `*…*` (italic) then
///   sent via webhook or plain channel send.
/// - `ACTION` (`PRIVMSG` whose text starts with `\x01ACTION `) — formatted
///   as `* nick body` regardless of send path.
///
/// Ping-fix (`U+200B` after the first character) is applied to the nick
/// wherever it appears as a label: webhook username, `**[nick]**` prefix,
/// and the `* nick` action prefix.
pub fn irc_to_discord_command(
    channel_id: u64,
    webhook_url: Option<&str>,
    sender_nick: &str,
    text: &str,
    is_notice: bool,
    resolver: &dyn IrcMentionResolver,
) -> DiscordCommand {
    use crate::formatting::{
        convert_irc_mentions, irc_to_discord_formatting, ping_fix_nick, truncate_for_discord,
    };

    let use_webhook = webhook_url.is_some();
    let ping_fixed_nick = ping_fix_nick(sender_nick);

    let (text_body, nick_field) = if let Some(action_body) = extract_action(text) {
        // CTCP ACTION (/me): "* nick action_body"
        let fmt = irc_to_discord_formatting(action_body);
        let with_mentions = convert_irc_mentions(&fmt, resolver);
        let full = format!("* {ping_fixed_nick} {with_mentions}");
        let body = truncate_for_discord(&full).into_owned();
        (body, ping_fixed_nick)
    } else {
        // Regular PRIVMSG or NOTICE
        let fmt = irc_to_discord_formatting(text);
        let with_mentions = convert_irc_mentions(&fmt, resolver);
        let content = if is_notice {
            format!("*{with_mentions}*")
        } else {
            with_mentions
        };

        if use_webhook {
            let body = truncate_for_discord(&content).into_owned();
            (body, ping_fixed_nick)
        } else {
            // Plain path: embed nick in the message text as "**[nick]** content"
            let plain = format!("**[{ping_fixed_nick}]** {content}");
            let body = truncate_for_discord(&plain).into_owned();
            (body, sender_nick.to_string())
        }
    };

    DiscordCommand::SendMessage {
        channel_id,
        webhook_url: webhook_url.map(str::to_string),
        sender_nick: nick_field,
        text: text_body,
    }
}

// ---------------------------------------------------------------------------
// IRC lifecycle state
// ---------------------------------------------------------------------------

/// Mutable IRC-side state maintained by the bridge processing task.
///
/// Tracks the uid→nick map for all external IRC users and the creation
/// timestamp of every channel we have seen in a `ChannelBurst`.  Both tables
/// are cleared on `LinkDown` / `PseudoclientManager::reset`.
#[derive(Debug, Default)]
pub struct IrcState {
    /// uid → current nick for every non-pseudoclient IRC user.
    nicks: std::collections::HashMap<String, String>,
    /// channel name (lowercased) → creation timestamp.
    channel_ts: std::collections::HashMap<String, u64>,
}

impl IrcState {
    /// Look up the current nick for a UID.
    #[must_use]
    pub fn nick_of(&self, uid: &str) -> Option<&str> {
        self.nicks.get(uid).map(String::as_str)
    }

    /// Look up the stored creation timestamp for a channel.
    #[must_use]
    pub fn ts_for_channel(&self, channel: &str) -> Option<u64> {
        self.channel_ts.get(&channel.to_lowercase()).copied()
    }

    /// Reset all tracked state (call on link loss).
    pub fn reset(&mut self) {
        self.nicks.clear();
        self.channel_ts.clear();
    }
}

/// Apply one `S2SEvent` to the bridge's IRC-side state.
///
/// Updates `state` and `pm` in place; never fails.  Events that carry no
/// meaningful state update (e.g. `LinkUp`, `BurstComplete`, message events)
/// are accepted and silently ignored so the caller can forward every event
/// here without filtering.
pub fn apply_irc_event(state: &mut IrcState, pm: &mut PseudoclientManager, event: &S2SEvent) {
    match event {
        S2SEvent::LinkDown { .. } => {
            state.reset();
            pm.reset();
        }

        S2SEvent::UserIntroduced { uid, nick, .. } => {
            state.nicks.insert(uid.clone(), nick.clone());
        }

        S2SEvent::UserNickChanged { uid, new_nick } => {
            if let Some(entry) = state.nicks.get_mut(uid) {
                entry.clone_from(new_nick);
            }
        }

        S2SEvent::UserQuit { uid, .. } => {
            state.nicks.remove(uid);
        }

        S2SEvent::ChannelBurst { channel, ts, .. } => {
            state
                .channel_ts
                .entry(channel.to_lowercase())
                .or_insert(*ts);
        }

        S2SEvent::NickForced { uid, new_nick } => {
            // Update our nick map for external users.
            if let Some(entry) = state.nicks.get_mut(uid) {
                entry.clone_from(new_nick);
            }
            // If the target is one of our pseudoclients, update its internal state.
            pm.apply_svsnick(uid, new_nick);
        }

        S2SEvent::UserParted { uid, channel, .. } => {
            // If one of our pseudoclients was parted, update its channel list.
            let did = pm.get_by_uid(uid).map(|s| s.discord_user_id);
            if let Some(did) = did {
                pm.part_channel(did, channel, "");
            }
        }

        S2SEvent::UserKicked { uid, channel, .. } => {
            // If one of our pseudoclients was kicked, update its channel list.
            let did = pm.get_by_uid(uid).map(|s| s.discord_user_id);
            if let Some(did) = did {
                pm.part_channel(did, channel, "");
            }
        }

        // These events require no IrcState / PseudoclientManager update.
        S2SEvent::LinkUp
        | S2SEvent::BurstComplete
        | S2SEvent::ServerIntroduced { .. }
        | S2SEvent::ServerQuit { .. }
        | S2SEvent::MessageReceived { .. }
        | S2SEvent::NoticeReceived { .. }
        | S2SEvent::AwaySet { .. }
        | S2SEvent::AwayCleared { .. } => {}
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use crate::formatting::DiscordResolver;
    use crate::irc::S2SCommand;

    use crate::discord::DiscordCommand;
    use crate::formatting::IrcMentionResolver;
    use crate::irc::S2SEvent;
    use crate::pseudoclients::PseudoclientManager;

    struct NullResolver;
    impl DiscordResolver for NullResolver {
        fn resolve_user(&self, _: &str) -> Option<String> {
            None
        }
        fn resolve_channel(&self, _: &str) -> Option<String> {
            None
        }
        fn resolve_role(&self, _: &str) -> Option<String> {
            None
        }
    }

    struct NullIrcResolver;
    impl IrcMentionResolver for NullIrcResolver {
        fn resolve_nick(&self, _: &str) -> Option<String> {
            None
        }
    }

    // --- discord_to_irc_commands ---

    #[test]
    fn empty_content_no_attachments_returns_empty() {
        let cmds = discord_to_irc_commands("uid1", "#chan", "", &[], None, &NullResolver);
        assert!(cmds.is_empty());
    }

    #[test]
    fn whitespace_only_content_no_attachments_returns_empty() {
        let cmds = discord_to_irc_commands("uid1", "#chan", "   \n  ", &[], None, &NullResolver);
        assert!(cmds.is_empty());
    }

    #[test]
    fn simple_text_produces_one_privmsg() {
        let cmds = discord_to_irc_commands("uid1", "#chan", "hello", &[], None, &NullResolver);
        assert_eq!(cmds.len(), 1);
        assert!(matches!(
            &cmds[0],
            S2SCommand::SendMessage { from_uid, target, text, timestamp: None }
            if from_uid == "uid1" && target == "#chan" && text == "hello"
        ));
    }

    #[test]
    fn attachment_only_produces_one_privmsg() {
        let urls = vec!["https://cdn.discord.com/x.png".to_string()];
        let cmds = discord_to_irc_commands("uid1", "#chan", "", &urls, None, &NullResolver);
        assert_eq!(cmds.len(), 1);
        assert!(matches!(
            &cmds[0],
            S2SCommand::SendMessage { text, .. } if text == "https://cdn.discord.com/x.png"
        ));
    }

    #[test]
    fn text_then_attachments_in_order() {
        let urls = vec![
            "https://cdn.discord.com/a.png".to_string(),
            "https://cdn.discord.com/b.png".to_string(),
        ];
        let cmds =
            discord_to_irc_commands("uid1", "#chan", "look at this", &urls, None, &NullResolver);
        assert_eq!(cmds.len(), 3);
        assert!(matches!(&cmds[0], S2SCommand::SendMessage { text, .. } if text == "look at this"));
        assert!(matches!(&cmds[1], S2SCommand::SendMessage { text, .. } if text.contains("a.png")));
        assert!(matches!(&cmds[2], S2SCommand::SendMessage { text, .. } if text.contains("b.png")));
    }

    #[test]
    fn multiline_content_produces_multiple_privmsgs() {
        let cmds = discord_to_irc_commands(
            "uid1",
            "#chan",
            "line one\nline two",
            &[],
            None,
            &NullResolver,
        );
        assert!(
            cmds.len() >= 2,
            "expected at least 2 commands, got {}",
            cmds.len()
        );
    }

    #[test]
    fn timestamp_is_propagated_to_all_commands() {
        use chrono::TimeZone;
        let ts = chrono::Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let urls = vec!["https://cdn.discord.com/x.png".to_string()];
        let cmds = discord_to_irc_commands("uid1", "#chan", "hi", &urls, Some(ts), &NullResolver);
        for cmd in &cmds {
            assert!(
                matches!(
                    cmd,
                    S2SCommand::SendMessage {
                        timestamp: Some(_),
                        ..
                    }
                ),
                "expected timestamp in {cmd:?}"
            );
        }
    }

    // --- BridgeMap ---

    fn entry(discord_id: &str, irc: &str, webhook: Option<&str>) -> BridgeEntry {
        BridgeEntry {
            discord_channel_id: discord_id.to_string(),
            irc_channel: irc.to_string(),
            webhook_url: webhook.map(str::to_string),
        }
    }

    #[test]
    fn lookup_by_discord_id_finds_entry() {
        let map = BridgeMap::from_config(&[entry("111", "#general", None)]);
        let info = map.by_discord_id(111).expect("should find entry");
        assert_eq!(info.irc_channel, "#general");
        assert_eq!(info.webhook_url, None);
    }

    #[test]
    fn lookup_by_irc_channel_finds_entry() {
        let map = BridgeMap::from_config(&[entry(
            "222",
            "#lobby",
            Some("https://discord.com/api/webhooks/99/tok"),
        )]);
        let info = map.by_irc_channel("#lobby").expect("should find entry");
        assert_eq!(info.discord_channel_id, 222);
        assert_eq!(
            info.webhook_url.as_deref(),
            Some("https://discord.com/api/webhooks/99/tok")
        );
    }

    #[test]
    fn irc_lookup_is_case_insensitive() {
        let map = BridgeMap::from_config(&[entry("333", "#General", None)]);
        assert!(map.by_irc_channel("#general").is_some());
        assert!(map.by_irc_channel("#GENERAL").is_some());
        assert!(map.by_irc_channel("#General").is_some());
    }

    #[test]
    fn unknown_discord_id_returns_none() {
        let map = BridgeMap::from_config(&[entry("111", "#general", None)]);
        assert!(map.by_discord_id(999).is_none());
    }

    #[test]
    fn unknown_irc_channel_returns_none() {
        let map = BridgeMap::from_config(&[entry("111", "#general", None)]);
        assert!(map.by_irc_channel("#other").is_none());
    }

    #[test]
    fn unparseable_discord_id_is_skipped() {
        let bad = BridgeEntry {
            discord_channel_id: "not-a-number".to_string(),
            irc_channel: "#test".to_string(),
            webhook_url: None,
        };
        let map = BridgeMap::from_config(&[bad]);
        assert!(map.by_irc_channel("#test").is_none());
    }

    #[test]
    fn multiple_entries_all_accessible() {
        let map = BridgeMap::from_config(&[
            entry("100", "#alpha", None),
            entry(
                "200",
                "#beta",
                Some("https://discord.com/api/webhooks/1/tok"),
            ),
        ]);
        assert!(map.by_discord_id(100).is_some());
        assert!(map.by_discord_id(200).is_some());
        assert!(map.by_irc_channel("#alpha").is_some());
        assert!(map.by_irc_channel("#beta").is_some());
    }

    #[test]
    fn from_config_empty_slice_gives_empty_map() {
        let map = BridgeMap::from_config(&[]);
        assert!(map.by_discord_id(1).is_none());
        assert!(map.by_irc_channel("#x").is_none());
    }

    // --- irc_to_discord_command ---

    #[test]
    fn privmsg_webhook_path_uses_webhook_url() {
        let cmd = irc_to_discord_command(
            42,
            Some("https://discord.com/api/webhooks/1/tok"),
            "alice",
            "hello",
            false,
            &NullIrcResolver,
        );
        assert!(matches!(
            &cmd,
            DiscordCommand::SendMessage { webhook_url: Some(u), .. } if u.contains("webhooks")
        ));
    }

    #[test]
    fn privmsg_plain_path_formats_with_bracket_prefix() {
        let cmd = irc_to_discord_command(42, None, "alice", "hello", false, &NullIrcResolver);
        assert!(matches!(
            &cmd,
            DiscordCommand::SendMessage { webhook_url: None, text, .. }
            if text.contains("**[") && text.contains(']') && text.contains("hello")
        ));
    }

    #[test]
    fn notice_webhook_path_wraps_in_italics() {
        let cmd = irc_to_discord_command(
            42,
            Some("https://discord.com/api/webhooks/1/tok"),
            "bob",
            "server restart soon",
            true,
            &NullIrcResolver,
        );
        assert!(matches!(
            &cmd,
            DiscordCommand::SendMessage { text, .. }
            if text.starts_with('*') && text.ends_with('*')
        ));
    }

    #[test]
    fn notice_plain_path_wraps_in_italics_and_has_bracket_prefix() {
        let cmd = irc_to_discord_command(42, None, "bob", "ping", true, &NullIrcResolver);
        assert!(matches!(
            &cmd,
            DiscordCommand::SendMessage { text, .. }
            if text.contains("**[") && text.contains("*ping*")
        ));
    }

    #[test]
    fn action_formats_as_star_nick_body() {
        let cmd = irc_to_discord_command(
            42,
            Some("https://discord.com/api/webhooks/1/tok"),
            "carol",
            "\x01ACTION waves\x01",
            false,
            &NullIrcResolver,
        );
        assert!(matches!(
            &cmd,
            DiscordCommand::SendMessage { text, .. } if text.contains("* ") && text.contains("waves")
        ));
        if let DiscordCommand::SendMessage { text, .. } = &cmd {
            assert!(
                text.starts_with("* "),
                "expected '* ' prefix, got: {text:?}"
            );
            assert!(text.contains("waves"), "action body missing");
        }
    }

    #[test]
    fn action_plain_path_same_star_format() {
        let cmd = irc_to_discord_command(
            42,
            None,
            "carol",
            "\x01ACTION waves\x01",
            false,
            &NullIrcResolver,
        );
        assert!(matches!(
            &cmd,
            DiscordCommand::SendMessage { text, .. }
            if text.starts_with("* ") && text.contains("waves")
        ));
    }

    #[test]
    fn action_without_trailing_ctcp_delimiter_still_detected() {
        let cmd = irc_to_discord_command(
            42,
            None,
            "dave",
            "\x01ACTION dances",
            false,
            &NullIrcResolver,
        );
        assert!(matches!(
            &cmd,
            DiscordCommand::SendMessage { text, .. }
            if text.starts_with("* ") && text.contains("dances")
        ));
    }

    #[test]
    fn ping_fix_applied_to_webhook_username() {
        let cmd = irc_to_discord_command(
            42,
            Some("https://discord.com/api/webhooks/1/tok"),
            "alice",
            "hi",
            false,
            &NullIrcResolver,
        );
        assert!(matches!(
            &cmd,
            DiscordCommand::SendMessage { sender_nick, .. }
            if sender_nick.contains('\u{200B}')
        ));
    }

    #[test]
    fn ping_fix_applied_to_nick_in_plain_path_text() {
        let cmd = irc_to_discord_command(42, None, "alice", "hi", false, &NullIrcResolver);
        assert!(matches!(
            &cmd,
            DiscordCommand::SendMessage { text, .. }
            if text.contains('\u{200B}')
        ));
    }

    #[test]
    fn plain_path_nick_field_is_unfixed_original() {
        let cmd = irc_to_discord_command(42, None, "alice", "hi", false, &NullIrcResolver);
        // For the plain path the send layer doesn't use sender_nick as a webhook
        // username, so we store the original nick (no ping-fix).
        assert!(matches!(
            &cmd,
            DiscordCommand::SendMessage { sender_nick, .. } if sender_nick == "alice"
        ));
    }

    #[test]
    fn channel_id_and_webhook_url_propagated() {
        let url = "https://discord.com/api/webhooks/99/secret";
        let cmd = irc_to_discord_command(1234, Some(url), "eve", "test", false, &NullIrcResolver);
        assert!(matches!(
            &cmd,
            DiscordCommand::SendMessage { channel_id: 1234, webhook_url: Some(u), .. }
            if u == url
        ));
    }

    #[test]
    fn irc_bold_formatting_converted_to_discord_bold() {
        let cmd = irc_to_discord_command(
            1,
            Some("https://discord.com/api/webhooks/1/t"),
            "nick",
            "\x02bold\x02",
            false,
            &NullIrcResolver,
        );
        assert!(matches!(
            &cmd,
            DiscordCommand::SendMessage { text, .. } if text.contains("**bold**")
        ));
    }

    // --- IrcState / apply_irc_event ---

    fn make_pm() -> PseudoclientManager {
        PseudoclientManager::new("001", "bridge", "users.example.com")
    }

    fn introduced(uid: &str, nick: &str) -> S2SEvent {
        S2SEvent::UserIntroduced {
            uid: uid.to_string(),
            nick: nick.to_string(),
            ident: "~u".to_string(),
            host: "host".to_string(),
            server_sid: "002".to_string(),
            realname: "Real Name".to_string(),
        }
    }

    #[test]
    fn user_introduced_adds_to_nick_map() {
        let mut state = IrcState::default();
        let mut pm = make_pm();
        apply_irc_event(&mut state, &mut pm, &introduced("001AAAAAA", "alice"));
        assert_eq!(state.nick_of("001AAAAAA"), Some("alice"));
    }

    #[test]
    fn user_nick_changed_updates_nick() {
        let mut state = IrcState::default();
        let mut pm = make_pm();
        apply_irc_event(&mut state, &mut pm, &introduced("001AAAAAA", "alice"));
        apply_irc_event(
            &mut state,
            &mut pm,
            &S2SEvent::UserNickChanged {
                uid: "001AAAAAA".to_string(),
                new_nick: "alice_".to_string(),
            },
        );
        assert_eq!(state.nick_of("001AAAAAA"), Some("alice_"));
    }

    #[test]
    fn user_quit_removes_from_nick_map() {
        let mut state = IrcState::default();
        let mut pm = make_pm();
        apply_irc_event(&mut state, &mut pm, &introduced("001AAAAAA", "alice"));
        apply_irc_event(
            &mut state,
            &mut pm,
            &S2SEvent::UserQuit {
                uid: "001AAAAAA".to_string(),
                reason: "Quit".to_string(),
            },
        );
        assert_eq!(state.nick_of("001AAAAAA"), None);
    }

    #[test]
    fn link_down_clears_nick_map() {
        let mut state = IrcState::default();
        let mut pm = make_pm();
        apply_irc_event(&mut state, &mut pm, &introduced("001AAAAAA", "alice"));
        apply_irc_event(
            &mut state,
            &mut pm,
            &S2SEvent::LinkDown {
                reason: "gone".to_string(),
            },
        );
        assert_eq!(state.nick_of("001AAAAAA"), None);
    }

    #[test]
    fn link_down_clears_channel_ts() {
        let mut state = IrcState::default();
        let mut pm = make_pm();
        apply_irc_event(
            &mut state,
            &mut pm,
            &S2SEvent::ChannelBurst {
                channel: "#general".to_string(),
                ts: 1_000,
                members: vec![],
            },
        );
        apply_irc_event(
            &mut state,
            &mut pm,
            &S2SEvent::LinkDown {
                reason: "gone".to_string(),
            },
        );
        assert_eq!(state.ts_for_channel("#general"), None);
    }

    #[test]
    fn channel_burst_stores_timestamp() {
        let mut state = IrcState::default();
        let mut pm = make_pm();
        apply_irc_event(
            &mut state,
            &mut pm,
            &S2SEvent::ChannelBurst {
                channel: "#general".to_string(),
                ts: 1_700_000_000,
                members: vec![],
            },
        );
        assert_eq!(state.ts_for_channel("#general"), Some(1_700_000_000));
    }

    #[test]
    fn channel_burst_does_not_overwrite_existing_ts() {
        let mut state = IrcState::default();
        let mut pm = make_pm();
        apply_irc_event(
            &mut state,
            &mut pm,
            &S2SEvent::ChannelBurst {
                channel: "#general".to_string(),
                ts: 1_000,
                members: vec![],
            },
        );
        apply_irc_event(
            &mut state,
            &mut pm,
            &S2SEvent::ChannelBurst {
                channel: "#general".to_string(),
                ts: 2_000,
                members: vec![],
            },
        );
        assert_eq!(state.ts_for_channel("#general"), Some(1_000));
    }

    #[test]
    fn channel_ts_lookup_is_case_insensitive() {
        let mut state = IrcState::default();
        let mut pm = make_pm();
        apply_irc_event(
            &mut state,
            &mut pm,
            &S2SEvent::ChannelBurst {
                channel: "#General".to_string(),
                ts: 42,
                members: vec![],
            },
        );
        assert_eq!(state.ts_for_channel("#general"), Some(42));
        assert_eq!(state.ts_for_channel("#GENERAL"), Some(42));
    }

    #[test]
    fn nick_forced_updates_external_nick() {
        let mut state = IrcState::default();
        let mut pm = make_pm();
        apply_irc_event(&mut state, &mut pm, &introduced("002BBBBBB", "bob"));
        apply_irc_event(
            &mut state,
            &mut pm,
            &S2SEvent::NickForced {
                uid: "002BBBBBB".to_string(),
                new_nick: "bob_".to_string(),
            },
        );
        assert_eq!(state.nick_of("002BBBBBB"), Some("bob_"));
    }

    #[test]
    fn nick_forced_updates_pseudoclient_nick() {
        let mut state = IrcState::default();
        let mut pm = make_pm();
        // Introduce a pseudoclient
        pm.introduce(
            99,
            "discorduser",
            "Discord User",
            &["#general".to_string()],
            1000,
        )
        .expect("introduce should succeed");
        let uid = pm.get_by_discord_id(99).expect("should exist").uid.clone();
        let orig_nick = pm.get_by_discord_id(99).expect("should exist").nick.clone();
        apply_irc_event(
            &mut state,
            &mut pm,
            &S2SEvent::NickForced {
                uid: uid.clone(),
                new_nick: "newnick".to_string(),
            },
        );
        let updated_nick = pm
            .get_by_discord_id(99)
            .expect("should still exist")
            .nick
            .clone();
        assert_ne!(updated_nick, orig_nick, "nick should have changed");
        assert_eq!(updated_nick, "newnick");
    }

    #[test]
    fn user_parted_removes_pseudoclient_from_channel() {
        let mut state = IrcState::default();
        let mut pm = make_pm();
        pm.introduce(77, "testuser", "Test User", &["#lobby".to_string()], 1000)
            .expect("introduce should succeed");
        let uid = pm.get_by_discord_id(77).expect("should exist").uid.clone();
        apply_irc_event(
            &mut state,
            &mut pm,
            &S2SEvent::UserParted {
                uid,
                channel: "#lobby".to_string(),
                reason: None,
            },
        );
        let channels = &pm.get_by_discord_id(77).map(|s| s.channels.clone());
        // After parting the only channel the pseudoclient should be removed entirely
        // (PseudoclientManager::part_channel returns Quit when no channels remain)
        assert!(
            channels.is_none()
                || channels
                    .as_ref()
                    .map_or(true, |c| !c.contains(&"#lobby".to_string())),
            "pseudoclient should no longer be in #lobby"
        );
    }

    #[test]
    fn user_kicked_removes_pseudoclient_from_channel() {
        let mut state = IrcState::default();
        let mut pm = make_pm();
        pm.introduce(
            88,
            "testuser2",
            "Test User 2",
            &["#kicked".to_string()],
            1000,
        )
        .expect("introduce should succeed");
        let uid = pm.get_by_discord_id(88).expect("should exist").uid.clone();
        apply_irc_event(
            &mut state,
            &mut pm,
            &S2SEvent::UserKicked {
                uid,
                channel: "#kicked".to_string(),
                by_uid: "003CCCCCC".to_string(),
                reason: "test".to_string(),
            },
        );
        let channels = &pm.get_by_discord_id(88).map(|s| s.channels.clone());
        assert!(
            channels.is_none()
                || channels
                    .as_ref()
                    .map_or(true, |c| !c.contains(&"#kicked".to_string())),
            "pseudoclient should no longer be in #kicked"
        );
    }

    #[test]
    fn user_parted_ignores_external_user() {
        // A PART from an external user (not our pseudoclient) should not crash.
        let mut state = IrcState::default();
        let mut pm = make_pm();
        apply_irc_event(&mut state, &mut pm, &introduced("002ZZZZZZ", "extern"));
        apply_irc_event(
            &mut state,
            &mut pm,
            &S2SEvent::UserParted {
                uid: "002ZZZZZZ".to_string(),
                channel: "#general".to_string(),
                reason: None,
            },
        );
        // Still trackable in nick map
        assert_eq!(state.nick_of("002ZZZZZZ"), Some("extern"));
    }

    #[test]
    fn link_down_resets_pseudoclient_manager() {
        let mut state = IrcState::default();
        let mut pm = make_pm();
        pm.introduce(55, "user55", "User 55", &["#test".to_string()], 1000)
            .expect("introduce should succeed");
        assert!(pm.get_by_discord_id(55).is_some());
        apply_irc_event(
            &mut state,
            &mut pm,
            &S2SEvent::LinkDown {
                reason: "down".to_string(),
            },
        );
        assert!(
            pm.get_by_discord_id(55).is_none(),
            "PseudoclientManager should be reset"
        );
    }
}
