//! Bridge orchestrator: stateful event handler that produces IRC/Discord commands.
//!
//! [`BridgeState`] holds all mutable bridge state and provides synchronous
//! handler methods that consume events and produce commands.  The async
//! `run_bridge` loop is a thin dispatcher that calls these methods.

use std::collections::HashMap;

use crate::config::Config;
use crate::discord::{DiscordCommand, DiscordEvent};
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

/// Whether the IRC link is ready for pseudoclient traffic.
///
/// Discord events always update state immediately.  IRC commands are only
/// emitted when `Ready`.  `LinkUp` sends our burst and transitions to
/// `Ready`; `LinkDown` resets to `NotReady`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinkPhase {
    /// Link is down or bursting — IRC commands are suppressed.
    NotReady,
    /// Burst complete; pseudoclients can be introduced and messages relayed.
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
    pub(crate) config: Config,
    pub(crate) bridge_map: BridgeMap,
    pub(crate) pm: PseudoclientManager,
    pub(crate) irc_state: IrcState,
    pub(crate) discord_state: DiscordState,
    /// Current IRC link lifecycle phase.
    link_phase: LinkPhase,
    /// Whether the remote server's burst (`EOS`) has been received since
    /// the last `LinkUp`.  Used to decide whether KILL reintroduction can
    /// happen immediately (remote nicks are known) or must be deferred.
    remote_burst_done: bool,
    /// Kill-reintroduction cooldowns: `discord_user_id` → epoch seconds.
    pub(crate) kill_cooldowns: HashMap<u64, u64>,
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
            link_phase: LinkPhase::NotReady,
            remote_burst_done: false,
            kill_cooldowns: HashMap::new(),
        }
    }

    /// Handle an IRC event.  Returns commands to send to IRC and Discord.
    pub fn handle_irc_event(&mut self, event: &S2SEvent, now_ts: u64) -> HandlerOutput {
        let mut output = HandlerOutput::default();

        match event {
            S2SEvent::LinkUp => {
                self.link_phase = LinkPhase::Ready;
                self.remote_burst_done = false;

                // Send our burst and go live immediately.  We don't need
                // to wait for the remote burst — IRC S2S allows both sides
                // to burst concurrently.  Any Discord events processed
                // while the link was down have already updated PM state;
                // the burst captures the current snapshot.  Nick collisions
                // are handled by the KILL handler (deferred to BurstComplete
                // so nick re-resolution has the remote nicks available).
                output.irc_commands.extend(produce_burst_commands(
                    &self.pm,
                    &self.irc_state,
                    now_ts,
                ));
            }
            S2SEvent::LinkDown { .. } => {
                self.link_phase = LinkPhase::NotReady;
                self.remote_burst_done = false;
                self.pm.clear_external_nicks();
                self.pm.clear_needs_reintroduce();
            }
            S2SEvent::BurstComplete => {
                self.remote_burst_done = true;

                // Reintroduce any pseudoclients that were killed during
                // the burst window.  known_nicks is now populated from the
                // remote burst, so nick re-resolution will pick non-
                // colliding nicks.
                let pending: Vec<u64> = self
                    .pm
                    .iter_states()
                    .filter(|s| s.needs_reintroduce)
                    .map(|s| s.discord_user_id)
                    .collect();
                for discord_id in pending {
                    let cmds = self.reintroduce_killed(discord_id, now_ts);
                    tracing::debug!(
                        discord_id,
                        cmd_count = cmds.len(),
                        "deferred reintroduce after BurstComplete"
                    );
                    output.irc_commands.extend(cmds);
                }
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

        // Identify killed pseudoclient before apply_irc_event runs.
        let killed_discord_id = if let S2SEvent::UserKilled { uid, .. } = event {
            self.pm.get_by_uid(uid).map(|ps| ps.discord_user_id)
        } else {
            None
        };

        // apply_irc_event marks the pseudoclient as needs_reintroduce
        // and clears its UID cache.
        apply_irc_event(&mut self.irc_state, &mut self.pm, event);

        // Handle killed pseudoclients.
        if let Some(discord_id) = killed_discord_id {
            if !self.config.pseudoclients.reintroduce_on_kill {
                // Operator's kill respected — remove from PM entirely.
                self.pm.quit(discord_id, "Killed");
            } else if self.remote_burst_done {
                // Remote burst is done — known_nicks is populated, safe to
                // reintroduce immediately with nick re-resolution.
                let cooldown_secs = 30u64;
                self.kill_cooldowns
                    .retain(|_, ts| now_ts.saturating_sub(*ts) < cooldown_secs);
                if self.kill_cooldowns.contains_key(&discord_id) {
                    tracing::warn!(
                        discord_id,
                        "not re-introducing killed pseudoclient — killed again within 30s cooldown"
                    );
                } else {
                    let cmds = self.reintroduce_killed(discord_id, now_ts);
                    tracing::debug!(
                        discord_id,
                        cmd_count = cmds.len(),
                        "re-introducing killed pseudoclient immediately"
                    );
                    self.kill_cooldowns.insert(discord_id, now_ts);
                    output.irc_commands.extend(cmds);
                }
            }
            // else: remote_burst_done is false — the pseudoclient is marked
            // needs_reintroduce by apply_irc_event.  BurstComplete will
            // reintroduce it once known_nicks is populated.
        }

        output
    }

    /// Remove a killed pseudoclient and reintroduce with a fresh UID and
    /// re-resolved nick.  Returns the IRC commands for the new introduction.
    fn reintroduce_killed(&mut self, discord_id: u64, now_ts: u64) -> Vec<S2SCommand> {
        let Some(ps) = self.pm.get_by_discord_id(discord_id) else {
            return vec![];
        };
        let username = ps.username.clone();
        let display_name = ps.display_name.clone();
        let channels = ps.channels.clone();
        let presence = ps.presence;

        if ps.needs_reintroduce {
            self.pm.remove_marked(discord_id);
        } else {
            self.pm.quit(discord_id, "Killed");
        }

        introduce_pseudoclient(
            &mut self.pm,
            &self.irc_state,
            discord_id,
            &username,
            &display_name,
            &channels,
            presence,
            now_ts,
        )
    }

    /// Handle a Discord event.  Returns commands to send to IRC and Discord.
    ///
    /// State is always updated immediately (so pseudoclients are tracked even
    /// while the IRC link is down).  IRC commands are only emitted when the
    /// link phase is `Ready`.
    pub fn handle_discord_event(&mut self, event: &DiscordEvent, now_ts: u64) -> HandlerOutput {
        self.process_discord_event(event, now_ts)
    }

    /// Inner Discord event processing.
    ///
    /// State is always updated.  IRC commands (introductions, messages, DMs)
    /// are only emitted when `link_phase == Ready`.
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

        // Route Discord messages to IRC (only when link is ready — messages
        // arriving while the link is down are dropped).
        if self.link_phase == LinkPhase::Ready {
            if let DiscordEvent::MessageReceived {
                channel_id,
                author_id,
                author_name,
                author_display_name,
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
                    &self.irc_state,
                    *channel_id,
                    *author_id,
                    author_name,
                    author_display_name,
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
        }

        // Always apply state update; only forward IRC commands when link is ready.
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
    use crate::discord::{DiscordPresence, MemberInfo};

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

    /// After BurstComplete, Discord events should produce IRC commands
    /// immediately (link phase is Ready).
    #[test]
    fn discord_events_processed_after_burst_complete() {
        let mut state = BridgeState::new(&test_config());
        let ts = 1_000_000;

        state.handle_irc_event(&S2SEvent::LinkUp, ts);

        let out = state.handle_discord_event(
            &DiscordEvent::MemberSnapshot {
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
            "after BurstComplete, Discord events should produce IRC commands immediately"
        );
    }

    /// Discord events update PM state even when the IRC link is not ready,
    /// but no IRC commands are emitted.
    #[test]
    fn discord_events_update_state_when_link_not_ready() {
        let mut state = BridgeState::new(&test_config());
        let ts = 1_000_000;

        // Link is NotReady (no LinkUp yet).
        let out = state.handle_discord_event(
            &DiscordEvent::MemberSnapshot {
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

        // No IRC commands emitted while link is not ready.
        assert!(
            out.irc_commands.is_empty(),
            "no IRC commands when link is not ready; got: {:?}",
            out.irc_commands
        );

        // But pseudoclient state IS updated.
        assert!(
            state.pm.get_by_discord_id(3001).is_some(),
            "pseudoclient should exist in PM even though link is not ready"
        );
    }

    /// Messages received when link is not ready are dropped (not queued).
    #[test]
    fn messages_dropped_when_link_not_ready() {
        let mut state = BridgeState::new(&test_config());
        let ts = 1_000_000;

        // Introduce a pseudoclient while link is not ready.
        state.handle_discord_event(
            &DiscordEvent::MemberSnapshot {
                guild_id: 999,
                members: vec![MemberInfo {
                    user_id: 42,
                    username: "alice".into(),
                    display_name: "Alice".into(),
                    presence: DiscordPresence::Online,
                }],
                channel_ids: vec![111],
                channel_names: std::collections::HashMap::new(),
                role_names: std::collections::HashMap::new(),
            },
            ts,
        );

        // Send a message while link is not ready.
        let out = state.handle_discord_event(
            &DiscordEvent::MessageReceived {
                channel_id: 111,
                author_id: 42,
                author_name: "alice".into(),
                author_display_name: "Alice".into(),
                content: "hello world".into(),
                attachments: vec![],
            },
            ts,
        );

        assert!(
            out.irc_commands.is_empty(),
            "messages should be dropped when link is not ready"
        );
    }

    /// BurstComplete emits AWAY for pseudoclients with non-Online presence.
    #[test]
    fn burst_includes_away_for_idle_pseudoclients() {
        let mut state = BridgeState::new(&test_config());
        let ts = 1_000_000;

        // Introduce a pseudoclient with Idle presence while link is not ready.
        state.handle_discord_event(
            &DiscordEvent::MemberSnapshot {
                guild_id: 999,
                members: vec![MemberInfo {
                    user_id: 5001,
                    username: "idler".into(),
                    display_name: "Idler".into(),
                    presence: DiscordPresence::Idle,
                }],
                channel_ids: vec![111],
                channel_names: std::collections::HashMap::new(),
                role_names: std::collections::HashMap::new(),
            },
            ts,
        );

        // Link up, burst complete — burst sent.
        let out = state.handle_irc_event(&S2SEvent::LinkUp, ts);

        // Should have IntroduceUser + JoinChannel + SetAway + BurstComplete (EOS).
        assert!(
            out.irc_commands
                .iter()
                .any(|c| matches!(c, S2SCommand::IntroduceUser { .. })),
            "burst should include IntroduceUser"
        );
        assert!(
            out.irc_commands
                .iter()
                .any(|c| matches!(c, S2SCommand::SetAway { reason, .. } if reason == "Idle")),
            "burst should include SetAway for idle pseudoclient; got: {:?}",
            out.irc_commands
        );
    }

    /// Presence change while link is down is stored and reflected in burst AWAY.
    #[test]
    fn presence_change_while_not_ready_reflected_in_burst() {
        let mut state = BridgeState::new(&test_config());
        let ts = 1_000_000;

        // Introduce online user while link is not ready.
        state.handle_discord_event(
            &DiscordEvent::MemberSnapshot {
                guild_id: 999,
                members: vec![MemberInfo {
                    user_id: 6001,
                    username: "frank".into(),
                    display_name: "Frank".into(),
                    presence: DiscordPresence::Online,
                }],
                channel_ids: vec![111],
                channel_names: std::collections::HashMap::new(),
                role_names: std::collections::HashMap::new(),
            },
            ts,
        );

        // User goes DnD while link is still not ready.
        state.handle_discord_event(
            &DiscordEvent::PresenceUpdated {
                user_id: 6001,
                guild_id: 999,
                presence: DiscordPresence::DoNotDisturb,
                username: Some("frank".into()),
                display_name: Some("Frank".into()),
            },
            ts + 10,
        );

        // Link comes up — burst sent on BurstComplete.
        let out = state.handle_irc_event(&S2SEvent::LinkUp, ts + 20);

        // Burst should include AWAY for the DnD presence.
        assert!(
            out.irc_commands.iter().any(
                |c| matches!(c, S2SCommand::SetAway { reason, .. } if reason == "Do Not Disturb")
            ),
            "burst should reflect DnD presence set while link was down; got: {:?}",
            out.irc_commands
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

        // Set up: link up, remote burst done, introduce pseudoclient.
        state.handle_irc_event(&S2SEvent::LinkUp, ts);
        state.handle_irc_event(&S2SEvent::BurstComplete, ts);
        let out = state.handle_discord_event(
            &DiscordEvent::MemberSnapshot {
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
        pm.introduce(
            42,
            "alice",
            "Alice",
            &["#test".to_string()],
            1000,
            DiscordPresence::Online,
        );
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

    /// External nicks (known_nicks) from a previous connection must be cleared
    /// on LinkDown.  If they survive, pseudoclient nick collision avoidance
    /// would incorrectly suffix nicks for users who quit while the link was
    /// down.
    #[test]
    fn link_down_clears_external_nicks() {
        let mut state = BridgeState::new(&test_config());
        let ts = 1_000_000;

        // Set up: link up (burst sent), then IRC user "alice" introduced.
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

        // "alice" is now a known external nick — pseudoclient would be suffixed.
        state.pm.introduce(
            42,
            "alice",
            "Alice",
            &["#test".to_string()],
            ts,
            DiscordPresence::Online,
        );
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
        state.pm.introduce(
            43,
            "alice",
            "Alice",
            &["#test".to_string()],
            ts,
            DiscordPresence::Online,
        );
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
            &DiscordEvent::MemberSnapshot {
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
        // Pseudoclient is marked needs_reintroduce but blocked by cooldown.
        assert!(
            state
                .pm
                .get_by_discord_id(8001)
                .is_some_and(|ps| ps.needs_reintroduce),
            "should be marked for deferred reintroduce"
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

    // --- Discord event IRC command gate ---

    #[test]
    fn discord_state_cmds_not_forwarded_after_link_down() {
        let mut state = BridgeState::new(&test_config());
        let ts = 1_000_000;

        // Link up, burst complete — phase is Ready.
        state.handle_irc_event(&S2SEvent::LinkUp, ts);

        // Link drops — phase returns to NotReady.
        state.handle_irc_event(
            &S2SEvent::LinkDown {
                reason: "test".into(),
            },
            ts,
        );

        // Discord event while link is down — should not produce IRC commands
        // but should still update state.
        let out = state.handle_discord_event(
            &DiscordEvent::MemberSnapshot {
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
        assert!(
            state.pm.get_by_discord_id(9001).is_some(),
            "pseudoclient state should be updated even when link is down"
        );
    }

    /// After IRC link drops and reconnects, existing pseudoclients must be
    /// re-introduced to the new link on BurstComplete.
    #[test]
    fn reconnect_rebursts_existing_pseudoclients() {
        let mut state = BridgeState::new(&test_config());
        let ts = 1_000_000;

        // Introduce a pseudoclient while link is not ready.
        state.handle_discord_event(
            &DiscordEvent::MemberSnapshot {
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
        let frank_uid = state.pm.get_by_discord_id(7777).unwrap().uid.clone();

        // First connect: burst sent on BurstComplete.
        let out = state.handle_irc_event(&S2SEvent::LinkUp, ts);
        assert!(
            out.irc_commands
                .iter()
                .any(|c| matches!(c, S2SCommand::IntroduceUser { .. })),
            "Frank should be introduced via burst on first connect"
        );

        // IRC link drops.
        state.handle_irc_event(
            &S2SEvent::LinkDown {
                reason: "test".into(),
            },
            ts + 100,
        );
        assert!(state.pm.get_by_discord_id(7777).is_some());

        // Reconnect: burst sent on BurstComplete.
        let out = state.handle_irc_event(&S2SEvent::LinkUp, ts + 200);

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

    /// Our burst (including EOS) is sent on BurstComplete.
    #[test]
    fn burst_sent_on_link_up() {
        let mut state = BridgeState::new(&test_config());
        let ts = 1_000_000;

        let out = state.handle_irc_event(&S2SEvent::LinkUp, ts);
        assert!(
            out.irc_commands
                .iter()
                .any(|c| matches!(c, S2SCommand::BurstComplete)),
            "LinkUp must send our EOS"
        );
    }

    /// KILL during burst window (before BurstComplete) defers reintroduction.
    #[test]
    fn kill_during_burst_deferred_to_burst_complete() {
        let mut config = test_config();
        config.pseudoclients.reintroduce_on_kill = true;
        let mut state = BridgeState::new(&config);
        let ts = 1_000_000;

        // Introduce pseudoclient while link is down, then link up.
        state.handle_discord_event(
            &DiscordEvent::MemberSnapshot {
                guild_id: 999,
                members: vec![MemberInfo {
                    user_id: 6001,
                    username: "victim".into(),
                    display_name: "Victim".into(),
                    presence: DiscordPresence::Online,
                }],
                channel_ids: vec![111],
                channel_names: std::collections::HashMap::new(),
                role_names: std::collections::HashMap::new(),
            },
            ts,
        );
        let out = state.handle_irc_event(&S2SEvent::LinkUp, ts);
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
            .expect("should burst pseudoclient");

        // KILL arrives before BurstComplete (nick collision from remote burst).
        let out = state.handle_irc_event(
            &S2SEvent::UserKilled {
                uid: old_uid,
                reason: "Nick collision".into(),
            },
            ts,
        );

        // Should NOT reintroduce immediately — defer to BurstComplete.
        assert!(
            !out.irc_commands
                .iter()
                .any(|c| matches!(c, S2SCommand::IntroduceUser { .. })),
            "KILL during burst window should not reintroduce immediately"
        );

        // Register the colliding external nick (as the remote burst would).
        state.handle_irc_event(
            &S2SEvent::UserIntroduced {
                uid: "001EXTAAA".into(),
                nick: "victim".into(),
                server_sid: "001".into(),
                realname: "External".into(),
                host: "example.com".into(),
                ident: "ext".into(),
            },
            ts,
        );

        // BurstComplete — should reintroduce with a suffixed nick.
        let out = state.handle_irc_event(&S2SEvent::BurstComplete, ts);
        let new_nick = out
            .irc_commands
            .iter()
            .find_map(|c| {
                if let S2SCommand::IntroduceUser { nick, .. } = c {
                    Some(nick.clone())
                } else {
                    None
                }
            })
            .expect("BurstComplete should reintroduce the killed pseudoclient");

        assert_ne!(
            new_nick, "victim",
            "reintroduced nick should be suffixed to avoid collision; got: {new_nick}"
        );
    }
}
