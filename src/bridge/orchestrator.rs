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
// Constants
// ---------------------------------------------------------------------------

/// A pseudoclient killed within this many seconds of a previous kill is not
/// reintroduced.  Prevents reintroduce loops when an operator is actively
/// killing a problem user.
const KILL_COOLDOWN_SECS: u64 = 30;

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
    /// The Discord bot's own user ID (set from `MemberSnapshot`).
    pub(crate) bot_user_id: u64,
    /// Persisted pseudoclient state loaded on startup.  Consumed entry-by-
    /// entry as users are introduced.
    pub(crate) seed_state: HashMap<u64, crate::persist::PersistedPseudoclient>,
    /// Whether pseudoclient state has been modified since the last save.
    pub(crate) state_dirty: bool,
}

impl BridgeState {
    /// Create a new bridge state from config, optionally with persisted
    /// pseudoclient state to restore on first `MemberSnapshot`.
    pub fn new(
        config: &Config,
        seed_state: HashMap<u64, crate::persist::PersistedPseudoclient>,
    ) -> Self {
        Self {
            config: config.clone(),
            bridge_map: BridgeMap::from_config(&config.bridges),
            pm: PseudoclientManager::new(&config.irc.sid, &config.pseudoclients.ident),
            irc_state: IrcState::default(),
            discord_state: DiscordState::default(),
            link_phase: LinkPhase::NotReady,
            remote_burst_done: false,
            kill_cooldowns: HashMap::new(),
            bot_user_id: 0,
            seed_state,
            state_dirty: false,
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
                } else if self.config.pseudoclients.dm_bridging
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
                self.prune_kill_cooldowns(now_ts);
                if self.kill_cooldowns.contains_key(&discord_id) {
                    tracing::warn!(
                        discord_id,
                        "not re-introducing killed pseudoclient — killed again within {KILL_COOLDOWN_SECS}s cooldown"
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

    /// Drop kill-cooldown entries older than `KILL_COOLDOWN_SECS`.
    fn prune_kill_cooldowns(&mut self, now_ts: u64) {
        self.kill_cooldowns
            .retain(|_, ts| now_ts.saturating_sub(*ts) < KILL_COOLDOWN_SECS);
    }

    /// Drop seed entries whose `last_active` is older than `offline_timeout`.
    /// Mirrors the offline-pseudoclient QUIT rule: a user who was idle past
    /// the threshold on the old bridge instance would have been removed, so
    /// their seed has no use now.
    ///
    /// Does nothing when the offline timeout is disabled (0) — matches the
    /// live-pseudoclient semantics.
    fn prune_stale_seeds(&mut self, now_ts: u64, offline_timeout: u64) {
        if offline_timeout == 0 {
            return;
        }
        self.seed_state
            .retain(|_, seed| now_ts.saturating_sub(seed.last_active) < offline_timeout);
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

        // Populate guild→irc-channel map and store bot user ID.
        if let DiscordEvent::MemberSnapshot {
            guild_id,
            channel_ids,
            bot_user_id,
            ..
        } = event
        {
            update_guild_irc_channels(
                &mut self.discord_state,
                &self.bridge_map,
                *guild_id,
                channel_ids,
            );
            self.bot_user_id = *bot_user_id;
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
                timestamp,
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
                    Some(*timestamp),
                    now_ts,
                    &resolver,
                );
                if !cmds.is_empty() {
                    self.state_dirty = true;
                }
                output.irc_commands.extend(cmds);
            }

            // Route Discord DMs to IRC.
            if let DiscordEvent::DmReceived {
                author_id,
                content,
                referenced_content,
                timestamp,
                ..
            } = event
                && self.config.pseudoclients.dm_bridging
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
                        self.pm.record_global_activity(*author_id, now_ts);
                        self.state_dirty = true;
                        output.irc_commands.push(S2SCommand::SendMessage {
                            from_uid,
                            target: target_uid,
                            text,
                            timestamp: Some(*timestamp),
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
        if !cmds.is_empty() {
            self.state_dirty = true;
        }
        if self.link_phase == LinkPhase::Ready {
            output.irc_commands.extend(cmds);
        }

        // Apply persisted state for any users that were just introduced.
        output.irc_commands.extend(self.apply_seeds(event, now_ts));

        output
    }

    /// Check for idle pseudoclients and emit PART/QUIT commands.
    ///
    /// # Clock behaviour
    ///
    /// All idle-timeout comparisons use wall-clock Unix seconds
    /// (`now_ts.saturating_sub(stored_ts)`).  Wall time is required because
    /// `went_offline_at` / `last_active` are persisted across restarts and
    /// seed restoration depends on comparing them to the new process's
    /// current time.
    ///
    /// Consequences of non-monotonic system-clock adjustments:
    /// - **Backward step** (e.g. NTP correction): `saturating_sub` clamps
    ///   to 0, so timeouts are deferred until the clock catches back up.
    ///   No pseudoclient state is lost.
    /// - **Forward jump** (VM resume, first NTP sync after long boot):
    ///   a batch of timeouts may fire in one tick.  Self-corrects on the
    ///   next tick — users come back on their next message.
    ///
    /// This is intentional.  Tracking a companion monotonic `Instant`
    /// alongside every wall-time field would let us distinguish "time
    /// elapsed" from "clock moved", but `Instant` can't be persisted, so
    /// seeds would still have to fall back to wall time after a restart.
    /// The extra complexity isn't justified for a bridge whose timeouts
    /// are days / weeks.
    pub fn check_idle_timeouts(&mut self, now_ts: u64) -> HandlerOutput {
        let offline_timeout = self.config.pseudoclients.offline_timeout_secs;

        // Opportunistic GC: stale kill cooldowns are only pruned when a new
        // KILL arrives.  Piggyback on the idle tick so long-lived entries
        // don't leak for users who were killed once and never again.
        self.prune_kill_cooldowns(now_ts);
        // Also drop seed entries whose users never showed up in any event.
        // A seed is equivalent to a live pseudoclient that has been offline
        // since `last_active`, so apply the same timeout.
        self.prune_stale_seeds(now_ts, offline_timeout);

        let mut output = HandlerOutput::default();
        let channel_timeout = self.config.pseudoclients.channel_idle_timeout_secs;
        let bot_id = self.bot_user_id;

        // Collect actions first to avoid borrow issues.
        let mut parts: Vec<(u64, String)> = Vec::new();
        let mut quits: Vec<u64> = Vec::new();

        for state in self.pm.iter_states() {
            if state.discord_user_id == bot_id || state.needs_reintroduce {
                continue;
            }

            // Offline + globally idle: QUIT (skip channel checks — QUIT
            // supersedes PART).
            if offline_timeout > 0
                && state.presence == DiscordPresence::Offline
                && state
                    .went_offline_at
                    .is_some_and(|t| now_ts.saturating_sub(t) >= offline_timeout)
                && now_ts.saturating_sub(state.last_active) >= offline_timeout
            {
                quits.push(state.discord_user_id);
                continue;
            }

            // Channel idle: PART from channels where inactive.
            if channel_timeout > 0 {
                for ch in &state.channels {
                    let last = state.channel_last_active.get(ch).copied().unwrap_or(0);
                    if now_ts.saturating_sub(last) >= channel_timeout {
                        parts.push((state.discord_user_id, ch.clone()));
                    }
                }
            }
        }

        // Apply PARTs.
        for (discord_id, channel) in parts {
            if let Some(ps) = self.pm.get_by_discord_id_mut(discord_id) {
                let uid = ps.uid.clone();
                ps.channels.retain(|c| c != &channel);
                ps.channel_last_active.remove(&channel);
                output.irc_commands.push(S2SCommand::PartChannel {
                    uid,
                    channel,
                    reason: Some("Idle timeout".to_string()),
                });
            }
        }

        // Apply QUITs.
        for discord_id in quits {
            if let Some(ps) = self.pm.get_by_discord_id(discord_id) {
                let uid = ps.uid.clone();
                output.irc_commands.push(S2SCommand::QuitUser {
                    uid,
                    reason: "Offline idle timeout".to_string(),
                });
            }
            self.pm.quit(discord_id, "Offline idle timeout");
        }

        if !output.irc_commands.is_empty() {
            self.state_dirty = true;
        }

        output
    }

    /// Apply persisted seed state for users referenced by a Discord event.
    ///
    /// For each user with an unconsumed seed entry:
    /// - If already introduced (online in snapshot, or on-demand), restore
    ///   channels and timestamps.
    /// - If not yet introduced (offline in snapshot), introduce them now
    ///   with their persisted channels and AWAY :Offline.
    fn apply_seeds(&mut self, event: &DiscordEvent, now_ts: u64) -> Vec<S2SCommand> {
        if self.seed_state.is_empty() {
            return vec![];
        }

        // Collect (user_id, username, display_name) for users referenced
        // by this event that have seed entries.  Bot is excluded — it
        // eagerly joins all channels and should not use persisted state.
        let bot_id = self.bot_user_id;
        let candidates: Vec<(u64, String, String)> = match event {
            DiscordEvent::MemberSnapshot { members, .. } => members
                .iter()
                .filter(|m| m.user_id != bot_id)
                .map(|m| (m.user_id, m.username.clone(), m.display_name.clone()))
                .collect(),
            DiscordEvent::PresenceUpdated {
                user_id,
                username,
                display_name,
                ..
            } => {
                let uname = username.clone().unwrap_or_default();
                let dname = display_name.clone().unwrap_or_default();
                vec![(*user_id, uname, dname)]
            }
            DiscordEvent::MessageReceived {
                author_id,
                author_name,
                author_display_name,
                ..
            } => vec![(*author_id, author_name.clone(), author_display_name.clone())],
            _ => return vec![],
        };

        let mut cmds = Vec::new();
        for (user_id, username, display_name) in candidates {
            // If not yet in PM (offline user skipped by apply_discord_event),
            // introduce them now so seed channels can be applied.
            if self.pm.get_by_discord_id(user_id).is_none() {
                cmds.extend(introduce_pseudoclient(
                    &mut self.pm,
                    &self.irc_state,
                    user_id,
                    &username,
                    &display_name,
                    &[],
                    DiscordPresence::Offline,
                    now_ts,
                ));
            }
            if let Some(seed) = self.seed_state.remove(&user_id) {
                cmds.extend(self.apply_seed(user_id, seed, now_ts));
            }
        }
        if !cmds.is_empty() {
            self.state_dirty = true;
        }
        cmds
    }

    /// Apply a single seed entry: restore channels and timestamps.
    fn apply_seed(
        &mut self,
        user_id: u64,
        seed: crate::persist::PersistedPseudoclient,
        now_ts: u64,
    ) -> Vec<S2SCommand> {
        let Some(state) = self.pm.get_by_discord_id_mut(user_id) else {
            return vec![];
        };

        state.last_active = seed.last_active;
        state.channel_last_active = seed.channel_last_active;
        if state.presence == DiscordPresence::Offline {
            state.went_offline_at = seed.went_offline_at;
        }

        let uid = state.uid.clone();
        let mut cmds = Vec::new();
        for channel in seed.channels {
            if state.channels.contains(&channel) {
                continue;
            }
            state.channels.push(channel.clone());
            let ts = self.irc_state.ts_for_channel(&channel).unwrap_or(now_ts);
            cmds.push(S2SCommand::JoinChannel {
                uid: uid.clone(),
                channel,
                ts,
            });
        }
        cmds
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
                dm_bridging: true,
                channel_idle_timeout_secs: 0,
                offline_timeout_secs: 0,
                state_file: None,
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
        let mut state = BridgeState::new(&test_config(), HashMap::new());
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
                bot_user_id: 0,
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
        let mut state = BridgeState::new(&test_config(), HashMap::new());
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
                bot_user_id: 0,
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
        let mut state = BridgeState::new(&test_config(), HashMap::new());
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
                bot_user_id: 0,
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
                timestamp: chrono::DateTime::<chrono::Utc>::from_timestamp(1_700_000_000, 0)
                    .unwrap(),
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
        let mut state = BridgeState::new(&test_config(), HashMap::new());
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
                bot_user_id: 0,
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
        let mut state = BridgeState::new(&test_config(), HashMap::new());
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
                bot_user_id: 0,
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
        let mut state = BridgeState::new(&config, HashMap::new());
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
                bot_user_id: 0,
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
        let mut state = BridgeState::new(&test_config(), HashMap::new());
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

    #[test]
    fn link_down_clears_needs_reintroduce_flags() {
        let mut config = test_config();
        config.pseudoclients.reintroduce_on_kill = true;
        let mut state = BridgeState::new(&config, HashMap::new());
        let ts = 1_000_000;

        // Introduce a pseudoclient.
        state.handle_discord_event(
            &DiscordEvent::MemberSnapshot {
                guild_id: 999,
                members: vec![MemberInfo {
                    user_id: 4001,
                    username: "alice".into(),
                    display_name: "Alice".into(),
                    presence: DiscordPresence::Online,
                }],
                channel_ids: vec![111],
                channel_names: std::collections::HashMap::new(),
                role_names: std::collections::HashMap::new(),
                bot_user_id: 0,
            },
            ts,
        );
        let out = state.handle_irc_event(&S2SEvent::LinkUp, ts);
        let uid = out
            .irc_commands
            .iter()
            .find_map(|c| {
                if let S2SCommand::IntroduceUser { uid, .. } = c {
                    Some(uid.clone())
                } else {
                    None
                }
            })
            .expect("should introduce alice");

        // Kill during burst window — deferred, needs_reintroduce set.
        state.handle_irc_event(
            &S2SEvent::UserKilled {
                uid,
                reason: "test".into(),
            },
            ts,
        );
        assert!(
            state
                .pm
                .get_by_discord_id(4001)
                .is_some_and(|ps| ps.needs_reintroduce),
        );

        // LinkDown should clear the flag.
        state.handle_irc_event(
            &S2SEvent::LinkDown {
                reason: "test".into(),
            },
            ts,
        );
        assert!(
            !state
                .pm
                .get_by_discord_id(4001)
                .is_some_and(|ps| ps.needs_reintroduce),
            "LinkDown should clear needs_reintroduce flags"
        );
    }

    // --- IRC→Discord message relay ---

    /// Helper: set up a bridge with link up, burst complete, and a pseudoclient.
    fn setup_bridge_with_pseudoclient() -> (BridgeState, String) {
        let mut state = BridgeState::new(&test_config(), HashMap::new());
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
        let mut state = BridgeState::new(&config, HashMap::new());
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
                bot_user_id: 0,
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

    #[test]
    fn kill_at_cooldown_boundary_allows_reintroduce() {
        let mut config = test_config();
        config.pseudoclients.reintroduce_on_kill = true;
        let mut state = BridgeState::new(&config, HashMap::new());
        let ts = 1_000_000;

        state.handle_irc_event(&S2SEvent::LinkUp, ts);
        state.handle_irc_event(&S2SEvent::BurstComplete, ts);
        state.handle_discord_event(
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
                bot_user_id: 0,
            },
            ts,
        );
        let uid1 = state
            .pm
            .get_by_discord_id(8001)
            .expect("introduced")
            .uid
            .clone();

        // First kill — reintroduces.
        let out = state.handle_irc_event(
            &S2SEvent::UserKilled {
                uid: uid1,
                reason: "kill".into(),
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

        // Second kill at exactly 30 seconds — cooldown has expired.
        let out = state.handle_irc_event(
            &S2SEvent::UserKilled {
                uid: uid2,
                reason: "kill at boundary".into(),
            },
            ts + 30,
        );
        assert!(
            out.irc_commands
                .iter()
                .any(|c| matches!(c, S2SCommand::IntroduceUser { .. })),
            "kill at exactly 30s should reintroduce (cooldown expired)"
        );
    }

    #[test]
    fn idle_tick_prunes_expired_kill_cooldowns() {
        let mut config = test_config();
        config.pseudoclients.reintroduce_on_kill = true;
        let mut state = BridgeState::new(&config, HashMap::new());
        let ts = 1_000_000;

        state.handle_irc_event(&S2SEvent::LinkUp, ts);
        state.handle_irc_event(&S2SEvent::BurstComplete, ts);
        state.handle_discord_event(
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
                bot_user_id: 0,
            },
            ts,
        );
        let uid = state
            .pm
            .get_by_discord_id(8001)
            .expect("introduced")
            .uid
            .clone();

        // First kill — adds to cooldowns at ts.
        state.handle_irc_event(
            &S2SEvent::UserKilled {
                uid,
                reason: "kill".into(),
            },
            ts,
        );
        assert!(state.kill_cooldowns.contains_key(&8001));

        // Idle tick while cooldown is still active: entry must survive.
        state.check_idle_timeouts(ts + KILL_COOLDOWN_SECS - 1);
        assert!(
            state.kill_cooldowns.contains_key(&8001),
            "entry within cooldown must not be pruned"
        );

        // Idle tick past the cooldown: entry must be gone.
        state.check_idle_timeouts(ts + KILL_COOLDOWN_SECS);
        assert!(
            !state.kill_cooldowns.contains_key(&8001),
            "expired cooldown must be pruned on idle tick"
        );
    }

    #[test]
    fn idle_tick_prunes_stale_seed_entries() {
        use crate::persist::PersistedPseudoclient;

        let mut config = test_config();
        config.pseudoclients.offline_timeout_secs = 200;
        let mut seed = HashMap::new();
        seed.insert(
            42,
            PersistedPseudoclient {
                channels: vec!["#test".to_string()],
                last_active: 1_000_000,
                channel_last_active: HashMap::new(),
                went_offline_at: None,
            },
        );
        seed.insert(
            99,
            PersistedPseudoclient {
                channels: vec!["#test".to_string()],
                last_active: 2_000_000,
                channel_last_active: HashMap::new(),
                went_offline_at: None,
            },
        );

        let mut state = BridgeState::new(&config, seed);

        // Tick at a time past user 42's last_active + offline_timeout but
        // within user 99's window.
        state.check_idle_timeouts(2_000_100);

        assert!(
            !state.seed_state.contains_key(&42),
            "stale seed must be pruned"
        );
        assert!(
            state.seed_state.contains_key(&99),
            "fresh seed must survive"
        );
    }

    #[test]
    fn seed_prune_boundary_exactly_at_timeout() {
        use crate::persist::PersistedPseudoclient;

        // Seed whose last_active is exactly `offline_timeout` in the past
        // must be pruned (the retain predicate is `delta < timeout`).
        let mut config = test_config();
        config.pseudoclients.offline_timeout_secs = 200;
        let mut seed = HashMap::new();
        seed.insert(
            42,
            PersistedPseudoclient {
                channels: vec!["#test".to_string()],
                last_active: 1_000_000,
                channel_last_active: HashMap::new(),
                went_offline_at: None,
            },
        );
        seed.insert(
            43,
            PersistedPseudoclient {
                channels: vec!["#test".to_string()],
                last_active: 1_000_001, // 1 second newer
                channel_last_active: HashMap::new(),
                went_offline_at: None,
            },
        );
        let mut state = BridgeState::new(&config, seed);

        // delta(42) = 200 exactly → prune.
        // delta(43) = 199 → keep.
        state.check_idle_timeouts(1_000_200);

        assert!(
            !state.seed_state.contains_key(&42),
            "seed at exactly offline_timeout boundary must be pruned"
        );
        assert!(
            state.seed_state.contains_key(&43),
            "seed one second fresher than boundary must be kept"
        );
    }

    #[test]
    fn idle_tick_does_not_prune_seeds_when_offline_timeout_disabled() {
        use crate::persist::PersistedPseudoclient;

        let mut config = test_config();
        config.pseudoclients.offline_timeout_secs = 0; // disabled
        let mut seed = HashMap::new();
        seed.insert(
            42,
            PersistedPseudoclient {
                channels: vec!["#test".to_string()],
                last_active: 1, // extremely old
                channel_last_active: HashMap::new(),
                went_offline_at: None,
            },
        );
        let mut state = BridgeState::new(&config, seed);

        state.check_idle_timeouts(1_000_000_000);

        assert!(
            state.seed_state.contains_key(&42),
            "seed pruning must mirror offline timeout disable"
        );
    }

    // --- reload_config ---

    #[test]
    fn reload_config_with_changed_bridges_produces_command() {
        let mut state = BridgeState::new(&test_config(), HashMap::new());
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
        let mut state = BridgeState::new(&test_config(), HashMap::new());
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
        let mut state = BridgeState::new(&test_config(), HashMap::new());
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
                bot_user_id: 0,
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
        let mut state = BridgeState::new(&test_config(), HashMap::new());
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
                bot_user_id: 0,
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
        let mut state = BridgeState::new(&test_config(), HashMap::new());
        let ts = 1_000_000;

        let out = state.handle_irc_event(&S2SEvent::LinkUp, ts);
        assert!(
            out.irc_commands
                .iter()
                .any(|c| matches!(c, S2SCommand::BurstComplete)),
            "LinkUp must send our EOS"
        );
    }

    // --- Idle timeouts ---

    #[test]
    fn channel_idle_timeout_parts_inactive_user() {
        let mut config = test_config();
        config.pseudoclients.channel_idle_timeout_secs = 100;
        let mut state = BridgeState::new(&config, HashMap::new());
        let ts = 1_000_000;

        // Introduce user, link up.
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
                bot_user_id: 0,
            },
            ts,
        );
        state.handle_irc_event(&S2SEvent::LinkUp, ts);

        // Send a message to join #test lazily.
        state.handle_discord_event(
            &DiscordEvent::MessageReceived {
                channel_id: 111,
                author_id: 42,
                author_name: "alice".into(),
                author_display_name: "Alice".into(),
                content: "hello".into(),
                attachments: vec![],
                timestamp: chrono::DateTime::<chrono::Utc>::from_timestamp(1_700_000_000, 0)
                    .unwrap(),
            },
            ts,
        );
        assert!(
            state
                .pm
                .get_by_discord_id(42)
                .unwrap()
                .channels
                .contains(&"#test".to_string())
        );

        // Check at ts + 99: not yet expired.
        let out = state.check_idle_timeouts(ts + 99);
        assert!(
            !out.irc_commands
                .iter()
                .any(|c| matches!(c, S2SCommand::PartChannel { .. })),
            "should not PART before timeout"
        );

        // Check at ts + 100: expired.
        let out = state.check_idle_timeouts(ts + 100);
        assert!(
            out.irc_commands
                .iter()
                .any(|c| matches!(c, S2SCommand::PartChannel { .. })),
            "should PART after timeout; got: {:?}",
            out.irc_commands
        );
        assert!(
            state.pm.get_by_discord_id(42).unwrap().channels.is_empty(),
            "channel should be removed from PM"
        );
    }

    #[test]
    fn offline_timeout_quits_idle_offline_user() {
        let mut config = test_config();
        config.pseudoclients.offline_timeout_secs = 200;
        let mut state = BridgeState::new(&config, HashMap::new());
        let ts = 1_000_000;

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
                bot_user_id: 0,
            },
            ts,
        );
        state.handle_irc_event(&S2SEvent::LinkUp, ts);

        // User goes offline.
        state.handle_discord_event(
            &DiscordEvent::PresenceUpdated {
                user_id: 42,
                guild_id: 999,
                presence: DiscordPresence::Offline,
                username: Some("alice".into()),
                display_name: Some("Alice".into()),
            },
            ts + 10,
        );

        // Check at ts + 209: not yet expired (offline for 199s, need 200).
        let out = state.check_idle_timeouts(ts + 209);
        assert!(
            !out.irc_commands
                .iter()
                .any(|c| matches!(c, S2SCommand::QuitUser { .. })),
            "should not QUIT before timeout"
        );

        // Check at ts + 210: expired (offline for 200s).
        let out = state.check_idle_timeouts(ts + 210);
        assert!(
            out.irc_commands
                .iter()
                .any(|c| matches!(c, S2SCommand::QuitUser { .. })),
            "should QUIT after timeout; got: {:?}",
            out.irc_commands
        );
        assert!(
            state.pm.get_by_discord_id(42).is_none(),
            "pseudoclient should be removed from PM"
        );
    }

    #[test]
    fn offline_timeout_does_not_quit_active_offline_user() {
        let mut config = test_config();
        config.pseudoclients.offline_timeout_secs = 200;
        let mut state = BridgeState::new(&config, HashMap::new());
        let ts = 1_000_000;

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
                bot_user_id: 0,
            },
            ts,
        );
        state.handle_irc_event(&S2SEvent::LinkUp, ts);

        // User goes offline but sends a message (invisible mode).
        state.handle_discord_event(
            &DiscordEvent::PresenceUpdated {
                user_id: 42,
                guild_id: 999,
                presence: DiscordPresence::Offline,
                username: Some("alice".into()),
                display_name: Some("Alice".into()),
            },
            ts + 10,
        );
        // Message at ts + 150 — recently active.
        state.handle_discord_event(
            &DiscordEvent::MessageReceived {
                channel_id: 111,
                author_id: 42,
                author_name: "alice".into(),
                author_display_name: "Alice".into(),
                content: "still here".into(),
                attachments: vec![],
                timestamp: chrono::DateTime::<chrono::Utc>::from_timestamp(1_700_000_000, 0)
                    .unwrap(),
            },
            ts + 150,
        );

        // Check at ts + 210: offline for 200s but active 60s ago.
        let out = state.check_idle_timeouts(ts + 210);
        assert!(
            !out.irc_commands
                .iter()
                .any(|c| matches!(c, S2SCommand::QuitUser { .. })),
            "should NOT quit an active-but-offline user"
        );
    }

    #[test]
    fn went_offline_at_cleared_when_coming_online() {
        let mut state = BridgeState::new(&test_config(), HashMap::new());
        let ts = 1_000_000;

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
                bot_user_id: 0,
            },
            ts,
        );
        // Go offline.
        state.handle_discord_event(
            &DiscordEvent::PresenceUpdated {
                user_id: 42,
                guild_id: 999,
                presence: DiscordPresence::Offline,
                username: Some("alice".into()),
                display_name: Some("Alice".into()),
            },
            ts + 10,
        );
        assert!(
            state
                .pm
                .get_by_discord_id(42)
                .unwrap()
                .went_offline_at
                .is_some()
        );

        // Come back online.
        state.handle_discord_event(
            &DiscordEvent::PresenceUpdated {
                user_id: 42,
                guild_id: 999,
                presence: DiscordPresence::Online,
                username: Some("alice".into()),
                display_name: Some("Alice".into()),
            },
            ts + 20,
        );
        assert!(
            state
                .pm
                .get_by_discord_id(42)
                .unwrap()
                .went_offline_at
                .is_none(),
            "went_offline_at should be cleared when coming online"
        );
    }

    #[test]
    fn zero_timeout_disables_idle_checks() {
        let mut config = test_config();
        config.pseudoclients.channel_idle_timeout_secs = 0;
        config.pseudoclients.offline_timeout_secs = 0;
        let mut state = BridgeState::new(&config, HashMap::new());
        let ts = 1_000_000;

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
                bot_user_id: 0,
            },
            ts,
        );
        state.handle_irc_event(&S2SEvent::LinkUp, ts);
        state.handle_discord_event(
            &DiscordEvent::MessageReceived {
                channel_id: 111,
                author_id: 42,
                author_name: "alice".into(),
                author_display_name: "Alice".into(),
                content: "hi".into(),
                attachments: vec![],
                timestamp: chrono::DateTime::<chrono::Utc>::from_timestamp(1_700_000_000, 0)
                    .unwrap(),
            },
            ts,
        );
        // Go offline.
        state.handle_discord_event(
            &DiscordEvent::PresenceUpdated {
                user_id: 42,
                guild_id: 999,
                presence: DiscordPresence::Offline,
                username: Some("alice".into()),
                display_name: Some("Alice".into()),
            },
            ts,
        );

        // Way past any reasonable timeout — but both are disabled (0).
        let out = state.check_idle_timeouts(ts + 999_999_999);
        assert!(
            out.irc_commands.is_empty(),
            "zero timeout should disable all idle checks"
        );
    }

    #[test]
    fn dm_activity_prevents_offline_timeout() {
        let mut config = test_config();
        config.pseudoclients.offline_timeout_secs = 200;
        config.pseudoclients.dm_bridging = true;
        let mut state = BridgeState::new(&config, HashMap::new());
        let ts = 1_000_000;

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
                bot_user_id: 0,
            },
            ts,
        );
        state.handle_irc_event(&S2SEvent::LinkUp, ts);

        // Go offline.
        state.handle_discord_event(
            &DiscordEvent::PresenceUpdated {
                user_id: 42,
                guild_id: 999,
                presence: DiscordPresence::Offline,
                username: Some("alice".into()),
                display_name: Some("Alice".into()),
            },
            ts + 10,
        );

        // DM activity at ts + 150 — updates last_active globally.
        state.pm.record_global_activity(42, ts + 150);

        // At ts + 210: offline for 200s but globally active 60s ago.
        let out = state.check_idle_timeouts(ts + 210);
        assert!(
            !out.irc_commands
                .iter()
                .any(|c| matches!(c, S2SCommand::QuitUser { .. })),
            "DM activity should prevent offline timeout"
        );

        // At ts + 350: offline for 340s AND last active 200s ago.
        let out = state.check_idle_timeouts(ts + 350);
        assert!(
            out.irc_commands
                .iter()
                .any(|c| matches!(c, S2SCommand::QuitUser { .. })),
            "should QUIT after both offline and activity timeouts expire"
        );
    }

    #[test]
    fn bot_exempt_from_idle_timeouts() {
        let mut config = test_config();
        config.pseudoclients.channel_idle_timeout_secs = 1;
        config.pseudoclients.offline_timeout_secs = 1;
        let mut state = BridgeState::new(&config, HashMap::new());
        let ts = 1_000_000;

        state.handle_discord_event(
            &DiscordEvent::MemberSnapshot {
                guild_id: 999,
                members: vec![MemberInfo {
                    user_id: 99,
                    username: "bridgebot".into(),
                    display_name: "BridgeBot".into(),
                    presence: DiscordPresence::Online,
                }],
                channel_ids: vec![111],
                channel_names: std::collections::HashMap::new(),
                role_names: std::collections::HashMap::new(),
                bot_user_id: 99,
            },
            ts,
        );
        state.handle_irc_event(&S2SEvent::LinkUp, ts);

        // Way past timeout.
        let out = state.check_idle_timeouts(ts + 1_000_000);
        assert!(
            out.irc_commands.is_empty(),
            "bot should be exempt from all timeouts"
        );
    }

    /// KILL during burst window (before BurstComplete) defers reintroduction.
    #[test]
    fn kill_during_burst_deferred_to_burst_complete() {
        let mut config = test_config();
        config.pseudoclients.reintroduce_on_kill = true;
        let mut state = BridgeState::new(&config, HashMap::new());
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
                bot_user_id: 0,
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

    // --- State persistence / seed map ---

    #[test]
    fn seed_state_restores_channels_on_member_snapshot() {
        use crate::persist::PersistedPseudoclient;

        let mut seed = HashMap::new();
        seed.insert(
            42,
            PersistedPseudoclient {
                channels: vec!["#test".to_string()],
                last_active: 500_000,
                channel_last_active: {
                    let mut m = HashMap::new();
                    m.insert("#test".to_string(), 500_000u64);
                    m
                },
                went_offline_at: Some(600_000),
            },
        );

        let mut state = BridgeState::new(&test_config(), seed);
        let ts = 1_000_000;

        state.handle_irc_event(&S2SEvent::LinkUp, ts);

        let out = state.handle_discord_event(
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
                bot_user_id: 0,
            },
            ts,
        );

        // Pseudoclient should have restored channel membership.
        let ps = state
            .pm
            .get_by_discord_id(42)
            .expect("should be introduced");
        assert_eq!(
            ps.channels,
            vec!["#test"],
            "channel should be restored from seed"
        );
        assert_eq!(ps.last_active, 500_000, "last_active should be restored");
        assert_eq!(
            ps.went_offline_at, None,
            "went_offline_at should NOT be restored for Online users"
        );
        assert_eq!(
            ps.channel_last_active.get("#test"),
            Some(&500_000),
            "channel_last_active should be restored"
        );

        // Should have emitted a JoinChannel for the restored channel.
        assert!(
            out.irc_commands.iter().any(|c| matches!(
                c,
                S2SCommand::JoinChannel { channel, .. } if channel == "#test"
            )),
            "should emit JoinChannel for restored channel; got: {:?}",
            out.irc_commands
        );

        // Seed application should mark state as dirty.
        assert!(state.state_dirty, "seed application should set dirty flag");
    }

    #[test]
    fn seed_state_filters_removed_bridge_channels() {
        use crate::persist::PersistedPseudoclient;

        let mut seed = HashMap::new();
        seed.insert(
            42,
            PersistedPseudoclient {
                channels: vec!["#test".to_string(), "#removed".to_string()],
                last_active: 500_000,
                channel_last_active: HashMap::new(),
                went_offline_at: None,
            },
        );

        // #removed is not in the bridge config, so it should be filtered
        // out during into_seed_map. Simulate that by pre-filtering.
        let valid_channels = vec!["#test"];
        let filtered_seed = crate::persist::into_seed_map(
            crate::persist::PersistedState {
                version: 1,
                pseudoclients: seed.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
            },
            &valid_channels,
        );

        let mut state = BridgeState::new(&test_config(), filtered_seed);
        let ts = 1_000_000;

        state.handle_irc_event(&S2SEvent::LinkUp, ts);

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
                bot_user_id: 0,
            },
            ts,
        );

        let ps = state
            .pm
            .get_by_discord_id(42)
            .expect("should be introduced");
        assert_eq!(
            ps.channels,
            vec!["#test"],
            "#removed should be filtered out"
        );
    }

    #[test]
    fn seed_state_ignored_for_bot_user() {
        use crate::persist::PersistedPseudoclient;

        let mut seed = HashMap::new();
        seed.insert(
            99,
            PersistedPseudoclient {
                channels: vec![],
                last_active: 1,
                channel_last_active: HashMap::new(),
                went_offline_at: Some(999),
            },
        );

        let mut state = BridgeState::new(&test_config(), seed);
        let ts = 1_000_000;

        state.handle_irc_event(&S2SEvent::LinkUp, ts);

        state.handle_discord_event(
            &DiscordEvent::MemberSnapshot {
                guild_id: 999,
                members: vec![MemberInfo {
                    user_id: 99,
                    username: "botuser".into(),
                    display_name: "Bot".into(),
                    presence: DiscordPresence::Online,
                }],
                channel_ids: vec![111],
                channel_names: std::collections::HashMap::new(),
                role_names: std::collections::HashMap::new(),
                bot_user_id: 99,
            },
            ts,
        );

        // Bot should use fresh state, not seed.
        let ps = state
            .pm
            .get_by_discord_id(99)
            .expect("bot should be introduced");
        assert_ne!(
            ps.went_offline_at,
            Some(999),
            "bot should not use seed went_offline_at"
        );
        assert_eq!(
            ps.last_active, ts,
            "bot should use current timestamp, not seed"
        );
    }

    #[test]
    fn seed_state_introduces_offline_member_with_persisted_channels() {
        use crate::persist::PersistedPseudoclient;

        let mut seed = HashMap::new();
        seed.insert(
            42,
            PersistedPseudoclient {
                channels: vec!["#test".to_string()],
                last_active: 500_000,
                channel_last_active: HashMap::new(),
                went_offline_at: Some(600_000),
            },
        );

        let mut state = BridgeState::new(&test_config(), seed);
        let ts = 1_000_000;

        state.handle_irc_event(&S2SEvent::LinkUp, ts);

        // Offline member WITH seed — should be introduced with persisted
        // channels and AWAY :Offline.
        let out = state.handle_discord_event(
            &DiscordEvent::MemberSnapshot {
                guild_id: 999,
                members: vec![MemberInfo {
                    user_id: 42,
                    username: "alice".into(),
                    display_name: "Alice".into(),
                    presence: DiscordPresence::Offline,
                }],
                channel_ids: vec![111],
                channel_names: std::collections::HashMap::new(),
                role_names: std::collections::HashMap::new(),
                bot_user_id: 0,
            },
            ts,
        );

        let ps = state
            .pm
            .get_by_discord_id(42)
            .expect("offline user with seed should be introduced");
        assert_eq!(
            ps.channels,
            vec!["#test"],
            "seed channels should be restored"
        );
        assert_eq!(
            ps.went_offline_at,
            Some(600_000),
            "went_offline_at should be restored for Offline user"
        );
        assert!(
            out.irc_commands.iter().any(|c| matches!(
                c,
                S2SCommand::JoinChannel { channel, .. } if channel == "#test"
            )),
            "should emit JoinChannel for restored channel; got: {:?}",
            out.irc_commands
        );
        assert!(
            out.irc_commands
                .iter()
                .any(|c| matches!(c, S2SCommand::SetAway { reason, .. } if reason == "Offline")),
            "should emit SetAway for offline user; got: {:?}",
            out.irc_commands
        );
    }

    #[test]
    fn offline_member_without_seed_not_introduced() {
        // Offline users with NO persisted state should still be skipped.
        let mut state = BridgeState::new(&test_config(), HashMap::new());
        let ts = 1_000_000;

        state.handle_irc_event(&S2SEvent::LinkUp, ts);

        state.handle_discord_event(
            &DiscordEvent::MemberSnapshot {
                guild_id: 999,
                members: vec![MemberInfo {
                    user_id: 42,
                    username: "alice".into(),
                    display_name: "Alice".into(),
                    presence: DiscordPresence::Offline,
                }],
                channel_ids: vec![111],
                channel_names: std::collections::HashMap::new(),
                role_names: std::collections::HashMap::new(),
                bot_user_id: 0,
            },
            ts,
        );

        assert!(
            state.pm.get_by_discord_id(42).is_none(),
            "offline user without seed should not be introduced"
        );
    }

    /// User not in MemberSnapshot (large guild chunking) but has persisted
    /// state.  On-demand introduction via message should consume the seed.
    #[test]
    fn seed_state_applied_on_demand_for_unchunked_user() {
        use crate::persist::PersistedPseudoclient;

        let mut seed = HashMap::new();
        seed.insert(
            42,
            PersistedPseudoclient {
                channels: vec!["#test".to_string()],
                last_active: 500_000,
                channel_last_active: HashMap::new(),
                went_offline_at: None,
            },
        );

        let mut state = BridgeState::new(&test_config(), seed);
        let ts = 1_000_000;

        // MemberSnapshot does NOT include user 42.
        state.handle_discord_event(
            &DiscordEvent::MemberSnapshot {
                guild_id: 999,
                members: vec![],
                channel_ids: vec![111],
                channel_names: std::collections::HashMap::new(),
                role_names: std::collections::HashMap::new(),
                bot_user_id: 0,
            },
            ts,
        );
        state.handle_irc_event(&S2SEvent::LinkUp, ts);

        assert!(state.pm.get_by_discord_id(42).is_none());
        assert!(!state.seed_state.is_empty());

        // User sends a message — on-demand introduction should consume seed.
        let out = state.handle_discord_event(
            &DiscordEvent::MessageReceived {
                channel_id: 111,
                author_id: 42,
                author_name: "alice".into(),
                author_display_name: "Alice".into(),
                content: "hello".into(),
                attachments: vec![],
                timestamp: chrono::DateTime::<chrono::Utc>::from_timestamp(1_700_000_000, 0)
                    .unwrap(),
            },
            ts + 10,
        );

        let ps = state
            .pm
            .get_by_discord_id(42)
            .expect("should be introduced on demand");
        assert!(
            ps.channels.contains(&"#test".to_string()),
            "seed channels should be restored; got: {:?}",
            ps.channels
        );
        assert_eq!(
            ps.last_active, 500_000,
            "seed last_active should be restored"
        );
        assert!(
            out.irc_commands.iter().any(|c| matches!(
                c,
                S2SCommand::JoinChannel { channel, .. } if channel == "#test"
            )),
            "should emit JoinChannel for restored channel; got: {:?}",
            out.irc_commands
        );
    }

    #[test]
    fn seed_state_applied_via_presence_updated() {
        use crate::persist::PersistedPseudoclient;

        let mut seed = HashMap::new();
        seed.insert(
            42,
            PersistedPseudoclient {
                channels: vec!["#test".to_string()],
                last_active: 500_000,
                channel_last_active: HashMap::new(),
                went_offline_at: None,
            },
        );

        let mut state = BridgeState::new(&test_config(), seed);
        let ts = 1_000_000;

        // MemberSnapshot without user 42 (large guild, offline not included).
        state.handle_discord_event(
            &DiscordEvent::MemberSnapshot {
                guild_id: 999,
                members: vec![],
                channel_ids: vec![111],
                channel_names: std::collections::HashMap::new(),
                role_names: std::collections::HashMap::new(),
                bot_user_id: 0,
            },
            ts,
        );
        state.handle_irc_event(&S2SEvent::LinkUp, ts);

        // User comes online via PresenceUpdated — seed should apply.
        let out = state.handle_discord_event(
            &DiscordEvent::PresenceUpdated {
                user_id: 42,
                guild_id: 999,
                presence: DiscordPresence::Online,
                username: Some("alice".into()),
                display_name: Some("Alice".into()),
            },
            ts + 10,
        );

        let ps = state
            .pm
            .get_by_discord_id(42)
            .expect("should be introduced");
        assert_eq!(
            ps.channels,
            vec!["#test"],
            "seed channels applied via PresenceUpdated"
        );
        assert!(
            out.irc_commands.iter().any(|c| matches!(
                c,
                S2SCommand::JoinChannel { channel, .. } if channel == "#test"
            )),
            "should emit JoinChannel; got: {:?}",
            out.irc_commands
        );
    }

    #[test]
    fn seed_only_path_sets_dirty_flag() {
        use crate::persist::PersistedPseudoclient;

        // Offline user with seed: apply_discord_event skips them (no cmds),
        // but apply_seeds introduces them (cmds).  Only apply_seeds should
        // set dirty.
        let mut seed = HashMap::new();
        seed.insert(
            42,
            PersistedPseudoclient {
                channels: vec!["#test".to_string()],
                last_active: 500_000,
                channel_last_active: HashMap::new(),
                went_offline_at: None,
            },
        );

        let mut state = BridgeState::new(&test_config(), seed);
        let ts = 1_000_000;

        state.handle_irc_event(&S2SEvent::LinkUp, ts);
        state.state_dirty = false;

        state.handle_discord_event(
            &DiscordEvent::MemberSnapshot {
                guild_id: 999,
                members: vec![MemberInfo {
                    user_id: 42,
                    username: "alice".into(),
                    display_name: "Alice".into(),
                    presence: DiscordPresence::Offline,
                }],
                channel_ids: vec![111],
                channel_names: std::collections::HashMap::new(),
                role_names: std::collections::HashMap::new(),
                bot_user_id: 0,
            },
            ts,
        );

        assert!(
            state.state_dirty,
            "apply_seeds introducing an offline user with seed should set dirty"
        );
    }

    // --- Dirty flag ---

    #[test]
    fn dirty_flag_set_on_discord_message_relay() {
        let mut state = BridgeState::new(&test_config(), HashMap::new());
        let ts = 1_000_000;

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
                bot_user_id: 0,
            },
            ts,
        );
        state.handle_irc_event(&S2SEvent::LinkUp, ts);

        // Clear dirty from the introduction above.
        state.state_dirty = false;

        // Send a message — should set dirty (record_activity updates PM).
        state.handle_discord_event(
            &DiscordEvent::MessageReceived {
                channel_id: 111,
                author_id: 42,
                author_name: "alice".into(),
                author_display_name: "Alice".into(),
                content: "hello".into(),
                attachments: vec![],
                timestamp: chrono::DateTime::<chrono::Utc>::from_timestamp(1_700_000_000, 0)
                    .unwrap(),
            },
            ts + 10,
        );
        assert!(state.state_dirty, "message relay should set dirty flag");
    }

    #[test]
    fn dirty_flag_not_set_when_no_state_changes() {
        let mut state = BridgeState::new(&test_config(), HashMap::new());
        let ts = 1_000_000;

        state.handle_irc_event(&S2SEvent::LinkUp, ts);
        state.state_dirty = false;

        // An IRC message from an unknown UID — no pseudoclient state change.
        state.handle_irc_event(
            &S2SEvent::MessageReceived {
                from_uid: "002AAAAAA".into(),
                target: "#unknown".into(),
                text: "hello".into(),
                timestamp: None,
            },
            ts + 10,
        );
        assert!(
            !state.state_dirty,
            "IRC message to unknown channel should not set dirty"
        );
    }

    #[test]
    fn dirty_flag_set_on_idle_timeout_part() {
        let mut config = test_config();
        config.pseudoclients.channel_idle_timeout_secs = 100;
        let mut state = BridgeState::new(&config, HashMap::new());
        let ts = 1_000_000;

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
                bot_user_id: 0,
            },
            ts,
        );
        state.handle_irc_event(&S2SEvent::LinkUp, ts);

        // Give alice a channel to be parted from.
        state.pm.ensure_in_channel(42, "#test", ts);
        state.state_dirty = false;

        // Timeout fires.
        state.check_idle_timeouts(ts + 200);
        assert!(state.state_dirty, "idle timeout PART should set dirty flag");
    }

    #[test]
    fn dirty_flag_set_on_apply_discord_event_producing_commands() {
        let mut state = BridgeState::new(&test_config(), HashMap::new());
        let ts = 1_000_000;

        state.state_dirty = false;

        // MemberSnapshot introduces a user — produces IRC commands.
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
                bot_user_id: 0,
            },
            ts,
        );
        assert!(
            state.state_dirty,
            "MemberSnapshot introducing users should set dirty"
        );
    }

    // --- DM bridging disabled ---

    #[test]
    fn irc_privmsg_to_bot_dropped_when_dm_bridging_disabled() {
        let mut config = test_config();
        config.pseudoclients.dm_bridging = false;
        let mut state = BridgeState::new(&config, HashMap::new());
        let ts = 1_000_000;

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
                bot_user_id: 42,
            },
            ts,
        );
        state.handle_irc_event(&S2SEvent::LinkUp, ts);
        state.handle_irc_event(&S2SEvent::BurstComplete, ts);

        // Register an external IRC user so the from_uid resolves.
        state.handle_irc_event(
            &S2SEvent::UserIntroduced {
                uid: "002AAAAAA".into(),
                nick: "external".into(),
                ident: "~u".into(),
                host: "host".into(),
                server_sid: "002".into(),
                realname: "External".into(),
            },
            ts,
        );

        let bot_uid = state
            .pm
            .get_by_discord_id(42)
            .expect("bot should be introduced")
            .uid
            .clone();

        // IRC user PRIVMSGs the bot pseudoclient directly.
        let out = state.handle_irc_event(
            &S2SEvent::MessageReceived {
                from_uid: "002AAAAAA".into(),
                target: bot_uid,
                text: "hi bot".into(),
                timestamp: None,
            },
            ts + 10,
        );

        assert!(
            out.discord_commands.is_empty(),
            "DM to bot should be dropped when dm_bridging=false; got: {:?}",
            out.discord_commands
        );
    }

    #[test]
    fn discord_dm_dropped_when_dm_bridging_disabled() {
        let mut config = test_config();
        config.pseudoclients.dm_bridging = false;
        let mut state = BridgeState::new(&config, HashMap::new());
        let ts = 1_000_000;

        state.handle_irc_event(&S2SEvent::LinkUp, ts);

        let out = state.handle_discord_event(
            &DiscordEvent::DmReceived {
                author_id: 42,
                author_name: "alice".into(),
                content: "hello bridge".into(),
                referenced_content: None,
                timestamp: chrono::DateTime::<chrono::Utc>::from_timestamp(1_700_000_000, 0)
                    .unwrap(),
            },
            ts,
        );

        assert!(
            out.irc_commands.is_empty(),
            "Discord DM should be dropped when dm_bridging=false; got: {:?}",
            out.irc_commands
        );
        assert!(
            out.discord_commands.is_empty(),
            "no error reply when dm_bridging=false; got: {:?}",
            out.discord_commands
        );
    }

    // --- Timestamp propagation ---

    #[test]
    fn discord_message_timestamp_propagates_to_irc_send() {
        let mut state = BridgeState::new(&test_config(), HashMap::new());
        let ts = 1_000_000;
        let msg_ts = chrono::DateTime::<chrono::Utc>::from_timestamp(1_700_000_123, 0).unwrap();

        state.handle_irc_event(&S2SEvent::LinkUp, ts);
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
                bot_user_id: 0,
            },
            ts,
        );
        state.handle_irc_event(&S2SEvent::BurstComplete, ts);

        let out = state.handle_discord_event(
            &DiscordEvent::MessageReceived {
                channel_id: 111,
                author_id: 42,
                author_name: "alice".into(),
                author_display_name: "Alice".into(),
                content: "hello".into(),
                attachments: vec![],
                timestamp: msg_ts,
            },
            ts + 10,
        );

        let send = out
            .irc_commands
            .iter()
            .find_map(|c| match c {
                S2SCommand::SendMessage { timestamp, .. } => Some(*timestamp),
                _ => None,
            })
            .expect("expected a SendMessage");
        assert_eq!(
            send,
            Some(msg_ts),
            "Discord msg.timestamp must be forwarded to S2SCommand::SendMessage"
        );
    }

    #[test]
    fn discord_dm_timestamp_propagates_to_irc_send() {
        let mut config = test_config();
        config.pseudoclients.dm_bridging = true;
        let mut state = BridgeState::new(&config, HashMap::new());
        let ts = 1_000_000;
        let msg_ts = chrono::DateTime::<chrono::Utc>::from_timestamp(1_700_000_456, 0).unwrap();

        // Introduce alice + bob so the DM has a valid target (alice DMs bob).
        state.handle_discord_event(
            &DiscordEvent::MemberSnapshot {
                guild_id: 999,
                members: vec![
                    MemberInfo {
                        user_id: 42,
                        username: "alice".into(),
                        display_name: "Alice".into(),
                        presence: DiscordPresence::Online,
                    },
                    MemberInfo {
                        user_id: 43,
                        username: "bob".into(),
                        display_name: "Bob".into(),
                        presence: DiscordPresence::Online,
                    },
                ],
                channel_ids: vec![111],
                channel_names: std::collections::HashMap::new(),
                role_names: std::collections::HashMap::new(),
                bot_user_id: 0,
            },
            ts,
        );
        state.handle_irc_event(&S2SEvent::LinkUp, ts);
        state.handle_irc_event(&S2SEvent::BurstComplete, ts);

        // DM routing resolves the target by parsing the `**[nick]**` prefix
        // of the referenced (quoted) message — see spec 09.
        let out = state.handle_discord_event(
            &DiscordEvent::DmReceived {
                author_id: 42,
                author_name: "alice".into(),
                content: "hi there".into(),
                referenced_content: Some("**[bob]** earlier message".into()),
                timestamp: msg_ts,
            },
            ts + 5,
        );

        let send = out
            .irc_commands
            .iter()
            .find_map(|c| match c {
                S2SCommand::SendMessage { timestamp, .. } => Some(*timestamp),
                _ => None,
            })
            .expect("expected a SendMessage from DM relay");
        assert_eq!(
            send,
            Some(msg_ts),
            "DM timestamp must be forwarded to S2SCommand::SendMessage"
        );
    }
}
