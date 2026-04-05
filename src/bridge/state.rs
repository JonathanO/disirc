use crate::discord::{DiscordEvent, DiscordPresence};
use crate::irc::{S2SCommand, S2SEvent};
use crate::pseudoclients::PseudoclientManager;

// ---------------------------------------------------------------------------
// IRC lifecycle state
// ---------------------------------------------------------------------------

/// Mutable IRC-side state maintained by the bridge processing task.
///
/// Tracks the uid→nick map for all external IRC users and the creation
/// timestamp of every channel we have seen in a `ChannelBurst`.  Both tables
/// are cleared on `LinkDown` / `PseudoclientManager::reset`.
///
/// Also tracks whether the S2S link is currently established so that the
/// bridge loop can suppress live-introduce commands when the link is down;
/// they would race with the burst that fires on `LinkUp`.
#[derive(Debug, Default)]
pub struct IrcState {
    /// `true` after `LinkUp`, `false` before first connect and after `LinkDown`.
    link_up: bool,
    /// uid → current nick for every non-pseudoclient IRC user.
    nicks: std::collections::HashMap<String, String>,
    /// channel name (lowercased) → creation timestamp.
    channel_ts: std::collections::HashMap<String, u64>,
}

impl IrcState {
    /// Returns `true` while the S2S link is established.
    #[must_use]
    pub fn is_link_up(&self) -> bool {
        self.link_up
    }

    /// Look up the current nick for a UID.
    #[must_use]
    pub fn nick_of(&self, uid: &str) -> Option<&str> {
        self.nicks.get(uid).map(String::as_str)
    }

    /// Look up the UID for a nick (case-insensitive).
    #[must_use]
    pub fn uid_of_nick(&self, nick: &str) -> Option<&str> {
        let lower = nick.to_ascii_lowercase();
        self.nicks
            .iter()
            .find(|(_, n)| n.to_ascii_lowercase() == lower)
            .map(|(uid, _)| uid.as_str())
    }

    /// Look up the stored creation timestamp for a channel.
    #[must_use]
    pub fn ts_for_channel(&self, channel: &str) -> Option<u64> {
        self.channel_ts.get(&channel.to_lowercase()).copied()
    }

    /// Reset all tracked state (call on link loss).
    pub fn reset(&mut self) {
        self.link_up = false;
        self.nicks.clear();
        self.channel_ts.clear();
    }
}

/// Apply one `S2SEvent` to the bridge's IRC-side state.
///
/// Updates `state` and `pm` in place; never fails.  Events that carry no
/// meaningful state update (e.g. `BurstComplete`, message events)
/// are accepted and silently ignored so the caller can forward every event
/// here without filtering.
pub fn apply_irc_event(state: &mut IrcState, pm: &mut PseudoclientManager, event: &S2SEvent) {
    match event {
        S2SEvent::LinkUp => {
            state.link_up = true;
        }

        S2SEvent::LinkDown { .. } => {
            // Reset IRC-side state: external nick map and channel timestamps are
            // no longer valid after a link loss.  PseudoclientManager is NOT
            // reset here — its state survives so the burst on the next LinkUp
            // can re-introduce all known Discord pseudoclients without waiting
            // for a fresh MemberSnapshot.
            state.reset(); // also sets link_up = false
        }

        S2SEvent::UserIntroduced { uid, nick, .. } => {
            state.nicks.insert(uid.clone(), nick.clone());
            // Track external nicks so pseudoclient introduction can avoid
            // nick collisions with real IRC users.
            if !pm.is_our_uid(uid) {
                pm.register_external_nick(nick);
            }
        }

        S2SEvent::UserNickChanged { uid, new_nick } => {
            if let Some(old_nick) = state.nicks.get(uid)
                && !pm.is_our_uid(uid)
            {
                pm.unregister_external_nick(old_nick);
                pm.register_external_nick(new_nick);
            }
            if let Some(entry) = state.nicks.get_mut(uid) {
                entry.clone_from(new_nick);
            }
        }

        S2SEvent::UserQuit { uid, .. } => {
            if let Some(nick) = state.nicks.remove(uid)
                && !pm.is_our_uid(uid)
            {
                pm.unregister_external_nick(&nick);
            }
        }

        S2SEvent::UserKilled { uid, reason, .. } => {
            if let Some(ps) = pm.get_by_uid(uid) {
                // One of our pseudoclients — remove from PM so it can be
                // re-introduced on-demand or immediately (if configured).
                tracing::debug!(
                    uid = %uid,
                    discord_id = ps.discord_user_id,
                    nick = %ps.nick,
                    reason = %reason,
                    "pseudoclient killed — removing from PM"
                );
                let discord_id = ps.discord_user_id;
                pm.quit(discord_id, "Killed");
                // Clear the cached UID so reintroduction allocates a fresh one,
                // avoiding UID collision with UnrealIRCd's kill state.
                pm.forget_uid(discord_id);
            } else {
                // External IRC user — remove from nick map.
                tracing::debug!(uid = %uid, reason = %reason, "external user killed");
                if let Some(nick) = state.nicks.remove(uid) {
                    pm.unregister_external_nick(&nick);
                }
            }
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
        S2SEvent::BurstComplete
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
    /// Discord channel ID → channel name (for mention resolution).
    pub channel_names: std::collections::HashMap<u64, String>,
    /// Discord role ID → role name (for mention resolution).
    pub role_names: std::collections::HashMap<u64, String>,
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
            channel_names,
            role_names,
        } => {
            // Store channel/role names for mention resolution.
            discord_state
                .channel_names
                .extend(channel_names.iter().map(|(&k, v)| (k, v.clone())));
            discord_state
                .role_names
                .extend(role_names.iter().map(|(&k, v)| (k, v.clone())));

            let channels = discord_state
                .guild_irc_channels
                .get(guild_id)
                .cloned()
                .unwrap_or_default();
            let mut cmds = Vec::new();
            for member in members {
                // Cache display name for all members so PresenceUpdated can
                // introduce them later when they come online, and for mention
                // resolution.
                discord_state
                    .display_names
                    .insert(member.user_id, member.display_name.clone());
                if !member.presence.is_non_offline() {
                    continue;
                }
                cmds.extend(introduce_pseudoclient(
                    pm,
                    irc_state,
                    member.user_id,
                    &member.username,
                    &member.display_name,
                    &channels,
                    member.presence,
                    now_ts,
                ));
            }
            tracing::debug!(guild_id, total = members.len(), "MemberSnapshot processed");
            cmds
        }

        DiscordEvent::MemberAdded {
            user_id,
            guild_id: _,
            display_name,
        } => {
            tracing::debug!(user_id, %display_name, "MemberAdded — cached display name");
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
            if let Some(state) = pm.quit(*user_id, "Left Discord") {
                tracing::debug!(user_id, uid = %state.uid, "MemberRemoved — quitting pseudoclient");
                return vec![S2SCommand::QuitUser {
                    uid: state.uid,
                    reason: "Left Discord".to_string(),
                }];
            }
            tracing::debug!(user_id, "MemberRemoved — no pseudoclient to quit");
            vec![]
        }

        // DMs are handled directly in the bridge loop; no state update needed.
        // (Separate arm to document intent; identical body to MemberRemoved fallthrough.)
        #[allow(clippy::match_same_arms)]
        DiscordEvent::DmReceived { .. } => vec![],

        DiscordEvent::PresenceUpdated {
            user_id,
            guild_id,
            presence,
            username,
            display_name,
        } => {
            // Cache/update display name if the presence payload carried it
            // (for mention resolution).
            if let Some(name) = display_name.as_ref().filter(|n| !n.is_empty()) {
                discord_state.display_names.insert(*user_id, name.clone());
            }

            // If the user is already introduced, update their away status
            // even if they went offline (AWAY :Offline rather than QUIT).
            if let Some(s) = pm.get_by_discord_id(*user_id) {
                let uid = s.uid.clone();
                let nick = s.nick.clone();
                // Keep stored presence current for burst re-introduction.
                pm.update_presence(*user_id, *presence);
                tracing::debug!(
                    user_id,
                    %nick,
                    %uid,
                    ?presence,
                    "PresenceUpdated — updating away status"
                );
                return match presence {
                    DiscordPresence::Online => vec![S2SCommand::ClearAway { uid }],
                    DiscordPresence::Idle => vec![S2SCommand::SetAway {
                        uid,
                        reason: "Idle".to_string(),
                    }],
                    DiscordPresence::DoNotDisturb => vec![S2SCommand::SetAway {
                        uid,
                        reason: "Do Not Disturb".to_string(),
                    }],
                    DiscordPresence::Offline => vec![S2SCommand::SetAway {
                        uid,
                        reason: "Offline".to_string(),
                    }],
                };
            }
            // Not yet introduced — only introduce for non-offline presence.
            if !presence.is_non_offline() {
                tracing::debug!(
                    user_id,
                    ?presence,
                    "PresenceUpdated — offline, not yet introduced, skipping"
                );
                return vec![];
            }
            let channels = discord_state
                .guild_irc_channels
                .get(guild_id)
                .cloned()
                .unwrap_or_default();
            // Resolve username from event data; fall back is not possible
            // without the usernames cache — skip introduction if absent.
            let Some(username) = username.as_ref().filter(|s| !s.is_empty()) else {
                tracing::debug!(
                    user_id,
                    ?presence,
                    "PresenceUpdated — no username available, skipping introduction"
                );
                return vec![];
            };
            let display_name = display_name
                .as_ref()
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    discord_state
                        .display_names
                        .get(user_id)
                        .filter(|s| !s.is_empty())
                })
                .cloned()
                .unwrap_or_else(|| username.clone());
            tracing::debug!(
                user_id,
                %username,
                %display_name,
                ?presence,
                "PresenceUpdated — introducing pseudoclient"
            );
            introduce_pseudoclient(
                pm,
                irc_state,
                *user_id,
                username,
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
#[allow(clippy::too_many_arguments)]
pub(crate) fn introduce_pseudoclient(
    pm: &mut PseudoclientManager,
    irc_state: &IrcState,
    user_id: u64,
    username: &str,
    display_name: &str,
    channels: &[String],
    presence: DiscordPresence,
    now_ts: u64,
) -> Vec<S2SCommand> {
    let mut cmds = Vec::new();

    if let Some(s) = pm.introduce(user_id, username, display_name, channels, now_ts, presence) {
        let uid = s.uid.clone();
        let nick = s.nick.clone();
        let chans = s.channels.clone();
        let host = format!("{user_id}.discord.com");
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
        // Set initial away if introduced as Idle/DnD (e.g. from burst).
        // Online needs no ClearAway — new users default to not-away.
        match presence {
            DiscordPresence::Idle => cmds.push(S2SCommand::SetAway {
                uid,
                reason: "Idle".to_string(),
            }),
            DiscordPresence::DoNotDisturb => cmds.push(S2SCommand::SetAway {
                uid,
                reason: "Do Not Disturb".to_string(),
            }),
            _ => {}
        }
    }

    cmds
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use crate::discord::{DiscordEvent, DiscordPresence, MemberInfo};
    use crate::irc::{S2SCommand, S2SEvent};
    use crate::pseudoclients::PseudoclientManager;

    fn make_pm() -> PseudoclientManager {
        PseudoclientManager::new("001", "bridge")
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

    fn make_discord_state_with_channels(guild_id: u64, channels: &[&str]) -> DiscordState {
        let mut ds = DiscordState::default();
        ds.guild_irc_channels
            .insert(guild_id, channels.iter().map(|s| s.to_string()).collect());
        ds
    }

    fn member(user_id: u64, name: &str, presence: DiscordPresence) -> MemberInfo {
        MemberInfo {
            user_id,
            username: name.to_string(),
            display_name: name.to_string(),
            presence,
        }
    }

    // --- IrcState / apply_irc_event ---

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
    fn link_up_sets_link_up_flag() {
        let mut state = IrcState::default();
        let mut pm = make_pm();
        assert!(!state.is_link_up(), "starts false");
        apply_irc_event(&mut state, &mut pm, &S2SEvent::LinkUp);
        assert!(state.is_link_up(), "true after LinkUp");
    }

    #[test]
    fn link_down_clears_link_up_flag() {
        let mut state = IrcState::default();
        let mut pm = make_pm();
        apply_irc_event(&mut state, &mut pm, &S2SEvent::LinkUp);
        apply_irc_event(
            &mut state,
            &mut pm,
            &S2SEvent::LinkDown {
                reason: "gone".to_string(),
            },
        );
        assert!(!state.is_link_up(), "false after LinkDown");
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
            DiscordPresence::Online,
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
        pm.introduce(
            77,
            "testuser",
            "Test User",
            &["#lobby".to_string()],
            1000,
            DiscordPresence::Online,
        )
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
            DiscordPresence::Online,
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
    fn kill_removes_pseudoclient_from_pm() {
        let mut state = IrcState::default();
        let mut pm = make_pm();
        pm.introduce(
            77,
            "alice",
            "Alice",
            &["#general".to_string()],
            1000,
            DiscordPresence::Online,
        )
        .expect("introduce should succeed");
        let uid = pm.get_by_discord_id(77).expect("should exist").uid.clone();

        apply_irc_event(
            &mut state,
            &mut pm,
            &S2SEvent::UserKilled {
                uid: uid.clone(),
                reason: "Killed by oper".to_string(),
            },
        );

        assert!(
            pm.get_by_discord_id(77).is_none(),
            "pseudoclient should be removed from PM after KILL"
        );
        assert!(
            pm.get_by_uid(&uid).is_none(),
            "pseudoclient UID should be removed from PM after KILL"
        );
    }

    #[test]
    fn kill_clears_uid_cache_so_reintroduction_gets_fresh_uid() {
        let mut state = IrcState::default();
        let mut pm = make_pm();
        pm.introduce(
            77,
            "alice",
            "Alice",
            &["#general".to_string()],
            1000,
            DiscordPresence::Online,
        )
        .expect("introduce should succeed");
        let old_uid = pm.get_by_discord_id(77).unwrap().uid.clone();

        // Kill the pseudoclient.
        apply_irc_event(
            &mut state,
            &mut pm,
            &S2SEvent::UserKilled {
                uid: old_uid.clone(),
                reason: "Killed".to_string(),
            },
        );
        assert!(pm.get_by_discord_id(77).is_none());

        // Re-introduce — should get a different UID.
        pm.introduce(
            77,
            "alice",
            "Alice",
            &["#general".to_string()],
            2000,
            DiscordPresence::Online,
        )
        .expect("reintroduce should succeed");
        let new_uid = pm.get_by_discord_id(77).unwrap().uid.clone();

        assert_ne!(
            old_uid, new_uid,
            "reintroduced pseudoclient must get a fresh UID to avoid collision; old={old_uid}, new={new_uid}"
        );
    }

    #[test]
    fn kill_of_external_user_only_removes_nick() {
        let mut state = IrcState::default();
        let mut pm = make_pm();
        apply_irc_event(&mut state, &mut pm, &introduced("001EXTUSER", "bob"));
        assert_eq!(state.nick_of("001EXTUSER"), Some("bob"));

        apply_irc_event(
            &mut state,
            &mut pm,
            &S2SEvent::UserKilled {
                uid: "001EXTUSER".to_string(),
                reason: "Killed".to_string(),
            },
        );

        assert_eq!(state.nick_of("001EXTUSER"), None, "nick should be removed");
    }

    #[test]
    fn introduced_external_user_registers_nick_for_collision_avoidance() {
        let mut state = IrcState::default();
        let mut pm = make_pm();
        apply_irc_event(&mut state, &mut pm, &introduced("001EXTUSER", "alice"));

        // Now introduce a pseudoclient with the same name — should get a suffixed nick.
        pm.introduce(
            42,
            "alice",
            "Alice",
            &["#general".to_string()],
            1000,
            DiscordPresence::Online,
        );
        let ps = pm.get_by_discord_id(42).unwrap();
        assert_ne!(
            ps.nick, "alice",
            "pseudoclient nick should differ from external user; got: {}",
            ps.nick
        );
    }

    #[test]
    fn quit_external_user_unregisters_nick() {
        let mut state = IrcState::default();
        let mut pm = make_pm();
        apply_irc_event(&mut state, &mut pm, &introduced("001EXTUSER", "alice"));

        // Quit the external user.
        apply_irc_event(
            &mut state,
            &mut pm,
            &S2SEvent::UserQuit {
                uid: "001EXTUSER".to_string(),
                reason: "Quit".to_string(),
            },
        );

        // Now a pseudoclient can use "alice" without collision.
        pm.introduce(
            42,
            "alice",
            "Alice",
            &["#general".to_string()],
            1000,
            DiscordPresence::Online,
        );
        let ps = pm.get_by_discord_id(42).unwrap();
        assert_eq!(
            ps.nick, "alice",
            "after external user quit, pseudoclient should get the exact nick"
        );
    }

    #[test]
    fn nick_change_updates_external_nick_registration() {
        let mut state = IrcState::default();
        let mut pm = make_pm();
        apply_irc_event(&mut state, &mut pm, &introduced("001EXTUSER", "alice"));

        // External user changes nick to "bob".
        apply_irc_event(
            &mut state,
            &mut pm,
            &S2SEvent::UserNickChanged {
                uid: "001EXTUSER".to_string(),
                new_nick: "bob".to_string(),
            },
        );

        // "alice" is now free — pseudoclient can use it.
        pm.introduce(
            42,
            "alice",
            "Alice",
            &["#general".to_string()],
            1000,
            DiscordPresence::Online,
        );
        assert_eq!(pm.get_by_discord_id(42).unwrap().nick, "alice");

        // "bob" is taken — pseudoclient should get a suffix.
        pm.introduce(
            43,
            "bob",
            "Bob",
            &["#general".to_string()],
            1000,
            DiscordPresence::Online,
        );
        assert_ne!(pm.get_by_discord_id(43).unwrap().nick, "bob");
    }

    #[test]
    fn link_down_preserves_pseudoclient_manager_for_reburst() {
        // PM state must survive LinkDown so the bridge can re-introduce all
        // pseudoclients immediately on the next LinkUp without waiting for a
        // fresh Discord MemberSnapshot.
        let mut state = IrcState::default();
        let mut pm = make_pm();
        pm.introduce(
            55,
            "user55",
            "User 55",
            &["#test".to_string()],
            1000,
            DiscordPresence::Online,
        )
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
                channel_names: std::collections::HashMap::new(),
                role_names: std::collections::HashMap::new(),
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
    fn offline_member_introduced_when_coming_online() {
        let mut ds = make_discord_state_with_channels(1, &["#general"]);
        let mut pm = make_pm();
        let irc = IrcState::default();

        // Snapshot includes an offline member.
        apply_discord_event(
            &mut ds,
            &mut pm,
            &irc,
            &DiscordEvent::MemberSnapshot {
                guild_id: 1,
                channel_ids: vec![],
                channel_names: std::collections::HashMap::new(),
                role_names: std::collections::HashMap::new(),
                members: vec![member(20, "bob", DiscordPresence::Offline)],
            },
            1000,
        );
        assert!(pm.get_by_discord_id(20).is_none(), "bob is offline");

        // Bob comes online — PresenceUpdated carries the username; display name
        // falls back to the cached value from the snapshot.
        let cmds = apply_discord_event(
            &mut ds,
            &mut pm,
            &irc,
            &DiscordEvent::PresenceUpdated {
                user_id: 20,
                guild_id: 1,
                presence: DiscordPresence::Online,
                username: Some("bob".into()),
                display_name: None,
            },
            1001,
        );
        assert!(
            pm.get_by_discord_id(20).is_some(),
            "bob should be introduced after coming online"
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, S2SCommand::IntroduceUser { .. })),
            "should produce IntroduceUser for bob"
        );
    }

    #[test]
    fn presence_updated_uses_event_carried_username() {
        let mut ds = make_discord_state_with_channels(1, &["#general"]);
        let mut pm = make_pm();
        let irc = IrcState::default();
        // No cached names — the event carries them.

        let cmds = apply_discord_event(
            &mut ds,
            &mut pm,
            &irc,
            &DiscordEvent::PresenceUpdated {
                user_id: 60,
                guild_id: 1,
                presence: DiscordPresence::Online,
                username: Some("frank_user".into()),
                display_name: Some("Frank Display".into()),
            },
            1000,
        );

        assert!(
            pm.get_by_discord_id(60).is_some(),
            "should be introduced from event-carried name"
        );
        // Nick should be derived from the username, not the display name.
        let nick = &pm.get_by_discord_id(60).unwrap().nick;
        assert_eq!(nick, "frank_user");
        // Display name should be stored.
        let gecos = &pm.get_by_discord_id(60).unwrap().display_name;
        assert_eq!(gecos, "Frank Display");
        // Display name should be cached for mention resolution.
        assert_eq!(
            ds.display_names.get(&60).map(String::as_str),
            Some("Frank Display")
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, S2SCommand::IntroduceUser { .. })),
        );
    }

    #[test]
    fn presence_updated_empty_username_skips_introduction() {
        let mut ds = make_discord_state_with_channels(1, &["#general"]);
        let mut pm = make_pm();
        let irc = IrcState::default();
        // Pre-cache display name (for mention resolution).
        ds.display_names.insert(70, "Cached Display".to_string());

        // Event carries empty username — cannot introduce without a username.
        let cmds = apply_discord_event(
            &mut ds,
            &mut pm,
            &irc,
            &DiscordEvent::PresenceUpdated {
                user_id: 70,
                guild_id: 1,
                presence: DiscordPresence::Online,
                username: Some(String::new()),
                display_name: Some(String::new()),
            },
            1000,
        );

        assert!(
            pm.get_by_discord_id(70).is_none(),
            "empty username should skip introduction"
        );
        assert!(
            cmds.is_empty(),
            "no commands should be emitted without a username"
        );
    }

    #[test]
    fn presence_updated_display_name_falls_back_to_cache() {
        let mut ds = make_discord_state_with_channels(1, &["#general"]);
        let mut pm = make_pm();
        let irc = IrcState::default();
        // Pre-cache display name.
        ds.display_names.insert(80, "Cached Display".to_string());

        // Event carries a username but empty display name — should fall back
        // to cached display name.
        let cmds = apply_discord_event(
            &mut ds,
            &mut pm,
            &irc,
            &DiscordEvent::PresenceUpdated {
                user_id: 80,
                guild_id: 1,
                presence: DiscordPresence::Online,
                username: Some("theuser".into()),
                display_name: Some(String::new()),
            },
            1000,
        );

        assert!(
            pm.get_by_discord_id(80).is_some(),
            "should be introduced with cached display name"
        );
        assert_eq!(pm.get_by_discord_id(80).unwrap().nick, "theuser");
        assert_eq!(
            pm.get_by_discord_id(80).unwrap().display_name,
            "Cached Display"
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, S2SCommand::IntroduceUser { .. })),
        );
    }

    #[test]
    fn member_snapshot_online_member_no_spurious_clear_away() {
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
                channel_names: std::collections::HashMap::new(),
                role_names: std::collections::HashMap::new(),
                members: vec![member(10, "alice", DiscordPresence::Online)],
            },
            1000,
        );

        // First introduction should NOT send ClearAway — new users default
        // to not-away on IRC.
        assert!(
            !cmds
                .iter()
                .any(|c| matches!(c, S2SCommand::ClearAway { .. })),
            "first introduction should not send ClearAway"
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
                channel_names: std::collections::HashMap::new(),
                role_names: std::collections::HashMap::new(),
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
    fn member_snapshot_dnd_member_gets_set_away() {
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
                channel_names: std::collections::HashMap::new(),
                role_names: std::collections::HashMap::new(),
                members: vec![member(10, "alice", DiscordPresence::DoNotDisturb)],
            },
            1000,
        );

        assert!(
            cmds.iter().any(
                |c| matches!(c, S2SCommand::SetAway { reason, .. } if reason == "Do Not Disturb")
            ),
            "DnD member should get SetAway"
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
                channel_names: std::collections::HashMap::new(),
                role_names: std::collections::HashMap::new(),
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
                channel_names: std::collections::HashMap::new(),
                role_names: std::collections::HashMap::new(),
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

        let cmds = apply_discord_event(
            &mut ds,
            &mut pm,
            &irc,
            &DiscordEvent::PresenceUpdated {
                user_id: 50,
                guild_id: 1,
                presence: DiscordPresence::Offline,
                username: Some("eve".into()),
                display_name: Some("eve".into()),
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

        let cmds = apply_discord_event(
            &mut ds,
            &mut pm,
            &irc,
            &DiscordEvent::PresenceUpdated {
                user_id: 50,
                guild_id: 1,
                presence: DiscordPresence::Online,
                username: Some("eve".into()),
                display_name: Some("eve".into()),
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
                username: None,
                display_name: None,
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
    fn presence_updated_no_cached_username_skips_introduction() {
        let mut ds = make_discord_state_with_channels(1, &["#general"]);
        let mut pm = make_pm();
        let irc = IrcState::default();
        // Display name cached but no username — cannot introduce.
        ds.display_names.insert(50, "Eve".to_string());

        let cmds = apply_discord_event(
            &mut ds,
            &mut pm,
            &irc,
            &DiscordEvent::PresenceUpdated {
                user_id: 50,
                guild_id: 1,
                presence: DiscordPresence::Online,
                username: None,
                display_name: None,
            },
            1000,
        );

        assert!(
            pm.get_by_discord_id(50).is_none(),
            "must not introduce without a username"
        );
        assert!(cmds.is_empty());
    }

    #[test]
    fn presence_updated_already_introduced_only_updates_away() {
        let mut ds = make_discord_state_with_channels(1, &["#general"]);
        let mut pm = make_pm();
        let irc = IrcState::default();

        // First introduce via event-carried names.
        apply_discord_event(
            &mut ds,
            &mut pm,
            &irc,
            &DiscordEvent::PresenceUpdated {
                user_id: 50,
                guild_id: 1,
                presence: DiscordPresence::Online,
                username: Some("eve".into()),
                display_name: Some("eve".into()),
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
                username: None,
                display_name: None,
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
    fn presence_updated_offline_sets_away_on_introduced_user() {
        let mut ds = make_discord_state_with_channels(1, &["#general"]);
        let mut pm = make_pm();
        let irc = IrcState::default();

        // Introduce via Online presence with event-carried names.
        apply_discord_event(
            &mut ds,
            &mut pm,
            &irc,
            &DiscordEvent::PresenceUpdated {
                user_id: 50,
                guild_id: 1,
                presence: DiscordPresence::Online,
                username: Some("eve".into()),
                display_name: Some("eve".into()),
            },
            1000,
        );
        assert!(pm.get_by_discord_id(50).is_some());

        // User goes offline — should set away, not quit.
        let cmds = apply_discord_event(
            &mut ds,
            &mut pm,
            &irc,
            &DiscordEvent::PresenceUpdated {
                user_id: 50,
                guild_id: 1,
                presence: DiscordPresence::Offline,
                username: None,
                display_name: None,
            },
            1001,
        );

        assert!(
            cmds.iter()
                .any(|c| matches!(c, S2SCommand::SetAway { reason, .. } if reason == "Offline")),
            "offline should set AWAY, not quit"
        );
        assert!(
            pm.get_by_discord_id(50).is_some(),
            "pseudoclient must persist (not quit) when user goes offline"
        );
    }

    #[test]
    fn presence_updated_online_clears_away_on_introduced_user() {
        let mut ds = make_discord_state_with_channels(1, &["#general"]);
        let mut pm = make_pm();
        let irc = IrcState::default();

        // Introduce, then set idle (event-carried names for first introduction).
        apply_discord_event(
            &mut ds,
            &mut pm,
            &irc,
            &DiscordEvent::PresenceUpdated {
                user_id: 50,
                guild_id: 1,
                presence: DiscordPresence::Idle,
                username: Some("eve".into()),
                display_name: Some("eve".into()),
            },
            1000,
        );

        // Come back online — should clear away.
        let cmds = apply_discord_event(
            &mut ds,
            &mut pm,
            &irc,
            &DiscordEvent::PresenceUpdated {
                user_id: 50,
                guild_id: 1,
                presence: DiscordPresence::Online,
                username: None,
                display_name: None,
            },
            1001,
        );

        assert!(
            cmds.iter()
                .any(|c| matches!(c, S2SCommand::ClearAway { .. })),
            "returning online should clear away"
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
                channel_names: std::collections::HashMap::new(),
                role_names: std::collections::HashMap::new(),
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
}
