use crate::discord::DiscordCommand;
use crate::formatting::{DiscordResolver, IrcMentionResolver};
use crate::irc::S2SCommand;
use crate::pseudoclients::{PseudoclientManager, sanitize_nick};

use super::map::BridgeMap;
use super::relay::{discord_to_irc_commands, irc_to_discord_command};
use super::state::{DiscordState, IrcState};

// ---------------------------------------------------------------------------
// Burst generation
// ---------------------------------------------------------------------------

/// Generate the burst `S2SCommand`s for all currently-known pseudoclients.
///
/// Each pseudoclient gets an `IntroduceUser` followed by one `JoinChannel`
/// per mapped IRC channel.  The sequence ends with `BurstComplete`.
/// Called on every `LinkUp` event.
pub fn produce_burst_commands(
    pm: &PseudoclientManager,
    irc_state: &IrcState,
    now_ts: u64,
) -> Vec<S2SCommand> {
    let mut cmds = Vec::new();
    for state in pm.iter_states() {
        let host = format!(
            "{}.{}",
            sanitize_nick(&state.display_name),
            pm.host_suffix()
        );
        cmds.push(S2SCommand::IntroduceUser {
            uid: state.uid.clone(),
            nick: state.nick.clone(),
            ident: pm.ident().to_string(),
            host,
            realname: state.display_name.clone(),
        });
        for channel in &state.channels {
            cmds.push(S2SCommand::JoinChannel {
                uid: state.uid.clone(),
                channel: channel.clone(),
                ts: irc_state.ts_for_channel(channel).unwrap_or(now_ts),
            });
        }
    }
    cmds.push(S2SCommand::BurstComplete);
    cmds
}

/// Route one IRC message (PRIVMSG or NOTICE) to Discord.
///
/// Returns `None` when:
/// - `from_uid` is one of our own pseudoclients (loop prevention), or
/// - `target` is not a mapped IRC channel.
#[allow(clippy::too_many_arguments)]
pub fn route_irc_to_discord(
    pm: &PseudoclientManager,
    bridge_map: &BridgeMap,
    irc_state: &IrcState,
    from_uid: &str,
    target: &str,
    text: &str,
    is_notice: bool,
    resolver: &dyn IrcMentionResolver,
) -> Option<DiscordCommand> {
    if pm.is_our_uid(from_uid) {
        return None;
    }
    let bridge = bridge_map.by_irc_channel(target)?;
    let nick = irc_state.nick_of(from_uid).unwrap_or(from_uid);
    Some(irc_to_discord_command(
        bridge.discord_channel_id,
        bridge.webhook_url.as_deref(),
        nick,
        text,
        is_notice,
        resolver,
    ))
}

/// Route one Discord `MessageReceived` event to IRC.
///
/// Returns the `S2SCommand`s to send.  Introduces the pseudoclient on-demand
/// (to the single mapped channel) if it is not yet known to `pm`.
/// Returns an empty vec when `channel_id` is not mapped.
#[allow(clippy::too_many_arguments)]
pub fn route_discord_to_irc(
    pm: &mut PseudoclientManager,
    bridge_map: &BridgeMap,
    discord_state: &DiscordState,
    irc_state: &IrcState,
    channel_id: u64,
    author_id: u64,
    author_name: &str,
    content: &str,
    attachments: &[String],
    timestamp: Option<chrono::DateTime<chrono::Utc>>,
    now_ts: u64,
    resolver: &dyn DiscordResolver,
) -> Vec<S2SCommand> {
    let Some(bridge) = bridge_map.by_discord_id(channel_id) else {
        return vec![];
    };
    let irc_channel = bridge.irc_channel.clone();

    // On-demand introduction: ensure a pseudoclient exists for this author.
    let mut cmds = Vec::new();
    if pm.get_by_discord_id(author_id).is_none() {
        let display_name = discord_state
            .display_names
            .get(&author_id)
            .cloned()
            .unwrap_or_else(|| author_name.to_string());
        let channels = vec![irc_channel.clone()];
        let ts = irc_state.ts_for_channel(&irc_channel).unwrap_or(now_ts);
        pm.introduce(author_id, &display_name, &display_name, &channels, ts);

        // Emit the S2S commands so the IRC server learns about this user.
        if let Some(state) = pm.get_by_discord_id(author_id) {
            let host = format!("{}.{}", sanitize_nick(&display_name), pm.host_suffix());
            cmds.push(S2SCommand::IntroduceUser {
                uid: state.uid.clone(),
                nick: state.nick.clone(),
                ident: pm.ident().to_string(),
                host,
                realname: display_name,
            });
            for channel in &state.channels {
                cmds.push(S2SCommand::JoinChannel {
                    uid: state.uid.clone(),
                    channel: channel.clone(),
                    ts,
                });
            }
        }
    }

    let uid = match pm.get_by_discord_id(author_id) {
        Some(s) => s.uid.clone(),
        None => return vec![],
    };

    cmds.extend(discord_to_irc_commands(
        &uid,
        &irc_channel,
        content,
        attachments,
        timestamp,
        resolver,
    ));
    cmds
}

/// Update `discord_state.guild_irc_channels` from a `MemberSnapshot`'s
/// `channel_ids` list.
///
/// Called in the bridge loop before `apply_discord_event` so that
/// `apply_discord_event` can use the (now-current) guild→irc-channel map.
pub fn update_guild_irc_channels(
    discord_state: &mut DiscordState,
    bridge_map: &BridgeMap,
    guild_id: u64,
    channel_ids: &[u64],
) {
    let irc_channels: Vec<String> = channel_ids
        .iter()
        .filter_map(|&cid| {
            bridge_map
                .by_discord_id(cid)
                .map(|info| info.irc_channel.clone())
        })
        .collect();
    discord_state
        .guild_irc_channels
        .insert(guild_id, irc_channels);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::BridgeEntry;
    use crate::discord::DiscordCommand;
    use crate::formatting::DiscordResolver;
    use crate::irc::{S2SCommand, S2SEvent};
    use crate::pseudoclients::PseudoclientManager;

    use super::super::state::apply_irc_event;

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

    fn make_bridge_map() -> BridgeMap {
        BridgeMap::from_config(&[BridgeEntry {
            discord_channel_id: "111".to_string(),
            irc_channel: "#general".to_string(),
            webhook_url: None,
        }])
    }

    // --- produce_burst_commands ---

    #[test]
    fn burst_empty_pm_produces_only_burst_complete() {
        let pm = make_pm();
        let irc = IrcState::default();
        let cmds = produce_burst_commands(&pm, &irc, 1_000);
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], S2SCommand::BurstComplete));
    }

    #[test]
    fn burst_one_pseudoclient_produces_introduce_join_burst_complete() {
        let mut pm = make_pm();
        pm.introduce(42, "alice", "Alice", &["#general".to_string()], 500);
        let irc = IrcState::default();
        let cmds = produce_burst_commands(&pm, &irc, 1_000);
        // IntroduceUser, JoinChannel, BurstComplete
        assert_eq!(cmds.len(), 3);
        assert!(matches!(cmds[0], S2SCommand::IntroduceUser { .. }));
        assert!(matches!(cmds[1], S2SCommand::JoinChannel { .. }));
        assert!(matches!(cmds[2], S2SCommand::BurstComplete));
    }

    #[test]
    fn burst_uses_channel_ts_from_irc_state() {
        let mut pm = make_pm();
        pm.introduce(42, "alice", "Alice", &["#general".to_string()], 500);
        let mut irc = IrcState::default();
        apply_irc_event(
            &mut irc,
            &mut make_pm(),
            &S2SEvent::ChannelBurst {
                channel: "#general".to_string(),
                ts: 9_999,
                members: vec![],
            },
        );
        let cmds = produce_burst_commands(&pm, &irc, 1_000);
        let ts = cmds.iter().find_map(|c| {
            if let S2SCommand::JoinChannel { ts, .. } = c {
                Some(*ts)
            } else {
                None
            }
        });
        assert_eq!(ts, Some(9_999));
    }

    #[test]
    fn burst_falls_back_to_now_ts_when_channel_unknown() {
        let mut pm = make_pm();
        pm.introduce(42, "alice", "Alice", &["#unknown".to_string()], 500);
        let irc = IrcState::default();
        let cmds = produce_burst_commands(&pm, &irc, 7_777);
        let ts = cmds.iter().find_map(|c| {
            if let S2SCommand::JoinChannel { ts, .. } = c {
                Some(*ts)
            } else {
                None
            }
        });
        assert_eq!(ts, Some(7_777));
    }

    #[test]
    fn burst_last_command_is_always_burst_complete() {
        let mut pm = make_pm();
        pm.introduce(1, "a", "A", &["#c1".to_string(), "#c2".to_string()], 0);
        pm.introduce(2, "b", "B", &["#c1".to_string()], 0);
        let irc = IrcState::default();
        let cmds = produce_burst_commands(&pm, &irc, 0);
        assert!(matches!(cmds.last(), Some(S2SCommand::BurstComplete)));
    }

    // --- route_irc_to_discord ---

    #[test]
    fn route_irc_own_uid_returns_none() {
        let mut pm = make_pm();
        pm.introduce(42, "alice", "Alice", &["#general".to_string()], 0);
        let bridge_map = make_bridge_map();
        let irc = IrcState::default();
        // Find the UID that was assigned to alice
        let uid = pm
            .get_by_discord_id(42)
            .expect("alice should be introduced")
            .uid
            .clone();
        let result = route_irc_to_discord(
            &pm,
            &bridge_map,
            &irc,
            &uid,
            "#general",
            "hello",
            false,
            &NullIrcResolver,
        );
        assert!(result.is_none(), "own pseudoclient UID must be filtered");
    }

    #[test]
    fn route_irc_unknown_channel_returns_none() {
        let pm = make_pm();
        let bridge_map = make_bridge_map();
        let irc = IrcState::default();
        let result = route_irc_to_discord(
            &pm,
            &bridge_map,
            &irc,
            "002AAAAAB",
            "#notbridged",
            "hello",
            false,
            &NullIrcResolver,
        );
        assert!(result.is_none());
    }

    #[test]
    fn route_irc_known_channel_external_uid_returns_command() {
        let pm = make_pm();
        let bridge_map = make_bridge_map();
        let mut irc = IrcState::default();
        // Register an external user nick
        apply_irc_event(&mut irc, &mut make_pm(), &introduced("002AAAAAB", "bob"));
        let result = route_irc_to_discord(
            &pm,
            &bridge_map,
            &irc,
            "002AAAAAB",
            "#general",
            "hi there",
            false,
            &NullIrcResolver,
        );
        assert!(result.is_some());
        if let Some(DiscordCommand::SendMessage {
            channel_id,
            sender_nick,
            text,
            ..
        }) = result
        {
            assert_eq!(channel_id, 111);
            // Plain path (no webhook): nick is the original unfixed nick
            assert_eq!(sender_nick, "bob");
            // Plain path embeds nick in text; confirm the message body is present
            assert!(
                text.contains("hi there"),
                "text should contain the message body"
            );
        }
    }

    #[test]
    fn route_irc_unknown_uid_falls_back_to_uid_as_nick() {
        let pm = make_pm();
        let bridge_map = make_bridge_map();
        let irc = IrcState::default(); // no nick registered
        let result = route_irc_to_discord(
            &pm,
            &bridge_map,
            &irc,
            "002ZZZZZ",
            "#general",
            "msg",
            false,
            &NullIrcResolver,
        );
        if let Some(DiscordCommand::SendMessage { sender_nick, .. }) = result {
            assert_eq!(sender_nick, "002ZZZZZ");
        } else {
            panic!("expected a SendMessage command");
        }
    }

    // --- route_discord_to_irc ---

    #[test]
    fn route_discord_unmapped_channel_returns_empty() {
        let mut pm = make_pm();
        let bridge_map = make_bridge_map();
        let ds = DiscordState::default();
        let irc = IrcState::default();
        let cmds = route_discord_to_irc(
            &mut pm,
            &bridge_map,
            &ds,
            &irc,
            999, // not in bridge_map
            1,
            "alice",
            "hello",
            &[],
            None,
            0,
            &NullResolver,
        );
        assert!(cmds.is_empty());
    }

    #[test]
    fn route_discord_known_channel_returns_privmsg_commands() {
        let mut pm = make_pm();
        pm.introduce(42, "alice", "Alice", &["#general".to_string()], 0);
        let bridge_map = make_bridge_map();
        let ds = DiscordState::default();
        let irc = IrcState::default();
        let cmds = route_discord_to_irc(
            &mut pm,
            &bridge_map,
            &ds,
            &irc,
            111,
            42,
            "Alice",
            "hello",
            &[],
            None,
            0,
            &NullResolver,
        );
        assert!(!cmds.is_empty());
        assert!(matches!(
            &cmds[0],
            S2SCommand::SendMessage { target, text, .. }
            if target == "#general" && text == "hello"
        ));
    }

    #[test]
    fn route_discord_on_demand_introduction_when_author_unknown() {
        let mut pm = make_pm();
        let bridge_map = make_bridge_map();
        let ds = DiscordState::default();
        let irc = IrcState::default();
        // author_id 77 has no pseudoclient yet
        let cmds = route_discord_to_irc(
            &mut pm,
            &bridge_map,
            &ds,
            &irc,
            111,
            77,
            "newuser",
            "first message",
            &[],
            None,
            0,
            &NullResolver,
        );
        // Must produce at least a SendMessage (introduction happened internally)
        assert!(
            cmds.iter()
                .any(|c| matches!(c, S2SCommand::SendMessage { .. }))
        );
        // Pseudoclient must now be registered
        assert!(pm.get_by_discord_id(77).is_some());
    }

    #[test]
    fn route_discord_on_demand_uses_cached_display_name() {
        let mut pm = make_pm();
        let bridge_map = make_bridge_map();
        let mut ds = DiscordState::default();
        ds.display_names.insert(77, "CachedName".to_string());
        let irc = IrcState::default();
        route_discord_to_irc(
            &mut pm,
            &bridge_map,
            &ds,
            &irc,
            111,
            77,
            "fallback_name",
            "hi",
            &[],
            None,
            0,
            &NullResolver,
        );
        let state = pm.get_by_discord_id(77).expect("should be introduced");
        assert_eq!(state.display_name, "CachedName");
    }

    #[test]
    fn route_discord_on_demand_emits_introduce_and_join_commands() {
        let mut pm = make_pm();
        let bridge_map = make_bridge_map();
        let ds = DiscordState::default();
        let irc = IrcState::default();
        // author_id 77 has no pseudoclient — on-demand introduction should
        // produce IntroduceUser + JoinChannel commands alongside SendMessage.
        let cmds = route_discord_to_irc(
            &mut pm,
            &bridge_map,
            &ds,
            &irc,
            111,
            77,
            "newuser",
            "first message",
            &[],
            None,
            0,
            &NullResolver,
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, S2SCommand::IntroduceUser { .. })),
            "on-demand introduction must emit IntroduceUser; got: {cmds:?}"
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, S2SCommand::JoinChannel { .. })),
            "on-demand introduction must emit JoinChannel; got: {cmds:?}"
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, S2SCommand::SendMessage { .. })),
            "message must still be sent; got: {cmds:?}"
        );
    }

    // --- update_guild_irc_channels ---

    #[test]
    fn update_guild_irc_channels_maps_known_discord_ids() {
        let mut ds = DiscordState::default();
        let bridge_map = make_bridge_map(); // 111 -> #general
        update_guild_irc_channels(&mut ds, &bridge_map, 5, &[111]);
        assert_eq!(
            ds.guild_irc_channels.get(&5),
            Some(&vec!["#general".to_string()])
        );
    }

    #[test]
    fn update_guild_irc_channels_filters_unknown_discord_ids() {
        let mut ds = DiscordState::default();
        let bridge_map = make_bridge_map(); // only 111 is mapped
        update_guild_irc_channels(&mut ds, &bridge_map, 5, &[999]);
        assert_eq!(ds.guild_irc_channels.get(&5), Some(&vec![] as &Vec<String>));
    }

    #[test]
    fn update_guild_irc_channels_overwrites_previous_entry() {
        let mut ds = DiscordState::default();
        ds.guild_irc_channels.insert(5, vec!["#old".to_string()]);
        let bridge_map = make_bridge_map();
        update_guild_irc_channels(&mut ds, &bridge_map, 5, &[111]);
        assert_eq!(
            ds.guild_irc_channels.get(&5),
            Some(&vec!["#general".to_string()])
        );
    }
}
