//! Bridge orchestrator: stateful event handler that produces IRC/Discord commands.
//!
//! [`BridgeState`] holds all mutable bridge state and provides synchronous
//! handler methods that consume events and produce commands.  The async
//! `run_bridge` loop is a thin dispatcher that calls these methods.

use std::collections::HashMap;

use crate::config::Config;
use crate::discord::{DiscordCommand, DiscordEvent, DiscordPresence};
use crate::formatting::{DiscordResolver, IrcMentionResolver};
use crate::irc::{S2SCommand, S2SEvent};
use crate::pseudoclients::PseudoclientManager;

use super::map::BridgeMap;
use super::routing::{
    DmRouteResult, produce_burst_commands, route_discord_to_irc, route_dm_to_irc,
    route_irc_to_discord, route_irc_to_dm, update_guild_irc_channels,
};
use super::state::{
    DiscordState, IrcState, apply_discord_event, apply_irc_event, introduce_pseudoclient,
};

// ---------------------------------------------------------------------------
// Link phase
// ---------------------------------------------------------------------------

/// IRC link lifecycle phase.  Prevents impossible state combinations that
/// arise when link-up and burst-complete are tracked as independent booleans.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinkPhase {
    /// No S2S link is established.
    Down,
    /// Link is up; receiving the uplink's burst.  Discord events are buffered.
    Bursting,
    /// Burst complete; pseudoclients can be introduced.
    Ready,
}

// ---------------------------------------------------------------------------
// Resolvers
// ---------------------------------------------------------------------------

struct BridgeIrcResolver<'a> {
    pm: &'a PseudoclientManager,
}

impl IrcMentionResolver for BridgeIrcResolver<'_> {
    fn resolve_nick(&self, nick: &str) -> Option<String> {
        let state = self.pm.get_by_nick(nick)?;
        Some(state.discord_user_id.to_string())
    }
}

struct BridgeDiscordResolver<'a> {
    discord_state: &'a DiscordState,
}

impl DiscordResolver for BridgeDiscordResolver<'_> {
    fn resolve_user(&self, id: &str) -> Option<String> {
        let uid: u64 = id.parse().ok()?;
        self.discord_state.display_names.get(&uid).cloned()
    }
    fn resolve_channel(&self, id: &str) -> Option<String> {
        let cid: u64 = id.parse().ok()?;
        self.discord_state.channel_names.get(&cid).cloned()
    }
    fn resolve_role(&self, id: &str) -> Option<String> {
        let rid: u64 = id.parse().ok()?;
        self.discord_state.role_names.get(&rid).cloned()
    }
}

// ---------------------------------------------------------------------------
// Output types
// ---------------------------------------------------------------------------

/// Commands produced by a handler invocation.
#[derive(Debug, Default)]
pub struct HandlerOutput {
    pub irc_commands: Vec<S2SCommand>,
    pub discord_commands: Vec<DiscordCommand>,
}

// HandlerOutput uses derive(Default) — call HandlerOutput::default() directly.

// ---------------------------------------------------------------------------
// BridgeState
// ---------------------------------------------------------------------------

/// All mutable bridge state, with synchronous handler methods.
pub struct BridgeState {
    pub config: Config,
    pub bridge_map: BridgeMap,
    pub pm: PseudoclientManager,
    pub irc_state: IrcState,
    pub discord_state: DiscordState,
    /// Current IRC link lifecycle phase.
    link_phase: LinkPhase,
    /// Discord events buffered during the uplink burst.
    deferred_discord_events: Vec<DiscordEvent>,
    /// Kill-reintroduction cooldowns: `discord_user_id` → epoch seconds.
    pub kill_cooldowns: HashMap<u64, u64>,
}

impl BridgeState {
    /// Create a new bridge state from config.
    pub fn new(config: &Config) -> Self {
        Self {
            config: config.clone(),
            bridge_map: BridgeMap::from_config(&config.bridges),
            pm: PseudoclientManager::new(&config.irc.sid, &config.pseudoclients.ident),
            irc_state: IrcState::default(),
            discord_state: DiscordState::default(),
            link_phase: LinkPhase::Down,
            deferred_discord_events: Vec::new(),
            kill_cooldowns: HashMap::new(),
        }
    }

    /// Handle an IRC event.  Returns commands to send to IRC and Discord.
    pub fn handle_irc_event(&mut self, event: &S2SEvent, now_ts: u64) -> HandlerOutput {
        let mut output = HandlerOutput::default();

        match event {
            S2SEvent::LinkUp => {
                self.link_phase = LinkPhase::Bursting;
                // Don't send anything yet — wait for the remote burst to
                // complete so we know all external nicks before introducing
                // our pseudoclients.
            }
            S2SEvent::LinkDown { .. } => {
                self.link_phase = LinkPhase::Down;
                self.deferred_discord_events.clear();
                self.pm.clear_external_nicks();
            }
            S2SEvent::BurstComplete => {
                self.link_phase = LinkPhase::Ready;

                // Send our burst: re-introduce existing pseudoclients
                // (on reconnect; empty on first connect), replay deferred
                // Discord events (which may introduce more), then send
                // our EOS.  Nick collisions with external users from the
                // remote burst are resolved by the KILL handler.
                //
                // produce_burst_commands appends BurstComplete (EOS) but
                // we want EOS after deferred replay, so strip it and add
                // it explicitly at the end.
                let burst = produce_burst_commands(&self.pm, &self.irc_state, now_ts);
                output.irc_commands.extend(
                    burst
                        .into_iter()
                        .filter(|c| !matches!(c, S2SCommand::BurstComplete)),
                );

                // Replay buffered Discord events now that all IRC nicks are
                // registered from the remote burst.
                let deferred: Vec<_> = self.deferred_discord_events.drain(..).collect();
                for event in deferred {
                    let inner = self.process_discord_event(&event, now_ts);
                    output.irc_commands.extend(inner.irc_commands);
                    output.discord_commands.extend(inner.discord_commands);
                }

                // Our EOS — signals end of our burst.
                output.irc_commands.push(S2SCommand::BurstComplete);
            }
            S2SEvent::MessageReceived {
                from_uid,
                target,
                text,
                ..
            } => {
                let resolver = BridgeIrcResolver { pm: &self.pm };
                if let Some(cmd) = route_irc_to_discord(
                    &self.pm,
                    &self.bridge_map,
                    &self.irc_state,
                    from_uid,
                    target,
                    text,
                    false,
                    &resolver,
                    self.config.formatting.irc_nick_colon_mention,
                ) {
                    output.discord_commands.push(cmd);
                } else if self.config.formatting.dm_bridging
                    && let Some(cmd) = route_irc_to_dm(
                        &self.pm,
                        &self.irc_state,
                        from_uid,
                        target,
                        text,
                        &resolver,
                        self.config.formatting.irc_nick_colon_mention,
                    )
                {
                    output.discord_commands.push(cmd);
                }
            }
            S2SEvent::NoticeReceived {
                from_uid,
                target,
                text,
            } => {
                let resolver = BridgeIrcResolver { pm: &self.pm };
                if let Some(cmd) = route_irc_to_discord(
                    &self.pm,
                    &self.bridge_map,
                    &self.irc_state,
                    from_uid,
                    target,
                    text,
                    true,
                    &resolver,
                    self.config.formatting.irc_nick_colon_mention,
                ) {
                    output.discord_commands.push(cmd);
                }
            }
            _ => {}
        }

        // Capture pseudoclient identity before apply_irc_event removes it.
        let killed_pseudoclient = if let S2SEvent::UserKilled { uid, .. } = event {
            self.pm.get_by_uid(uid).map(|ps| {
                (
                    ps.discord_user_id,
                    ps.display_name.clone(),
                    ps.channels.clone(),
                )
            })
        } else {
            None
        };

        apply_irc_event(&mut self.irc_state, &mut self.pm, event);

        // Re-introduce killed pseudoclients if configured.
        if let Some((discord_id, display_name, channels)) = killed_pseudoclient
            && self.config.pseudoclients.reintroduce_on_kill
            && self.irc_state.is_link_up()
        {
            let cooldown_secs = 30u64;
            // Check and prune in one step: remove the entry if expired,
            // then check whether it survived.
            self.kill_cooldowns
                .retain(|_, ts| now_ts.saturating_sub(*ts) < cooldown_secs);
            if self.kill_cooldowns.contains_key(&discord_id) {
                tracing::warn!(
                    discord_id,
                    nick = %display_name,
                    "not re-introducing killed pseudoclient — killed again within 30s cooldown"
                );
            } else {
                let username = self
                    .discord_state
                    .usernames
                    .get(&discord_id)
                    .cloned()
                    .unwrap_or_else(|| display_name.clone());
                let cmds = introduce_pseudoclient(
                    &mut self.pm,
                    &self.irc_state,
                    discord_id,
                    &username,
                    &display_name,
                    &channels,
                    DiscordPresence::Online,
                    now_ts,
                );
                let new_uid = self
                    .pm
                    .get_by_discord_id(discord_id)
                    .map(|ps| ps.uid.as_str());
                tracing::debug!(
                    discord_id,
                    nick = %display_name,
                    new_uid = ?new_uid,
                    cmd_count = cmds.len(),
                    "re-introducing killed pseudoclient"
                );
                self.kill_cooldowns.insert(discord_id, now_ts);
                output.irc_commands.extend(cmds);
            }
        }

        output
    }

    /// Handle a Discord event.  Returns commands to send to IRC and Discord.
    pub fn handle_discord_event(&mut self, event: DiscordEvent, now_ts: u64) -> HandlerOutput {
        // Buffer events until the IRC link is ready.  Events arriving while
        // the link is Down or Bursting are replayed after BurstComplete.
        // Without this, state would be updated (e.g. pm.introduce()) but the
        // resulting IRC commands would be silently dropped, leaving
        // pseudoclients marked as introduced but never sent to IRC.
        if self.link_phase != LinkPhase::Ready {
            self.deferred_discord_events.push(event);
            return HandlerOutput::default();
        }

        self.process_discord_event(&event, now_ts)
    }

    /// Inner Discord event processing (used both live and for deferred replay).
    fn process_discord_event(&mut self, event: &DiscordEvent, now_ts: u64) -> HandlerOutput {
        let mut output = HandlerOutput::default();

        // Populate guild→irc-channel map.
        if let DiscordEvent::MemberSnapshot {
            guild_id,
            channel_ids,
            ..
        } = event
        {
            update_guild_irc_channels(
                &mut self.discord_state,
                &self.bridge_map,
                *guild_id,
                channel_ids,
            );
        }

        // Route Discord messages to IRC.
        if let DiscordEvent::MessageReceived {
            channel_id,
            author_id,
            author_name,
            content,
            attachments,
        } = event
        {
            let resolver = BridgeDiscordResolver {
                discord_state: &self.discord_state,
            };
            let cmds = route_discord_to_irc(
                &mut self.pm,
                &self.bridge_map,
                &self.discord_state,
                &self.irc_state,
                *channel_id,
                *author_id,
                author_name,
                content,
                attachments,
                None,
                now_ts,
                &resolver,
            );
            output.irc_commands.extend(cmds);
        }

        // Route Discord DMs to IRC.
        if let DiscordEvent::DmReceived {
            author_id,
            content,
            referenced_content,
            ..
        } = event
            && self.config.formatting.dm_bridging
        {
            let resolver = BridgeDiscordResolver {
                discord_state: &self.discord_state,
            };
            match route_dm_to_irc(
                &self.pm,
                &self.irc_state,
                *author_id,
                content,
                referenced_content.as_deref(),
                &resolver,
            ) {
                DmRouteResult::Relay {
                    from_uid,
                    target_uid,
                    text,
                } => {
                    output.irc_commands.push(S2SCommand::SendMessage {
                        from_uid,
                        target: target_uid,
                        text,
                        timestamp: None,
                    });
                }
                DmRouteResult::Error(msg) => {
                    output.discord_commands.push(DiscordCommand::SendBotDm {
                        recipient_user_id: *author_id,
                        text: msg,
                    });
                }
            }
        }

        // Apply state update and optionally forward introduce commands.
        let cmds = apply_discord_event(
            &mut self.discord_state,
            &mut self.pm,
            &self.irc_state,
            event,
            now_ts,
        );
        if self.link_phase == LinkPhase::Ready {
            output.irc_commands.extend(cmds);
        }

        output
    }

    /// Update config and return a `ReloadBridges` command if bridges changed.
    pub fn reload_config(&mut self, new_config: Config) -> Option<DiscordCommand> {
        let diff = crate::config::diff_bridges(&self.config.bridges, &new_config.bridges);
        let cmd = if diff.is_empty() {
            None
        } else {
            let added_ids: Vec<u64> = diff
                .added
                .iter()
                .chain(diff.webhook_changed.iter())
                .filter_map(|e| e.discord_channel_id.parse().ok())
                .collect();
            let removed_ids: Vec<u64> = diff
                .removed
                .iter()
                .filter_map(|e| e.discord_channel_id.parse().ok())
                .collect();
            let added_webhook_ids: Vec<u64> = diff
                .added
                .iter()
                .chain(diff.webhook_changed.iter())
                .filter_map(|e| {
                    e.webhook_url
                        .as_deref()
                        .and_then(crate::discord::webhook_id_from_url)
                })
                .collect();
            let removed_webhook_ids: Vec<u64> = diff
                .removed
                .iter()
                .chain(diff.webhook_changed.iter())
                .filter_map(|e| {
                    e.webhook_url
                        .as_deref()
                        .and_then(crate::discord::webhook_id_from_url)
                })
                .collect();
            self.bridge_map = BridgeMap::from_config(&new_config.bridges);
            Some(DiscordCommand::ReloadBridges {
                added_channel_ids: added_ids,
                removed_channel_ids: removed_ids,
                added_webhook_ids,
                removed_webhook_ids,
            })
        };
        self.config = new_config;
        cmd
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        BridgeEntry, Config, DiscordConfig, FormattingConfig, IrcConfig, PseudoclientConfig,
    };
    use crate::discord::MemberInfo;

    fn test_config() -> Config {
        Config {
            discord: DiscordConfig { token: "x".into() },
            irc: IrcConfig {
                uplink: "localhost".into(),
                port: 6667,
                tls: false,
                link_name: "bridge.test".into(),
                link_password: "pw".into(),
                sid: "002".into(),
                description: "test".into(),
                connect_timeout: 15,
            },
            pseudoclients: PseudoclientConfig {
                ident: "discord".into(),
                reintroduce_on_kill: false,
            },
            formatting: FormattingConfig::default(),
            bridges: vec![BridgeEntry {
                discord_channel_id: "111".into(),
                irc_channel: "#test".into(),
                webhook_url: None,
            }],
        }
    }

    /// Discord events during the uplink burst must be buffered and replayed
    /// after BurstComplete, ensuring IRC nicks from the burst are registered
    /// before pseudoclients are introduced (avoiding nick collisions).
    #[test]
    fn pseudoclient_deferred_until_burst_complete_avoids_nick_collision() {
        let mut state = BridgeState::new(&test_config());
        let ts = 1_000_000;

        // LinkUp — no commands yet, just transitions to Bursting.
        let out = state.handle_irc_event(&S2SEvent::LinkUp, ts);
        assert!(
            out.irc_commands.is_empty(),
            "LinkUp should not emit any commands"
        );

        // Discord user "jono" appears in MemberSnapshot during uplink burst.
        let out = state.handle_discord_event(
            DiscordEvent::MemberSnapshot {
                guild_id: 999,
                members: vec![MemberInfo {
                    user_id: 3001,
                    username: "jono".into(),
                    display_name: "jono".into(),
                    presence: DiscordPresence::Online,
                }],
                channel_ids: vec![111],
                channel_names: std::collections::HashMap::new(),
                role_names: std::collections::HashMap::new(),
            },
            ts,
        );
        // Must be buffered — no commands yet.
        assert!(
            out.irc_commands.is_empty(),
            "Discord events during uplink burst must be buffered; got: {:?}",
            out.irc_commands
        );

        // IRC user "jono" introduced in the uplink burst.
        state.handle_irc_event(
            &S2SEvent::UserIntroduced {
                uid: "001JONO01".into(),
                nick: "jono".into(),
                server_sid: "001".into(),
                realname: "Jono".into(),
                host: "example.com".into(),
                ident: "jono".into(),
            },
            ts,
        );

        // Uplink BurstComplete — deferred events should replay.
        let out = state.handle_irc_event(&S2SEvent::BurstComplete, ts);

        // Find the IntroduceUser command for the pseudoclient.
        let nick = out
            .irc_commands
            .iter()
            .find_map(|c| {
                if let S2SCommand::IntroduceUser { nick, .. } = c {
                    Some(nick.clone())
                } else {
                    None
                }
            })
            .expect("pseudoclient should be introduced after BurstComplete");

        assert_ne!(
            nick, "jono",
            "pseudoclient must get a suffixed nick to avoid collision; got: {nick}"
        );
    }

    /// After BurstComplete, Discord events should be processed immediately
    /// (not buffered).
    #[test]
    fn discord_events_processed_after_burst_complete() {
        let mut state = BridgeState::new(&test_config());
        let ts = 1_000_000;

        state.handle_irc_event(&S2SEvent::LinkUp, ts);
        state.handle_irc_event(&S2SEvent::BurstComplete, ts);

        let out = state.handle_discord_event(
            DiscordEvent::MemberSnapshot {
                guild_id: 999,
                members: vec![MemberInfo {
                    user_id: 4001,
                    username: "Alice".into(),
                    display_name: "Alice".into(),
                    presence: DiscordPresence::Online,
                }],
                channel_ids: vec![111],
                channel_names: std::collections::HashMap::new(),
                role_names: std::collections::HashMap::new(),
            },
            ts,
        );

        assert!(
            out.irc_commands
                .iter()
                .any(|c| matches!(c, S2SCommand::IntroduceUser { .. })),
            "after BurstComplete, Discord events should produce commands immediately"
        );
    }

    /// KILL of a pseudoclient with reintroduce_on_kill=true should produce
    /// IntroduceUser commands with a fresh UID.
    #[test]
    fn kill_with_reintroduce_produces_new_uid() {
        let mut config = test_config();
        config.pseudoclients.reintroduce_on_kill = true;
        let mut state = BridgeState::new(&config);
        let ts = 1_000_000;

        // Set up: link up, burst complete, introduce pseudoclient.
        state.handle_irc_event(&S2SEvent::LinkUp, ts);
        state.handle_irc_event(&S2SEvent::BurstComplete, ts);
        let out = state.handle_discord_event(
            DiscordEvent::MemberSnapshot {
                guild_id: 999,
                members: vec![MemberInfo {
                    user_id: 5001,
                    username: "Bob".into(),
                    display_name: "Bob".into(),
                    presence: DiscordPresence::Online,
                }],
                channel_ids: vec![111],
                channel_names: std::collections::HashMap::new(),
                role_names: std::collections::HashMap::new(),
            },
            ts,
        );
        let old_uid = out
            .irc_commands
            .iter()
            .find_map(|c| {
                if let S2SCommand::IntroduceUser { uid, .. } = c {
                    Some(uid.clone())
                } else {
                    None
                }
            })
            .expect("should introduce Bob");

        // Kill Bob.
        let out = state.handle_irc_event(
            &S2SEvent::UserKilled {
                uid: old_uid.clone(),
                reason: "testing".into(),
            },
            ts,
        );

        let new_uid = out
            .irc_commands
            .iter()
            .find_map(|c| {
                if let S2SCommand::IntroduceUser { uid, .. } = c {
                    Some(uid.clone())
                } else {
                    None
                }
            })
            .expect("should reintroduce Bob");

        assert_ne!(
            old_uid, new_uid,
            "reintroduced UID must differ from killed UID"
        );
    }

    /// IRC resolver finds pseudoclient by nick.
    #[test]
    fn irc_resolver_finds_pseudoclient() {
        let mut pm = PseudoclientManager::new("002", "bridge");
        pm.introduce(42, "alice", "Alice", &["#test".to_string()], 1000);
        let resolver = BridgeIrcResolver { pm: &pm };
        assert_eq!(resolver.resolve_nick("alice"), Some("42".to_string()));
    }

    /// IRC resolver returns None for unknown nick.
    #[test]
    fn irc_resolver_unknown_nick() {
        let pm = PseudoclientManager::new("002", "bridge");
        let resolver = BridgeIrcResolver { pm: &pm };
        assert_eq!(resolver.resolve_nick("nobody"), None);
    }

    /// Discord resolver finds user/channel/role by ID.
    #[test]
    fn discord_resolver_lookups() {
        let mut ds = DiscordState::default();
        ds.display_names.insert(42, "Alice".to_string());
        ds.channel_names.insert(100, "general".to_string());
        ds.role_names.insert(200, "Moderator".to_string());
        let resolver = BridgeDiscordResolver { discord_state: &ds };
        assert_eq!(resolver.resolve_user("42"), Some("Alice".to_string()));
        assert_eq!(resolver.resolve_channel("100"), Some("general".to_string()));
        assert_eq!(resolver.resolve_role("200"), Some("Moderator".to_string()));
    }

    /// Discord resolver returns None for unknown/invalid IDs.
    #[test]
    fn discord_resolver_unknown() {
        let ds = DiscordState::default();
        let resolver = BridgeDiscordResolver { discord_state: &ds };
        assert_eq!(resolver.resolve_user("999"), None);
        assert_eq!(resolver.resolve_user("notanumber"), None);
    }

    // --- LinkDown recovery ---

    /// Deferred Discord events from a previous connection must be discarded
    /// on LinkDown.  If they survive, the next BurstComplete would replay
    /// stale events from a connection that no longer exists.
    #[test]
    fn link_down_clears_deferred_discord_events() {
        let mut state = BridgeState::new(&test_config());
        let ts = 1_000_000;

        // LinkUp → buffer a Discord event during burst.
        state.handle_irc_event(&S2SEvent::LinkUp, ts);
        state.handle_discord_event(
            DiscordEvent::MemberSnapshot {
                guild_id: 999,
                members: vec![MemberInfo {
                    user_id: 7001,
                    username: "Alice".into(),
                    display_name: "Alice".into(),
                    presence: DiscordPresence::Online,
                }],
                channel_ids: vec![111],
                channel_names: std::collections::HashMap::new(),
                role_names: std::collections::HashMap::new(),
            },
            ts,
        );
        assert!(
            !state.deferred_discord_events.is_empty(),
            "event should be buffered during burst"
        );

        // Link drops before BurstComplete.
        state.handle_irc_event(
            &S2SEvent::LinkDown {
                reason: "connection lost".into(),
            },
            ts,
        );

        assert!(
            state.deferred_discord_events.is_empty(),
            "deferred events must be cleared on LinkDown"
        );
    }

    /// External nicks (known_nicks) from a previous connection must be cleared
    /// on LinkDown.  If they survive, pseudoclient nick collision avoidance
    /// would incorrectly suffix nicks for users who quit while the link was
    /// down.
    #[test]
    fn link_down_clears_external_nicks() {
        let mut state = BridgeState::new(&test_config());
        let ts = 1_000_000;

        // Set up: link up, burst with IRC user "alice", burst complete.
        state.handle_irc_event(&S2SEvent::LinkUp, ts);
        state.handle_irc_event(
            &S2SEvent::UserIntroduced {
                uid: "001ALICE1".into(),
                nick: "alice".into(),
                server_sid: "001".into(),
                realname: "Alice".into(),
                host: "example.com".into(),
                ident: "alice".into(),
            },
            ts,
        );
        state.handle_irc_event(&S2SEvent::BurstComplete, ts);

        // "alice" is now a known external nick — pseudoclient would be suffixed.
        state
            .pm
            .introduce(42, "alice", "Alice", &["#test".to_string()], ts);
        let suffixed = state.pm.get_by_discord_id(42).unwrap().nick.clone();
        assert_ne!(suffixed, "alice", "should be suffixed before LinkDown");
        // Clean up the test introduction.
        state.pm.quit(42, "test");

        // Link drops — "alice" may have quit while we were disconnected.
        state.handle_irc_event(
            &S2SEvent::LinkDown {
                reason: "connection lost".into(),
            },
            ts,
        );

        // After LinkDown, "alice" should no longer be known.
        // A new pseudoclient introduction should get the exact nick.
        state
            .pm
            .introduce(43, "alice", "Alice", &["#test".to_string()], ts);
        let nick = state.pm.get_by_discord_id(43).unwrap().nick.clone();
        assert_eq!(
            nick, "alice",
            "after LinkDown, external nicks should be cleared; got: {nick}"
        );
    }

    // --- IRC→Discord message relay ---

    /// Helper: set up a bridge with link up, burst complete, and a pseudoclient.
    fn setup_bridge_with_pseudoclient() -> (BridgeState, String) {
        let mut state = BridgeState::new(&test_config());
        let ts = 1_000_000;
        state.handle_irc_event(&S2SEvent::LinkUp, ts);
        state.handle_irc_event(&S2SEvent::BurstComplete, ts);

        // Introduce an external IRC user so messages come from a known UID.
        state.handle_irc_event(
            &S2SEvent::UserIntroduced {
                uid: "001AAA001".into(),
                nick: "ircuser".into(),
                server_sid: "001".into(),
                realname: "IRC User".into(),
                host: "example.com".into(),
                ident: "ircuser".into(),
            },
            ts,
        );
        (state, "001AAA001".to_string())
    }

    #[test]
    fn irc_message_routed_to_discord() {
        let (mut state, uid) = setup_bridge_with_pseudoclient();
        let out = state.handle_irc_event(
            &S2SEvent::MessageReceived {
                from_uid: uid,
                target: "#test".into(),
                text: "hello".into(),
                timestamp: None,
            },
            1_000_000,
        );
        assert!(
            !out.discord_commands.is_empty(),
            "IRC PRIVMSG to bridged channel should produce a Discord command"
        );
    }

    #[test]
    fn irc_notice_routed_to_discord() {
        let (mut state, uid) = setup_bridge_with_pseudoclient();
        let out = state.handle_irc_event(
            &S2SEvent::NoticeReceived {
                from_uid: uid,
                target: "#test".into(),
                text: "notice text".into(),
            },
            1_000_000,
        );
        assert!(
            !out.discord_commands.is_empty(),
            "IRC NOTICE to bridged channel should produce a Discord command"
        );
    }

    // --- KILL cooldown ---

    #[test]
    fn kill_within_cooldown_does_not_reintroduce() {
        let mut config = test_config();
        config.pseudoclients.reintroduce_on_kill = true;
        let mut state = BridgeState::new(&config);
        let ts = 1_000_000;

        state.handle_irc_event(&S2SEvent::LinkUp, ts);
        state.handle_irc_event(&S2SEvent::BurstComplete, ts);
        let out = state.handle_discord_event(
            DiscordEvent::MemberSnapshot {
                guild_id: 999,
                members: vec![MemberInfo {
                    user_id: 8001,
                    username: "Charlie".into(),
                    display_name: "Charlie".into(),
                    presence: DiscordPresence::Online,
                }],
                channel_ids: vec![111],
                channel_names: std::collections::HashMap::new(),
                role_names: std::collections::HashMap::new(),
            },
            ts,
        );
        let uid1 = out
            .irc_commands
            .iter()
            .find_map(|c| {
                if let S2SCommand::IntroduceUser { uid, .. } = c {
                    Some(uid.clone())
                } else {
                    None
                }
            })
            .expect("should introduce Charlie");

        // First kill — should reintroduce.
        let out = state.handle_irc_event(
            &S2SEvent::UserKilled {
                uid: uid1.clone(),
                reason: "first kill".into(),
            },
            ts,
        );
        let uid2 = out
            .irc_commands
            .iter()
            .find_map(|c| {
                if let S2SCommand::IntroduceUser { uid, .. } = c {
                    Some(uid.clone())
                } else {
                    None
                }
            })
            .expect("first kill should reintroduce");

        // Second kill within cooldown — should NOT reintroduce.
        let out = state.handle_irc_event(
            &S2SEvent::UserKilled {
                uid: uid2,
                reason: "second kill".into(),
            },
            ts + 5, // only 5 seconds later
        );
        assert!(
            !out.irc_commands
                .iter()
                .any(|c| matches!(c, S2SCommand::IntroduceUser { .. })),
            "second kill within cooldown must not reintroduce"
        );

        // Third kill after cooldown expires — should reintroduce again.
        // Re-introduce manually since the cooldown suppressed it.
        state.handle_discord_event(
            DiscordEvent::PresenceUpdated {
                user_id: 8001,
                guild_id: 999,
                presence: DiscordPresence::Online,
                username: None,
                display_name: None,
            },
            ts + 30,
        );
        let uid3 = state
            .pm
            .get_by_discord_id(8001)
            .map(|s| s.uid.clone())
            .expect("should be introduced via presence");
        let out = state.handle_irc_event(
            &S2SEvent::UserKilled {
                uid: uid3,
                reason: "third kill".into(),
            },
            ts + 30,
        );
        assert!(
            out.irc_commands
                .iter()
                .any(|c| matches!(c, S2SCommand::IntroduceUser { .. })),
            "kill after cooldown expires should reintroduce"
        );
    }

    // --- reload_config ---

    #[test]
    fn reload_config_with_changed_bridges_produces_command() {
        let mut state = BridgeState::new(&test_config());
        let mut new_config = test_config();
        new_config.bridges.push(BridgeEntry {
            discord_channel_id: "222".into(),
            irc_channel: "#new".into(),
            webhook_url: None,
        });
        let cmd = state.reload_config(new_config);
        assert!(
            cmd.is_some(),
            "reload_config with changed bridges should return a DiscordCommand"
        );
    }

    #[test]
    fn reload_config_with_no_changes_returns_none() {
        let mut state = BridgeState::new(&test_config());
        let same_config = test_config();
        let cmd = state.reload_config(same_config);
        assert!(
            cmd.is_none(),
            "reload_config with identical bridges should return None"
        );
    }

    // --- Discord event buffering gate ---

    #[test]
    fn discord_state_cmds_not_forwarded_after_link_down() {
        let mut state = BridgeState::new(&test_config());
        let ts = 1_000_000;

        // Link up, burst complete — phase is Ready.
        state.handle_irc_event(&S2SEvent::LinkUp, ts);
        state.handle_irc_event(&S2SEvent::BurstComplete, ts);

        // Link drops — phase returns to Down.
        state.handle_irc_event(
            &S2SEvent::LinkDown {
                reason: "test".into(),
            },
            ts,
        );

        // Discord event while link is down — should not produce IRC commands.
        let out = state.handle_discord_event(
            DiscordEvent::MemberSnapshot {
                guild_id: 999,
                members: vec![MemberInfo {
                    user_id: 9001,
                    username: "Dave".into(),
                    display_name: "Dave".into(),
                    presence: DiscordPresence::Online,
                }],
                channel_ids: vec![111],
                channel_names: std::collections::HashMap::new(),
                role_names: std::collections::HashMap::new(),
            },
            ts,
        );
        assert!(
            out.irc_commands.is_empty(),
            "Discord events when link is down should not produce IRC commands"
        );
    }

    /// Discord events arriving before LinkUp (phase=Down) must be buffered
    /// and replayed after BurstComplete, not silently dropped.
    #[test]
    fn discord_event_before_link_up_replayed_after_burst() {
        let mut state = BridgeState::new(&test_config());
        let ts = 1_000_000;

        // Discord MemberSnapshot arrives before IRC link is up.
        let out = state.handle_discord_event(
            DiscordEvent::MemberSnapshot {
                guild_id: 999,
                members: vec![MemberInfo {
                    user_id: 9002,
                    username: "Eve".into(),
                    display_name: "Eve".into(),
                    presence: DiscordPresence::Online,
                }],
                channel_ids: vec![111],
                channel_names: std::collections::HashMap::new(),
                role_names: std::collections::HashMap::new(),
            },
            ts,
        );
        assert!(
            out.irc_commands.is_empty(),
            "should be buffered, not processed"
        );
        assert!(
            state.pm.get_by_discord_id(9002).is_none(),
            "pseudoclient should not be introduced yet"
        );

        // IRC link comes up, burst completes.
        state.handle_irc_event(&S2SEvent::LinkUp, ts);
        let out = state.handle_irc_event(&S2SEvent::BurstComplete, ts);

        // The buffered MemberSnapshot should now be replayed.
        assert!(
            out.irc_commands
                .iter()
                .any(|c| matches!(c, S2SCommand::IntroduceUser { .. })),
            "buffered MemberSnapshot should produce IntroduceUser after BurstComplete"
        );
        assert!(
            state.pm.get_by_discord_id(9002).is_some(),
            "Eve should be introduced after replay"
        );
    }

    /// After IRC link drops and reconnects, existing pseudoclients must be
    /// re-introduced to the new link on BurstComplete.
    #[test]
    fn reconnect_rebursts_existing_pseudoclients() {
        let mut state = BridgeState::new(&test_config());
        let ts = 1_000_000;

        // First connect: introduce a pseudoclient.
        state.handle_irc_event(&S2SEvent::LinkUp, ts);
        let out = state.handle_discord_event(
            DiscordEvent::MemberSnapshot {
                guild_id: 999,
                members: vec![MemberInfo {
                    user_id: 7777,
                    username: "Frank".into(),
                    display_name: "Frank".into(),
                    presence: DiscordPresence::Online,
                }],
                channel_ids: vec![111],
                channel_names: std::collections::HashMap::new(),
                role_names: std::collections::HashMap::new(),
            },
            ts,
        );
        assert!(out.irc_commands.is_empty(), "buffered during burst");
        let out = state.handle_irc_event(&S2SEvent::BurstComplete, ts);
        assert!(
            out.irc_commands
                .iter()
                .any(|c| matches!(c, S2SCommand::IntroduceUser { .. })),
            "Frank should be introduced on first connect"
        );
        let frank_uid = state.pm.get_by_discord_id(7777).unwrap().uid.clone();

        // IRC link drops.
        state.handle_irc_event(
            &S2SEvent::LinkDown {
                reason: "test".into(),
            },
            ts + 100,
        );
        // Frank still exists in pm.
        assert!(state.pm.get_by_discord_id(7777).is_some());

        // Reconnect: link up, remote burst, burst complete.
        state.handle_irc_event(&S2SEvent::LinkUp, ts + 200);
        let out = state.handle_irc_event(&S2SEvent::BurstComplete, ts + 200);

        // Frank should be re-introduced with the same UID.
        let reintroduced = out
            .irc_commands
            .iter()
            .find_map(|c| {
                if let S2SCommand::IntroduceUser { uid, nick, .. } = c {
                    Some((uid.clone(), nick.clone()))
                } else {
                    None
                }
            })
            .expect("Frank should be re-introduced on reconnect");
        assert_eq!(reintroduced.0, frank_uid, "should reuse same UID");
        assert_eq!(reintroduced.1, "Frank");

        // Our EOS should be present.
        assert!(
            out.irc_commands
                .iter()
                .any(|c| matches!(c, S2SCommand::BurstComplete)),
            "burst should end with our EOS"
        );
    }

    /// Our EOS is sent on BurstComplete, not LinkUp.
    #[test]
    fn eos_sent_on_burst_complete_not_link_up() {
        let mut state = BridgeState::new(&test_config());
        let ts = 1_000_000;

        let out = state.handle_irc_event(&S2SEvent::LinkUp, ts);
        assert!(
            !out.irc_commands
                .iter()
                .any(|c| matches!(c, S2SCommand::BurstComplete)),
            "LinkUp must not send EOS"
        );

        let out = state.handle_irc_event(&S2SEvent::BurstComplete, ts);
        assert!(
            out.irc_commands
                .iter()
                .any(|c| matches!(c, S2SCommand::BurstComplete)),
            "BurstComplete must send our EOS"
        );
    }
}
