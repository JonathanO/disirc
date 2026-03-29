use chrono::{DateTime, Utc};
use tokio::sync::mpsc;

use crate::config::{BridgeEntry, Config};
use crate::discord::{DiscordCommand, DiscordEvent, DiscordPresence};
use crate::formatting::{DiscordResolver, IrcMentionResolver};
use crate::irc::{S2SCommand, S2SEvent};
use crate::pseudoclients::{PseudoclientManager, sanitize_nick};
use crate::signal::ControlEvent;

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
            // Reset IRC-side state: external nick map and channel timestamps are
            // no longer valid after a link loss.  PseudoclientManager is NOT
            // reset here — its state survives so the burst on the next LinkUp
            // can re-introduce all known Discord pseudoclients without waiting
            // for a fresh MemberSnapshot.
            state.reset();
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
// Discord lifecycle state
// ---------------------------------------------------------------------------

/// Mutable Discord-side state maintained by the bridge processing task.
///
/// - `display_names`: populated from `MemberSnapshot` and `MemberAdded` so the
///   bridge can look up a user's display name when a `PresenceUpdated` event
///   (which carries no display name) needs to introduce a pseudoclient.
/// - `guild_irc_channels`: populated by the bridge loop from `BridgeMap` and
///   the Discord module's guild↔channel associations.  Maps a Discord guild ID
///   to the IRC channel names the bridge serves for that guild.
#[derive(Debug, Default)]
pub struct DiscordState {
    /// Discord user ID → current display name.
    pub display_names: std::collections::HashMap<u64, String>,
    /// Discord guild ID → IRC channel names served by this guild.
    pub guild_irc_channels: std::collections::HashMap<u64, Vec<String>>,
}

/// Apply one `DiscordEvent` to the bridge's Discord-side state.
///
/// Returns the `S2SCommand`s that must be forwarded to the IRC connection
/// module.  Caller supplies `now_ts` (Unix seconds) for UID/SJOIN timestamps
/// when no stored channel timestamp is available.
pub fn apply_discord_event(
    discord_state: &mut DiscordState,
    pm: &mut PseudoclientManager,
    irc_state: &IrcState,
    event: &DiscordEvent,
    now_ts: u64,
) -> Vec<S2SCommand> {
    match event {
        DiscordEvent::MemberSnapshot {
            guild_id,
            members,
            channel_ids: _,
        } => {
            let channels = discord_state
                .guild_irc_channels
                .get(guild_id)
                .cloned()
                .unwrap_or_default();
            let mut cmds = Vec::new();
            for member in members {
                // Option B: only introduce non-offline members.
                if !member.presence.is_non_offline() {
                    continue;
                }
                discord_state
                    .display_names
                    .insert(member.user_id, member.display_name.clone());
                cmds.extend(introduce_pseudoclient(
                    pm,
                    irc_state,
                    member.user_id,
                    &member.display_name,
                    &channels,
                    member.presence,
                    now_ts,
                ));
            }
            cmds
        }

        DiscordEvent::MemberAdded {
            user_id,
            guild_id: _,
            display_name,
        } => {
            // Cache the display name; introduction is deferred to PresenceUpdated.
            discord_state
                .display_names
                .insert(*user_id, display_name.clone());
            vec![]
        }

        DiscordEvent::MemberRemoved {
            user_id,
            guild_id: _,
        } => {
            discord_state.display_names.remove(user_id);
            // Look up the UID before quitting (quit removes all state).
            let uid = pm.get_by_discord_id(*user_id).map(|s| s.uid.clone());
            if let Some(uid) = uid {
                pm.quit(*user_id, "Left Discord");
                return vec![S2SCommand::QuitUser {
                    uid,
                    reason: "Left Discord".to_string(),
                }];
            }
            vec![]
        }

        DiscordEvent::PresenceUpdated {
            user_id,
            guild_id,
            presence,
        } => {
            if !presence.is_non_offline() {
                return vec![];
            }
            let channels = discord_state
                .guild_irc_channels
                .get(guild_id)
                .cloned()
                .unwrap_or_default();
            let Some(display_name) = discord_state
                .display_names
                .get(user_id)
                .filter(|s| !s.is_empty())
                .cloned()
            else {
                // No cached display name — skip introduction. The user will
                // be introduced on-demand when they send a message (which
                // carries their username).
                return vec![];
            };
            introduce_pseudoclient(
                pm,
                irc_state,
                *user_id,
                &display_name,
                &channels,
                *presence,
                now_ts,
            )
        }

        // MessageReceived is handled by the message relay paths, not here.
        DiscordEvent::MessageReceived { .. } => vec![],
    }
}

/// Introduce a pseudoclient if not already present, then apply away state.
///
/// Returns the `S2SCommand`s needed to introduce the user and set their
/// initial presence.  Returns only away/back commands if already introduced.
fn introduce_pseudoclient(
    pm: &mut PseudoclientManager,
    irc_state: &IrcState,
    user_id: u64,
    display_name: &str,
    channels: &[String],
    presence: DiscordPresence,
    now_ts: u64,
) -> Vec<S2SCommand> {
    let mut cmds = Vec::new();

    if pm.get_by_discord_id(user_id).is_none() {
        // Not yet introduced — call pm.introduce() to allocate uid/nick.
        if pm
            .introduce(user_id, display_name, display_name, channels, now_ts)
            .is_some()
        {
            // Read back the allocated state.
            let (uid, nick, chans) = {
                let s = pm.get_by_discord_id(user_id).expect("just introduced");
                (s.uid.clone(), s.nick.clone(), s.channels.clone())
            };
            let host = format!("{}.{}", sanitize_nick(display_name), pm.host_suffix());
            cmds.push(S2SCommand::IntroduceUser {
                uid: uid.clone(),
                nick,
                ident: pm.ident().to_string(),
                host,
                realname: display_name.to_string(),
            });
            for channel in &chans {
                let ts = irc_state.ts_for_channel(channel).unwrap_or(now_ts);
                cmds.push(S2SCommand::JoinChannel {
                    uid: uid.clone(),
                    channel: channel.clone(),
                    ts,
                });
            }
        }
    }

    // Apply the presence as away / back.
    if let Some(s) = pm.get_by_discord_id(user_id) {
        let uid = s.uid.clone();
        match presence {
            DiscordPresence::Online => cmds.push(S2SCommand::ClearAway { uid }),
            DiscordPresence::Idle => cmds.push(S2SCommand::SetAway {
                uid,
                reason: "Idle".to_string(),
            }),
            DiscordPresence::DoNotDisturb => cmds.push(S2SCommand::SetAway {
                uid,
                reason: "Do Not Disturb".to_string(),
            }),
            DiscordPresence::Offline => {}
        }
    }

    cmds
}

// ---------------------------------------------------------------------------
// Bridge loop helpers
// ---------------------------------------------------------------------------

/// Null resolver: no IRC mention conversion.
struct NoopIrcResolver;
impl IrcMentionResolver for NoopIrcResolver {
    fn resolve_nick(&self, _: &str) -> Option<String> {
        None
    }
}

/// Null resolver: no Discord mention conversion.
struct NoopDiscordResolver;
impl DiscordResolver for NoopDiscordResolver {
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
    timestamp: Option<DateTime<Utc>>,
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
            let host = format!(
                "{}.{}",
                crate::pseudoclients::sanitize_nick(&display_name),
                pm.host_suffix()
            );
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
// Bridge loop
// ---------------------------------------------------------------------------

/// Current Unix timestamp in seconds.
fn unix_now() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Main bridge processing loop.
///
/// Owns `PseudoclientManager`, `IrcState`, and `DiscordState`.  Runs until
/// both event channels close (which happens when the connection tasks exit).
///
/// - `config_path` — path to the config file, used for hot-reload on
///   `ControlEvent::Reload`.
pub async fn run_bridge(
    config: &Config,
    config_path: &std::path::Path,
    mut irc_event_rx: mpsc::Receiver<S2SEvent>,
    irc_cmd_tx: mpsc::Sender<S2SCommand>,
    mut discord_event_rx: mpsc::Receiver<DiscordEvent>,
    discord_cmd_tx: mpsc::Sender<DiscordCommand>,
    mut control_rx: mpsc::Receiver<ControlEvent>,
) {
    let mut current_config = config.clone();
    let mut bridge_map = BridgeMap::from_config(&config.bridges);
    let mut pm = PseudoclientManager::new(
        &config.irc.sid,
        &config.pseudoclients.ident,
        &config.pseudoclients.host_suffix,
    );
    let mut irc_state = IrcState::default();
    let mut discord_state = DiscordState::default();

    loop {
        tokio::select! {
            maybe_event = irc_event_rx.recv() => {
                let Some(event) = maybe_event else { break };

                match &event {
                    S2SEvent::LinkUp => {
                        let now = unix_now();
                        for cmd in produce_burst_commands(&pm, &irc_state, now) {
                            let _ = irc_cmd_tx.send(cmd).await;
                        }
                    }
                    S2SEvent::MessageReceived { from_uid, target, text, timestamp } => {
                        if let Some(cmd) = route_irc_to_discord(
                            &pm, &bridge_map, &irc_state,
                            from_uid, target, text, false, &NoopIrcResolver,
                        ) {
                            let _ = discord_cmd_tx.send(cmd).await;
                        }
                        // TODO: thread `timestamp` (IRC server-time) through to
                        // the Discord send path for accurate message timing.
                        let _ = timestamp;
                    }
                    S2SEvent::NoticeReceived { from_uid, target, text } => {
                        if let Some(cmd) = route_irc_to_discord(
                            &pm, &bridge_map, &irc_state,
                            from_uid, target, text, true, &NoopIrcResolver,
                        ) {
                            let _ = discord_cmd_tx.send(cmd).await;
                        }
                    }
                    _ => {}
                }

                apply_irc_event(&mut irc_state, &mut pm, &event);
            }

            maybe_event = discord_event_rx.recv() => {
                let Some(event) = maybe_event else { break };

                // Populate guild→irc-channel map before apply_discord_event uses it.
                if let DiscordEvent::MemberSnapshot { guild_id, channel_ids, .. } = &event {
                    update_guild_irc_channels(&mut discord_state, &bridge_map, *guild_id, channel_ids);
                }

                // Route Discord messages to IRC before state update.
                if let DiscordEvent::MessageReceived {
                    channel_id,
                    author_id,
                    author_name,
                    content,
                    attachments,
                } = &event
                {
                    let now = unix_now();
                    let cmds = route_discord_to_irc(
                        &mut pm, &bridge_map, &discord_state, &irc_state,
                        *channel_id, *author_id, author_name, content, attachments,
                        None, now, &NoopDiscordResolver,
                    );
                    for cmd in cmds {
                        let _ = irc_cmd_tx.send(cmd).await;
                    }
                }

                let now = unix_now();
                let cmds = apply_discord_event(&mut discord_state, &mut pm, &irc_state, &event, now);
                for cmd in cmds {
                    let _ = irc_cmd_tx.send(cmd).await;
                }
            }

            maybe_ctrl = control_rx.recv() => {
                match maybe_ctrl {
                    Some(ControlEvent::Reload) => {
                        match crate::config::reload(config_path, &current_config) {
                            Ok((new_config, diff)) => {
                                if !diff.is_empty() {
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
                                            e.webhook_url.as_deref()
                                                .and_then(crate::discord::webhook_id_from_url)
                                        })
                                        .collect();
                                    let removed_webhook_ids: Vec<u64> = diff
                                        .removed
                                        .iter()
                                        .chain(diff.webhook_changed.iter())
                                        .filter_map(|e| {
                                            e.webhook_url.as_deref()
                                                .and_then(crate::discord::webhook_id_from_url)
                                        })
                                        .collect();
                                    let _ = discord_cmd_tx
                                        .send(DiscordCommand::ReloadBridges {
                                            added_channel_ids: added_ids,
                                            removed_channel_ids: removed_ids,
                                            added_webhook_ids,
                                            removed_webhook_ids,
                                        })
                                        .await;
                                    bridge_map = BridgeMap::from_config(&new_config.bridges);
                                }
                                current_config = new_config;
                                tracing::info!("Config reloaded");
                            }
                            Err(e) => {
                                tracing::warn!("Config reload failed: {e}");
                            }
                        }
                    }
                    None => break,
                }
            }
        }
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

    use crate::discord::{DiscordCommand, DiscordEvent, DiscordPresence, MemberInfo};
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
    fn link_down_preserves_pseudoclient_manager_for_reburst() {
        // PM state must survive LinkDown so the bridge can re-introduce all
        // pseudoclients immediately on the next LinkUp without waiting for a
        // fresh Discord MemberSnapshot.
        let mut state = IrcState::default();
        let mut pm = make_pm();
        pm.introduce(55, "user55", "User 55", &["#test".to_string()], 1000)
            .expect("introduce should succeed");
        apply_irc_event(
            &mut state,
            &mut pm,
            &S2SEvent::LinkDown {
                reason: "down".to_string(),
            },
        );
        assert!(
            pm.get_by_discord_id(55).is_some(),
            "PseudoclientManager must survive LinkDown for reburst"
        );
    }

    // --- apply_discord_event / DiscordState ---

    fn make_discord_state_with_channels(guild_id: u64, channels: &[&str]) -> DiscordState {
        let mut ds = DiscordState::default();
        ds.guild_irc_channels
            .insert(guild_id, channels.iter().map(|s| s.to_string()).collect());
        ds
    }

    fn member(user_id: u64, name: &str, presence: DiscordPresence) -> MemberInfo {
        MemberInfo {
            user_id,
            display_name: name.to_string(),
            presence,
        }
    }

    #[test]
    fn member_snapshot_introduces_non_offline_members() {
        let mut ds = make_discord_state_with_channels(1, &["#general"]);
        let mut pm = make_pm();
        let irc = IrcState::default();

        let cmds = apply_discord_event(
            &mut ds,
            &mut pm,
            &irc,
            &DiscordEvent::MemberSnapshot {
                guild_id: 1,
                channel_ids: vec![],
                members: vec![
                    member(10, "alice", DiscordPresence::Online),
                    member(20, "bob", DiscordPresence::Offline),
                ],
            },
            1000,
        );

        assert!(
            pm.get_by_discord_id(10).is_some(),
            "alice should be introduced"
        );
        assert!(
            pm.get_by_discord_id(20).is_none(),
            "bob is offline, not introduced"
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, S2SCommand::IntroduceUser { .. })),
            "should produce IntroduceUser"
        );
    }

    #[test]
    fn member_snapshot_online_member_gets_clear_away() {
        let mut ds = make_discord_state_with_channels(1, &["#general"]);
        let mut pm = make_pm();
        let irc = IrcState::default();

        let cmds = apply_discord_event(
            &mut ds,
            &mut pm,
            &irc,
            &DiscordEvent::MemberSnapshot {
                guild_id: 1,
                channel_ids: vec![],
                members: vec![member(10, "alice", DiscordPresence::Online)],
            },
            1000,
        );

        assert!(
            cmds.iter()
                .any(|c| matches!(c, S2SCommand::ClearAway { .. })),
            "online member should get ClearAway"
        );
    }

    #[test]
    fn member_snapshot_idle_member_gets_set_away() {
        let mut ds = make_discord_state_with_channels(1, &["#general"]);
        let mut pm = make_pm();
        let irc = IrcState::default();

        let cmds = apply_discord_event(
            &mut ds,
            &mut pm,
            &irc,
            &DiscordEvent::MemberSnapshot {
                guild_id: 1,
                channel_ids: vec![],
                members: vec![member(10, "alice", DiscordPresence::Idle)],
            },
            1000,
        );

        assert!(
            cmds.iter()
                .any(|c| matches!(c, S2SCommand::SetAway { reason, .. } if reason == "Idle")),
            "idle member should get SetAway Idle"
        );
    }

    #[test]
    fn member_snapshot_caches_display_names() {
        let mut ds = make_discord_state_with_channels(1, &["#general"]);
        let mut pm = make_pm();
        let irc = IrcState::default();

        apply_discord_event(
            &mut ds,
            &mut pm,
            &irc,
            &DiscordEvent::MemberSnapshot {
                guild_id: 1,
                channel_ids: vec![],
                members: vec![member(10, "alice", DiscordPresence::Online)],
            },
            1000,
        );

        assert_eq!(ds.display_names.get(&10).map(|s| s.as_str()), Some("alice"));
    }

    #[test]
    fn member_added_caches_display_name_without_introducing() {
        let mut ds = DiscordState::default();
        let mut pm = make_pm();
        let irc = IrcState::default();

        let cmds = apply_discord_event(
            &mut ds,
            &mut pm,
            &irc,
            &DiscordEvent::MemberAdded {
                user_id: 42,
                guild_id: 1,
                display_name: "charlie".to_string(),
            },
            1000,
        );

        assert!(cmds.is_empty(), "MemberAdded should not introduce");
        assert_eq!(
            ds.display_names.get(&42).map(|s| s.as_str()),
            Some("charlie")
        );
        assert!(pm.get_by_discord_id(42).is_none());
    }

    #[test]
    fn member_removed_quits_introduced_pseudoclient() {
        let mut ds = make_discord_state_with_channels(1, &["#general"]);
        let mut pm = make_pm();
        let irc = IrcState::default();

        // Introduce first
        apply_discord_event(
            &mut ds,
            &mut pm,
            &irc,
            &DiscordEvent::MemberSnapshot {
                guild_id: 1,
                channel_ids: vec![],
                members: vec![member(10, "alice", DiscordPresence::Online)],
            },
            1000,
        );
        assert!(pm.get_by_discord_id(10).is_some());

        // Now remove
        let cmds = apply_discord_event(
            &mut ds,
            &mut pm,
            &irc,
            &DiscordEvent::MemberRemoved {
                user_id: 10,
                guild_id: 1,
            },
            1000,
        );

        assert!(
            pm.get_by_discord_id(10).is_none(),
            "pseudoclient should be quit"
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, S2SCommand::QuitUser { .. })),
            "should produce QuitUser"
        );
    }

    #[test]
    fn member_removed_non_introduced_is_noop() {
        let mut ds = DiscordState::default();
        let mut pm = make_pm();
        let irc = IrcState::default();

        let cmds = apply_discord_event(
            &mut ds,
            &mut pm,
            &irc,
            &DiscordEvent::MemberRemoved {
                user_id: 99,
                guild_id: 1,
            },
            1000,
        );

        assert!(cmds.is_empty());
    }

    #[test]
    fn presence_updated_offline_is_silently_dropped() {
        let mut ds = make_discord_state_with_channels(1, &["#general"]);
        let mut pm = make_pm();
        let irc = IrcState::default();
        ds.display_names.insert(50, "dave".to_string());

        let cmds = apply_discord_event(
            &mut ds,
            &mut pm,
            &irc,
            &DiscordEvent::PresenceUpdated {
                user_id: 50,
                guild_id: 1,
                presence: DiscordPresence::Offline,
            },
            1000,
        );

        assert!(
            cmds.is_empty(),
            "offline presence should produce no commands"
        );
        assert!(
            pm.get_by_discord_id(50).is_none(),
            "should not be introduced"
        );
    }

    #[test]
    fn presence_updated_non_offline_introduces_unknown_user() {
        let mut ds = make_discord_state_with_channels(1, &["#general"]);
        let mut pm = make_pm();
        let irc = IrcState::default();
        ds.display_names.insert(50, "eve".to_string());

        let cmds = apply_discord_event(
            &mut ds,
            &mut pm,
            &irc,
            &DiscordEvent::PresenceUpdated {
                user_id: 50,
                guild_id: 1,
                presence: DiscordPresence::Online,
            },
            1000,
        );

        assert!(pm.get_by_discord_id(50).is_some(), "should be introduced");
        assert!(
            cmds.iter()
                .any(|c| matches!(c, S2SCommand::IntroduceUser { .. }))
        );
    }

    #[test]
    fn presence_updated_no_cached_display_name_skips_introduction() {
        let mut ds = make_discord_state_with_channels(1, &["#general"]);
        let mut pm = make_pm();
        let irc = IrcState::default();
        // No display name cached for user 50 — should not introduce.

        let cmds = apply_discord_event(
            &mut ds,
            &mut pm,
            &irc,
            &DiscordEvent::PresenceUpdated {
                user_id: 50,
                guild_id: 1,
                presence: DiscordPresence::Online,
            },
            1000,
        );

        assert!(
            pm.get_by_discord_id(50).is_none(),
            "must not introduce with no display name"
        );
        assert!(cmds.is_empty());
    }

    #[test]
    fn presence_updated_empty_display_name_skips_introduction() {
        let mut ds = make_discord_state_with_channels(1, &["#general"]);
        let mut pm = make_pm();
        let irc = IrcState::default();
        ds.display_names.insert(50, String::new()); // empty display name

        let cmds = apply_discord_event(
            &mut ds,
            &mut pm,
            &irc,
            &DiscordEvent::PresenceUpdated {
                user_id: 50,
                guild_id: 1,
                presence: DiscordPresence::Online,
            },
            1000,
        );

        assert!(
            pm.get_by_discord_id(50).is_none(),
            "must not introduce with empty display name"
        );
        assert!(cmds.is_empty());
    }

    #[test]
    fn presence_updated_already_introduced_only_updates_away() {
        let mut ds = make_discord_state_with_channels(1, &["#general"]);
        let mut pm = make_pm();
        let irc = IrcState::default();
        ds.display_names.insert(50, "eve".to_string());

        // First introduce
        apply_discord_event(
            &mut ds,
            &mut pm,
            &irc,
            &DiscordEvent::PresenceUpdated {
                user_id: 50,
                guild_id: 1,
                presence: DiscordPresence::Online,
            },
            1000,
        );

        // Now presence update with Idle — should NOT produce a second IntroduceUser
        let cmds = apply_discord_event(
            &mut ds,
            &mut pm,
            &irc,
            &DiscordEvent::PresenceUpdated {
                user_id: 50,
                guild_id: 1,
                presence: DiscordPresence::Idle,
            },
            1000,
        );

        assert!(
            !cmds
                .iter()
                .any(|c| matches!(c, S2SCommand::IntroduceUser { .. })),
            "should not re-introduce"
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, S2SCommand::SetAway { reason, .. } if reason == "Idle")),
            "should set away"
        );
    }

    #[test]
    fn member_snapshot_join_channel_uses_irc_state_ts() {
        let mut ds = make_discord_state_with_channels(1, &["#general"]);
        let mut pm = make_pm();
        let mut irc = IrcState::default();

        // Simulate a ChannelBurst so irc_state has a ts for #general
        apply_irc_event(
            &mut irc,
            &mut pm,
            &S2SEvent::ChannelBurst {
                channel: "#general".to_string(),
                ts: 5_000,
                members: vec![],
            },
        );

        let cmds = apply_discord_event(
            &mut ds,
            &mut pm,
            &irc,
            &DiscordEvent::MemberSnapshot {
                guild_id: 1,
                channel_ids: vec![],
                members: vec![member(10, "alice", DiscordPresence::Online)],
            },
            9_999,
        );

        let join_ts = cmds.iter().find_map(|c| {
            if let S2SCommand::JoinChannel { ts, .. } = c {
                Some(*ts)
            } else {
                None
            }
        });
        assert_eq!(join_ts, Some(5_000), "should use channel ts from IrcState");
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

    fn make_bridge_map() -> BridgeMap {
        use crate::config::BridgeEntry;
        BridgeMap::from_config(&[BridgeEntry {
            discord_channel_id: "111".to_string(),
            irc_channel: "#general".to_string(),
            webhook_url: None,
        }])
    }

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
