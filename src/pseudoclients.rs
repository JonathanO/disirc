use std::collections::{HashMap, HashSet};

use crate::discord::DiscordPresence;
use crate::irc::S2SCommand;
use crate::irc::unreal::{IrcCommand, IrcMessage, SjoinParams};

// ---------------------------------------------------------------------------
// Nick sanitization
// ---------------------------------------------------------------------------

/// Characters allowed in IRC nicks per `UnrealIRCd` defaults.
fn is_valid_nick_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            '_' | '-' | '[' | ']' | '\\' | '^' | '{' | '}' | '|' | '`'
        )
}

/// Sanitize a Discord username into a valid IRC nick.
///
/// 1. Replace invalid chars with `_`.
/// 2. If result starts with a digit, prefix with `d`.
/// 3. Truncate to 30 characters.
#[must_use]
pub fn sanitize_nick(username: &str) -> String {
    let mut nick: String = username
        .chars()
        .map(|c| if is_valid_nick_char(c) { c } else { '_' })
        .collect();

    if nick.starts_with(|c: char| c.is_ascii_digit()) {
        nick.insert(0, 'd');
    }

    if nick.is_empty() {
        nick.push('_');
    }

    nick.truncate(30);
    nick
}

/// Resolve a unique nick using the collision fallback chain.
///
/// 1. Try `base` as-is.
/// 2. Append `_` up to 3 times.
/// 3. Truncate + append last 8 hex digits of Discord user ID.
/// 4. Final fallback: `d` + 6-char UID suffix (guaranteed unique).
#[must_use]
pub fn resolve_nick(
    base: &str,
    discord_user_id: u64,
    uid: &str,
    existing_nicks: &NickSet,
) -> String {
    // Try base nick
    if !existing_nicks.contains(base) {
        return base.to_string();
    }

    // Try appending underscores (up to 3)
    let mut candidate = base.to_string();
    for _ in 0..3 {
        candidate.push('_');
        if candidate.len() <= 30 && !existing_nicks.contains(&candidate) {
            return candidate;
        }
    }

    // Truncate + append 8 hex digits of Discord user ID
    let hex_suffix = format!("{:08x}", discord_user_id & 0xFFFF_FFFF);
    let max_prefix_len = 30 - hex_suffix.len(); // 22
    let mut hex_candidate = base.to_string();
    hex_candidate.truncate(max_prefix_len);
    hex_candidate.push_str(&hex_suffix);
    if !existing_nicks.contains(&hex_candidate) {
        return hex_candidate;
    }

    // Final fallback: d + 6-char UID suffix (guaranteed unique)
    uid_nick(uid)
}

/// Derive the guaranteed-unique nick from a UID: `d` + last 6 chars.
///
/// UID format: 3-char SID + 6 alphanumeric chars.
/// Example: `0D0ABCXYZ` → `dABCXYZ` (but we take last 6 of 9-char UID).
#[must_use]
pub fn uid_nick(uid: &str) -> String {
    debug_assert!(uid.len() >= 4, "UID too short: {uid}");
    let suffix = &uid[uid.len().saturating_sub(6)..];
    format!("d{suffix}")
}

// ---------------------------------------------------------------------------
// UID generation
// ---------------------------------------------------------------------------

/// Generates unique UIDs under a given SID.
///
/// UIDs are `<SID>` + 6 alphanumeric chars (`[A-Z0-9]`), stable per Discord
/// user ID for the duration of the session.
pub struct UidGenerator {
    sid: String,
    /// Maps Discord user ID → assigned UID for session stability.
    assigned: HashMap<u64, String>,
    /// Counter for generating the next unique suffix.
    counter: u64,
}

impl UidGenerator {
    /// Create a new generator for the given SID (3 chars).
    #[must_use]
    pub fn new(sid: &str) -> Self {
        debug_assert!(sid.len() == 3, "SID must be 3 characters: {sid}");
        Self {
            sid: sid.to_string(),
            assigned: HashMap::new(),
            counter: 0,
        }
    }

    /// Get or create a UID for a Discord user ID.
    ///
    /// Returns the same UID if called again with the same `discord_user_id`.
    pub fn get_or_create(&mut self, discord_user_id: u64) -> &str {
        self.assigned.entry(discord_user_id).or_insert_with(|| {
            let suffix = Self::encode_counter(self.counter);
            self.counter += 1;
            format!("{}{suffix}", self.sid)
        })
    }

    /// Remove a single UID assignment so the next `get_or_create` for this
    /// user allocates a fresh UID. Used after a KILL to avoid UID collisions.
    pub fn forget(&mut self, discord_user_id: u64) {
        self.assigned.remove(&discord_user_id);
    }

    /// Reset all assignments (e.g. on reconnect).
    pub fn reset(&mut self) {
        self.assigned.clear();
        self.counter = 0;
    }

    /// Encode a counter value as a 6-char `[A-Z0-9]` string.
    ///
    /// Uses base-36 encoding (0-9, A-Z) to maximise the UID space.
    /// 36^6 = 2,176,782,336 unique UIDs — more than sufficient.
    fn encode_counter(mut n: u64) -> String {
        const ALPHABET: &[u8; 36] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
        let mut chars = [b'A'; 6];
        for c in chars.iter_mut().rev() {
            *c = ALPHABET[(n % 36) as usize];
            n /= 36;
        }
        // SAFETY: all bytes are ASCII alphanumeric from ALPHABET
        String::from_utf8(chars.to_vec()).expect("encode_counter produced invalid UTF-8")
    }

    /// Look up the UID for a Discord user, if already assigned.
    #[must_use]
    pub fn lookup(&self, discord_user_id: u64) -> Option<&str> {
        self.assigned.get(&discord_user_id).map(String::as_str)
    }
}

// ---------------------------------------------------------------------------
// Nick set (case-insensitive)
// ---------------------------------------------------------------------------

/// A set of IRC nicks for collision detection.
///
/// IRC nicks are compared case-insensitively (ASCII).
#[derive(Debug, Default)]
pub struct NickSet {
    nicks: HashSet<String>,
}

impl NickSet {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a nick into the set.
    pub fn insert(&mut self, nick: &str) {
        self.nicks.insert(nick.to_ascii_lowercase());
    }

    /// Remove a nick from the set.
    pub fn remove(&mut self, nick: &str) {
        self.nicks.remove(&nick.to_ascii_lowercase());
    }

    /// Check if a nick is in the set (case-insensitive).
    #[must_use]
    pub fn contains(&self, nick: &str) -> bool {
        self.nicks.contains(&nick.to_ascii_lowercase())
    }
}

// ---------------------------------------------------------------------------
// Pseudoclient state
// ---------------------------------------------------------------------------

/// Per-pseudoclient state.
#[derive(Debug, Clone)]
pub struct PseudoclientState {
    pub discord_user_id: u64,
    pub uid: String,
    pub nick: String,
    /// Original Discord username (the unique @handle).
    pub username: String,
    pub display_name: String,
    pub channels: Vec<String>,
    pub presence: DiscordPresence,
    /// Set after a KILL during the burst window.  The pseudoclient will be
    /// reintroduced (with nick re-resolution) when `BurstComplete` arrives.
    pub needs_reintroduce: bool,
}

impl PseudoclientState {
    /// Build the `S2SCommand::IntroduceUser` for this pseudoclient.
    pub fn introduce_command(&self, ident: &str) -> S2SCommand {
        S2SCommand::IntroduceUser {
            uid: self.uid.clone(),
            nick: self.nick.clone(),
            ident: ident.to_string(),
            host: format!("{}.discord.com", self.discord_user_id),
            realname: self.display_name.clone(),
        }
    }

    /// Build the AWAY command for this pseudoclient's current presence.
    /// Returns `None` for Online (no away status).
    pub fn away_command(&self) -> Option<S2SCommand> {
        match self.presence {
            DiscordPresence::Idle => Some(S2SCommand::SetAway {
                uid: self.uid.clone(),
                reason: "Idle".to_string(),
            }),
            DiscordPresence::DoNotDisturb => Some(S2SCommand::SetAway {
                uid: self.uid.clone(),
                reason: "Do Not Disturb".to_string(),
            }),
            DiscordPresence::Offline => Some(S2SCommand::SetAway {
                uid: self.uid.clone(),
                reason: "Offline".to_string(),
            }),
            DiscordPresence::Online => None,
        }
    }
}

/// Manages all pseudoclients and their state.
pub struct PseudoclientManager {
    /// SID for our server link.
    sid: String,
    /// Ident for all pseudoclients (from config).
    ident: String,
    /// UID generator.
    uid_generator: UidGenerator,
    /// Discord user ID → state.
    by_discord_id: HashMap<u64, PseudoclientState>,
    /// IRC nick (lowercased) → Discord user ID.
    nick_to_discord: HashMap<String, u64>,
    /// IRC UID → Discord user ID.
    uid_to_discord: HashMap<String, u64>,
    /// Known nicks on the network (for collision detection).
    known_nicks: NickSet,
}

impl PseudoclientManager {
    /// Create a new manager with the given config values.
    #[must_use]
    pub fn new(sid: &str, ident: &str) -> Self {
        Self {
            sid: sid.to_string(),
            ident: ident.to_string(),
            uid_generator: UidGenerator::new(sid),
            by_discord_id: HashMap::new(),
            nick_to_discord: HashMap::new(),
            uid_to_discord: HashMap::new(),
            known_nicks: NickSet::new(),
        }
    }

    /// Register an external IRC nick (from burst or network events).
    pub fn register_external_nick(&mut self, nick: &str) {
        self.known_nicks.insert(nick);
    }

    /// Remove an external IRC nick (quit/nick change).
    pub fn unregister_external_nick(&mut self, nick: &str) {
        self.known_nicks.remove(nick);
    }

    /// Introduce a pseudoclient for a Discord user.
    ///
    /// Returns the allocated state (uid, nick, channels), or `None` if the
    /// user already has a pseudoclient (use `join_channel` to add channels).
    pub fn introduce(
        &mut self,
        discord_user_id: u64,
        username: &str,
        display_name: &str,
        channels: &[String],
        timestamp: u64,
        presence: DiscordPresence,
    ) -> Option<&PseudoclientState> {
        if self.by_discord_id.contains_key(&discord_user_id) {
            return None;
        }

        let uid = self
            .uid_generator
            .get_or_create(discord_user_id)
            .to_string();
        let base_nick = sanitize_nick(username);
        let nick = resolve_nick(&base_nick, discord_user_id, &uid, &self.known_nicks);

        let state = PseudoclientState {
            discord_user_id,
            uid: uid.clone(),
            nick: nick.clone(),
            username: username.to_string(),
            display_name: display_name.to_string(),
            channels: channels.to_vec(),
            presence,
            needs_reintroduce: false,
        };

        self.known_nicks.insert(&nick);
        self.nick_to_discord
            .insert(nick.to_ascii_lowercase(), discord_user_id);
        self.uid_to_discord.insert(uid, discord_user_id);
        self.by_discord_id.insert(discord_user_id, state);

        // Return a reference to the just-inserted state.
        let _ = timestamp; // preserved for API compatibility; callers use it for S2SCommand timestamps
        self.by_discord_id.get(&discord_user_id)
    }

    /// Remove a pseudoclient and all associated state.
    ///
    /// Returns the removed state, or `None` if no pseudoclient exists for
    /// this Discord user.
    pub fn quit(&mut self, discord_user_id: u64, _reason: &str) -> Option<PseudoclientState> {
        let state = self.by_discord_id.remove(&discord_user_id)?;
        self.known_nicks.remove(&state.nick);
        self.nick_to_discord
            .remove(&state.nick.to_ascii_lowercase());
        self.uid_to_discord.remove(&state.uid);
        Some(state)
    }

    /// Ensure a pseudoclient is in a channel.  Returns `JoinChannel` if
    /// they weren't already in it, `None` if they were (or don't exist).
    pub fn ensure_in_channel(
        &mut self,
        discord_user_id: u64,
        channel: &str,
        timestamp: u64,
    ) -> Option<S2SCommand> {
        let state = self.by_discord_id.get_mut(&discord_user_id)?;
        if state.channels.iter().any(|c| c == channel) {
            return None;
        }
        let uid = state.uid.clone();
        state.channels.push(channel.to_string());
        Some(S2SCommand::JoinChannel {
            uid,
            channel: channel.to_string(),
            ts: timestamp,
        })
    }

    /// Join an existing pseudoclient to an additional channel.
    ///
    /// Returns the SJOIN message, or `None` if the pseudoclient doesn't exist
    /// or is already in the channel.
    pub fn join_channel(
        &mut self,
        discord_user_id: u64,
        channel: &str,
        timestamp: u64,
    ) -> Option<IrcMessage> {
        let state = self.by_discord_id.get_mut(&discord_user_id)?;

        if state.channels.iter().any(|c| c == channel) {
            return None;
        }

        state.channels.push(channel.to_string());

        Some(IrcMessage {
            tags: vec![],
            prefix: Some(self.sid.clone()),
            command: IrcCommand::Sjoin(SjoinParams {
                timestamp,
                channel: channel.to_string(),
                modes: "+".to_string(),
                members: vec![state.uid.clone()],
            }),
        })
    }

    /// Part a pseudoclient from a channel.
    ///
    /// Returns `PartResult::Part` with the PART line, `PartResult::Quit` if
    /// the pseudoclient has no remaining channels, or `PartResult::NotFound`
    /// if the pseudoclient doesn't exist or isn't in the channel.
    pub fn part_channel(
        &mut self,
        discord_user_id: u64,
        channel: &str,
        reason: &str,
    ) -> PartResult {
        let Some(state) = self.by_discord_id.get_mut(&discord_user_id) else {
            return PartResult::NotFound;
        };

        let Some(idx) = state.channels.iter().position(|c| c == channel) else {
            return PartResult::NotFound;
        };

        state.channels.swap_remove(idx);

        if state.channels.is_empty() {
            // No remaining channels — quit the pseudoclient entirely
            let uid = state.uid.clone();
            let nick = state.nick.clone();
            self.by_discord_id.remove(&discord_user_id);
            self.known_nicks.remove(&nick);
            self.nick_to_discord.remove(&nick.to_ascii_lowercase());
            self.uid_to_discord.remove(&uid);
            PartResult::Quit(IrcMessage {
                tags: vec![],
                prefix: Some(uid),
                command: IrcCommand::Quit {
                    reason: reason.to_string(),
                },
            })
        } else {
            PartResult::Part(IrcMessage {
                tags: vec![],
                prefix: Some(state.uid.clone()),
                command: IrcCommand::Part {
                    channel: channel.to_string(),
                    reason: Some(reason.to_string()),
                },
            })
        }
    }

    /// Handle an SVSNICK: update the nick in all state maps.
    ///
    /// Returns `true` if the nick was updated, `false` if the UID is unknown.
    pub fn apply_svsnick(&mut self, uid: &str, new_nick: &str) -> bool {
        let Some(&discord_user_id) = self.uid_to_discord.get(uid) else {
            return false;
        };

        let Some(state) = self.by_discord_id.get_mut(&discord_user_id) else {
            return false;
        };

        let old_nick = state.nick.clone();
        self.known_nicks.remove(&old_nick);
        self.nick_to_discord.remove(&old_nick.to_ascii_lowercase());

        state.nick = new_nick.to_string();
        self.known_nicks.insert(new_nick);
        self.nick_to_discord
            .insert(new_nick.to_ascii_lowercase(), discord_user_id);

        true
    }

    /// Look up a pseudoclient by Discord user ID.
    #[must_use]
    pub fn get_by_discord_id(&self, discord_user_id: u64) -> Option<&PseudoclientState> {
        self.by_discord_id.get(&discord_user_id)
    }

    /// Mutable lookup by Discord user ID.
    pub fn get_by_discord_id_mut(
        &mut self,
        discord_user_id: u64,
    ) -> Option<&mut PseudoclientState> {
        self.by_discord_id.get_mut(&discord_user_id)
    }

    /// Look up a pseudoclient by IRC nick (case-insensitive).
    #[must_use]
    pub fn get_by_nick(&self, nick: &str) -> Option<&PseudoclientState> {
        let discord_id = self.nick_to_discord.get(&nick.to_ascii_lowercase())?;
        self.by_discord_id.get(discord_id)
    }

    /// Look up a pseudoclient by IRC UID.
    #[must_use]
    pub fn get_by_uid(&self, uid: &str) -> Option<&PseudoclientState> {
        let discord_id = self.uid_to_discord.get(uid)?;
        self.by_discord_id.get(discord_id)
    }

    /// Check if a UID belongs to one of our pseudoclients.
    #[must_use]
    pub fn is_our_uid(&self, uid: &str) -> bool {
        self.uid_to_discord.contains_key(uid)
    }

    /// Reset all state (e.g. on reconnect).
    pub fn reset(&mut self) {
        self.by_discord_id.clear();
        self.nick_to_discord.clear();
        self.uid_to_discord.clear();
        self.known_nicks = NickSet::new();
        self.uid_generator.reset();
    }

    /// Clear all registered external nicks. Called on link loss — the nicks
    /// will be re-registered from the burst on the next connection.
    pub fn clear_external_nicks(&mut self) {
        self.known_nicks = NickSet::new();
        // Re-add our own pseudoclient nicks so they're still tracked.
        for state in self.by_discord_id.values() {
            self.known_nicks.insert(&state.nick);
        }
    }

    /// Number of active pseudoclients.
    #[must_use]
    pub fn count(&self) -> usize {
        self.by_discord_id.len()
    }

    /// Clear the cached UID assignment for a Discord user so the next
    /// introduction allocates a fresh UID.  Used after KILL to avoid
    /// UID collisions with the recently-killed UID.
    pub fn forget_uid(&mut self, discord_user_id: u64) {
        self.uid_generator.forget(discord_user_id);
    }

    /// Returns `true` if no pseudoclients have been introduced.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_discord_id.is_empty()
    }

    /// Return the ident used for all pseudoclients.
    #[must_use]
    pub fn ident(&self) -> &str {
        &self.ident
    }

    /// Iterate over all active pseudoclient states.
    pub fn iter_states(&self) -> impl Iterator<Item = &PseudoclientState> {
        self.by_discord_id.values()
    }

    /// Remove a pseudoclient entry that was previously marked with
    /// `mark_needs_reintroduce`.  Unlike `quit`, this does NOT touch
    /// `known_nicks` — those were already cleaned up when the pseudoclient
    /// was marked.
    pub fn remove_marked(&mut self, discord_user_id: u64) -> Option<PseudoclientState> {
        self.by_discord_id.remove(&discord_user_id)
    }

    /// Clear all `needs_reintroduce` flags (e.g. on `LinkDown`; a fresh
    /// connection starts clean; all pseudoclients will be burst normally).
    pub fn clear_needs_reintroduce(&mut self) {
        for state in self.by_discord_id.values_mut() {
            state.needs_reintroduce = false;
        }
    }

    /// Mark a pseudoclient as needing reintroduction after a KILL.
    ///
    /// The entry stays in PM (the user is still desired) but the old UID
    /// is dead on the network.  `produce_burst_commands` will skip entries
    /// with this flag.  The orchestrator clears the flag after reintroduction.
    pub fn mark_needs_reintroduce(&mut self, discord_user_id: u64) {
        if let Some(state) = self.by_discord_id.get_mut(&discord_user_id) {
            state.needs_reintroduce = true;
            // Remove from nick/uid maps since the identity is dead on IRC.
            self.nick_to_discord
                .remove(&state.nick.to_ascii_lowercase());
            self.uid_to_discord.remove(&state.uid);
            self.known_nicks.remove(&state.nick);
        }
    }

    /// Rename a pseudoclient after a Discord username change.
    ///
    /// Sanitises the new username, resolves collisions against `known_nicks`,
    /// and updates all internal maps.  Returns `Some((old_nick, new_nick))` if
    /// the nick actually changed, `None` if the user doesn't exist or the
    /// username/nick is unchanged.
    pub fn rename(&mut self, discord_user_id: u64, new_username: &str) -> Option<(String, String)> {
        let state = self.by_discord_id.get(&discord_user_id)?;
        if state.username == new_username {
            return None;
        }

        let old_nick = state.nick.clone();
        let uid = state.uid.clone();

        // Resolve the new nick without our own nick in known_nicks (to
        // avoid treating our current nick as a collision with ourselves).
        self.known_nicks.remove(&old_nick);
        let new_nick = resolve_nick(
            &sanitize_nick(new_username),
            discord_user_id,
            &uid,
            &self.known_nicks,
        );
        // Always re-register a nick — either the old one (unchanged) or
        // the new one.  This avoids a remove-then-maybe-restore pattern.
        self.known_nicks.insert(&new_nick);

        if new_nick == old_nick {
            // Sanitised nick didn't change — just update the username.
            self.by_discord_id
                .get_mut(&discord_user_id)
                .expect("just looked up")
                .username = new_username.to_string();
            return None;
        }

        // Update nick maps: remove old, insert new.
        self.nick_to_discord.remove(&old_nick.to_ascii_lowercase());
        self.nick_to_discord
            .insert(new_nick.to_ascii_lowercase(), discord_user_id);

        let state = self
            .by_discord_id
            .get_mut(&discord_user_id)
            .expect("just looked up");
        state.nick.clone_from(&new_nick);
        state.username = new_username.to_string();

        Some((old_nick, new_nick))
    }

    /// Update the stored presence for a pseudoclient.
    ///
    /// Returns `true` if the pseudoclient was found and updated, `false` if
    /// no pseudoclient exists for `discord_user_id`.
    pub fn update_presence(&mut self, discord_user_id: u64, presence: DiscordPresence) -> bool {
        if let Some(state) = self.by_discord_id.get_mut(&discord_user_id) {
            state.presence = presence;
            true
        } else {
            false
        }
    }
}

/// Result of `part_channel`.
#[derive(Debug, PartialEq)]
pub enum PartResult {
    /// Pseudoclient parted but remains in other channels.
    Part(IrcMessage),
    /// Pseudoclient had no remaining channels and was quit.
    Quit(IrcMessage),
    /// Pseudoclient not found or not in that channel.
    NotFound,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -------------------------------------------------------------------
    // Nick sanitization
    // -------------------------------------------------------------------

    #[test]
    fn sanitize_clean_username() {
        assert_eq!(sanitize_nick("alice"), "alice");
    }

    #[test]
    fn sanitize_replaces_invalid_chars() {
        assert_eq!(sanitize_nick("hello world!"), "hello_world_");
    }

    #[test]
    fn sanitize_preserves_special_chars() {
        assert_eq!(
            sanitize_nick("a[b]c\\d^e{f}g|h`i-j_k"),
            "a[b]c\\d^e{f}g|h`i-j_k"
        );
    }

    #[test]
    fn sanitize_prefixes_digit_start() {
        assert_eq!(sanitize_nick("123abc"), "d123abc");
    }

    #[test]
    fn sanitize_truncates_to_30() {
        let long_name = "a".repeat(40);
        let result = sanitize_nick(&long_name);
        assert_eq!(result.len(), 30);
    }

    #[test]
    fn sanitize_digit_prefix_then_truncate() {
        // 30 digits → prefixed with 'd' → 31 chars → truncated to 30
        let digits = "1".repeat(30);
        let result = sanitize_nick(&digits);
        assert_eq!(result.len(), 30);
        assert!(result.starts_with('d'));
    }

    #[test]
    fn sanitize_empty_username() {
        assert_eq!(sanitize_nick(""), "_");
    }

    #[test]
    fn sanitize_all_invalid_chars() {
        assert_eq!(sanitize_nick("@#$"), "___");
    }

    #[test]
    fn sanitize_unicode_username() {
        assert_eq!(sanitize_nick("ünïcödé"), "_n_c_d_");
    }

    // -------------------------------------------------------------------
    // Nick collision resolution
    // -------------------------------------------------------------------

    #[test]
    fn resolve_no_collision() {
        let nicks = NickSet::new();
        assert_eq!(resolve_nick("alice", 12345, "0D0AAAAAA", &nicks), "alice");
    }

    #[test]
    fn resolve_underscore_fallback() {
        let mut nicks = NickSet::new();
        nicks.insert("alice");
        assert_eq!(resolve_nick("alice", 12345, "0D0AAAAAA", &nicks), "alice_");
    }

    #[test]
    fn resolve_multiple_underscores() {
        let mut nicks = NickSet::new();
        nicks.insert("alice");
        nicks.insert("alice_");
        assert_eq!(resolve_nick("alice", 12345, "0D0AAAAAA", &nicks), "alice__");
    }

    #[test]
    fn resolve_three_underscores() {
        let mut nicks = NickSet::new();
        nicks.insert("alice");
        nicks.insert("alice_");
        nicks.insert("alice__");
        assert_eq!(
            resolve_nick("alice", 12345, "0D0AAAAAA", &nicks),
            "alice___"
        );
    }

    #[test]
    fn resolve_hex_fallback() {
        let mut nicks = NickSet::new();
        nicks.insert("alice");
        nicks.insert("alice_");
        nicks.insert("alice__");
        nicks.insert("alice___");
        let result = resolve_nick("alice", 0xDEAD_BEEF, "0D0AAAAAA", &nicks);
        assert_eq!(result, "alicedeadbeef");
    }

    #[test]
    fn resolve_hex_truncates_base() {
        let mut nicks = NickSet::new();
        let long_nick = "a".repeat(30);
        nicks.insert(&long_nick);
        // Underscores would exceed 30, so they're skipped
        // Hex fallback: truncate base to 22 + 8 hex = 30
        let result = resolve_nick(&long_nick, 0xDEAD_BEEF, "0D0AAAAAA", &nicks);
        assert_eq!(result.len(), 30);
        assert!(result.ends_with("deadbeef"));
    }

    #[test]
    fn resolve_uid_final_fallback() {
        let mut nicks = NickSet::new();
        nicks.insert("alice");
        nicks.insert("alice_");
        nicks.insert("alice__");
        nicks.insert("alice___");
        nicks.insert("alicedeadbeef");
        let result = resolve_nick("alice", 0xDEAD_BEEF, "0D0ABCDEF", &nicks);
        assert_eq!(result, "dABCDEF");
    }

    #[test]
    fn resolve_case_insensitive() {
        let mut nicks = NickSet::new();
        nicks.insert("Alice");
        assert_eq!(resolve_nick("alice", 12345, "0D0AAAAAA", &nicks), "alice_");
    }

    // -------------------------------------------------------------------
    // uid_nick
    // -------------------------------------------------------------------

    #[test]
    fn uid_nick_standard() {
        assert_eq!(uid_nick("0D0ABCDEF"), "dABCDEF");
    }

    // -------------------------------------------------------------------
    // UID generation
    // -------------------------------------------------------------------

    #[test]
    fn uid_generator_creates_unique() {
        let mut uid_gen = UidGenerator::new("0D0");
        let uid1 = uid_gen.get_or_create(100).to_string();
        let uid2 = uid_gen.get_or_create(200).to_string();
        assert_ne!(uid1, uid2);
        assert!(uid1.starts_with("0D0"));
        assert!(uid2.starts_with("0D0"));
        assert_eq!(uid1.len(), 9);
    }

    #[test]
    fn uid_generator_stable_per_user() {
        let mut uid_gen = UidGenerator::new("0D0");
        let uid1 = uid_gen.get_or_create(100).to_string();
        let uid2 = uid_gen.get_or_create(100).to_string();
        assert_eq!(uid1, uid2);
    }

    #[test]
    fn uid_generator_reset_clears() {
        let mut uid_gen = UidGenerator::new("0D0");
        let uid1 = uid_gen.get_or_create(100).to_string();
        uid_gen.reset();
        assert!(uid_gen.lookup(100).is_none());
        // After reset, same user gets a fresh UID (counter reset too)
        let uid2 = uid_gen.get_or_create(100).to_string();
        assert_eq!(uid1, uid2); // counter resets to 0, so same encoding
    }

    #[test]
    fn uid_generator_lookup() {
        let mut uid_gen = UidGenerator::new("0D0");
        assert!(uid_gen.lookup(100).is_none());
        uid_gen.get_or_create(100);
        assert!(uid_gen.lookup(100).is_some());
    }

    #[test]
    fn uid_encode_counter_zero() {
        assert_eq!(UidGenerator::encode_counter(0), "AAAAAA");
    }

    #[test]
    fn uid_encode_counter_one() {
        assert_eq!(UidGenerator::encode_counter(1), "AAAAAB");
    }

    #[test]
    fn uid_encode_counter_36() {
        // 36 in base-36 = "BA" (10), padded to 6 = "AAAABA"
        assert_eq!(UidGenerator::encode_counter(36), "AAAABA");
    }

    // -------------------------------------------------------------------
    // NickSet
    // -------------------------------------------------------------------

    #[test]
    fn nickset_case_insensitive() {
        let mut set = NickSet::new();
        set.insert("Alice");
        assert!(set.contains("alice"));
        assert!(set.contains("ALICE"));
        assert!(set.contains("Alice"));
    }

    #[test]
    fn nickset_remove() {
        let mut set = NickSet::new();
        set.insert("alice");
        assert!(set.contains("alice"));
        set.remove("Alice");
        assert!(!set.contains("alice"));
    }

    // -------------------------------------------------------------------
    // PseudoclientManager — introduce
    // -------------------------------------------------------------------

    fn make_manager() -> PseudoclientManager {
        PseudoclientManager::new("0D0", "discord")
    }

    #[test]
    fn introduce_creates_state_and_updates_maps() {
        let mut mgr = make_manager();
        let state = mgr
            .introduce(
                100,
                "alice",
                "Alice Display",
                &["#test".to_string()],
                1000,
                DiscordPresence::Online,
            )
            .expect("should introduce");

        // Returned state has correct fields.
        assert_eq!(state.nick, "alice");
        assert_eq!(state.display_name, "Alice Display");
        assert_eq!(state.discord_user_id, 100);
        assert_eq!(state.channels, vec!["#test".to_string()]);
        assert!(!state.uid.is_empty());

        // Internal maps are updated.
        assert!(!mgr.is_empty());
        assert!(mgr.get_by_discord_id(100).is_some());
        assert!(mgr.get_by_nick("alice").is_some());
        assert!(mgr.get_by_nick("Alice").is_some()); // case-insensitive
        assert_eq!(mgr.count(), 1);
    }

    #[test]
    fn introduce_multiple_channels() {
        let mut mgr = make_manager();
        let channels = vec!["#a".to_string(), "#b".to_string(), "#c".to_string()];
        let state = mgr
            .introduce(
                100,
                "alice",
                "Alice",
                &channels,
                1000,
                DiscordPresence::Online,
            )
            .expect("should introduce");

        assert_eq!(state.channels, channels);
    }

    #[test]
    fn introduce_duplicate_returns_none() {
        let mut mgr = make_manager();
        mgr.introduce(
            100,
            "alice",
            "Alice",
            &["#test".to_string()],
            1000,
            DiscordPresence::Online,
        );
        let result = mgr.introduce(
            100,
            "alice",
            "Alice",
            &["#test".to_string()],
            1000,
            DiscordPresence::Online,
        );
        assert!(result.is_none());
    }

    #[test]
    fn is_empty_true_when_no_pseudoclients() {
        let mgr = make_manager();
        assert!(mgr.is_empty());
    }

    #[test]
    fn introduce_with_collision() {
        let mut mgr = make_manager();
        mgr.register_external_nick("alice");
        let state = mgr
            .introduce(
                100,
                "alice",
                "Alice",
                &["#test".to_string()],
                1000,
                DiscordPresence::Online,
            )
            .expect("should introduce");

        assert_eq!(state.nick, "alice_");
        assert!(mgr.get_by_nick("alice_").is_some());
    }

    // -------------------------------------------------------------------
    // PseudoclientManager — quit
    // -------------------------------------------------------------------

    #[test]
    fn quit_generates_message() {
        let mut mgr = make_manager();
        mgr.introduce(
            100,
            "alice",
            "Alice",
            &["#test".to_string()],
            1000,
            DiscordPresence::Online,
        );
        let uid = mgr.get_by_discord_id(100).unwrap().uid.clone();
        let removed = mgr.quit(100, "Disconnected from Discord").unwrap();
        assert_eq!(removed.uid, uid);
        assert_eq!(removed.discord_user_id, 100);
    }

    #[test]
    fn quit_cleans_state() {
        let mut mgr = make_manager();
        mgr.introduce(
            100,
            "alice",
            "Alice",
            &["#test".to_string()],
            1000,
            DiscordPresence::Online,
        );
        mgr.quit(100, "bye");
        assert!(mgr.get_by_discord_id(100).is_none());
        assert!(mgr.get_by_nick("alice").is_none());
        assert_eq!(mgr.count(), 0);
    }

    #[test]
    fn quit_unknown_returns_none() {
        let mut mgr = make_manager();
        assert!(mgr.quit(999, "bye").is_none());
    }

    // -------------------------------------------------------------------
    // PseudoclientManager — rename
    // -------------------------------------------------------------------

    #[test]
    fn rename_changes_nick_and_username() {
        let mut mgr = make_manager();
        mgr.introduce(
            100,
            "oldname",
            "Old",
            &["#test".to_string()],
            1000,
            DiscordPresence::Online,
        );

        let result = mgr.rename(100, "newname");
        assert_eq!(result, Some(("oldname".to_string(), "newname".to_string())));

        let ps = mgr.get_by_discord_id(100).unwrap();
        assert_eq!(ps.nick, "newname");
        assert_eq!(ps.username, "newname");
        // Old nick should be gone from lookup.
        assert!(mgr.get_by_nick("oldname").is_none());
        // New nick should work.
        assert!(mgr.get_by_nick("newname").is_some());
    }

    #[test]
    fn rename_same_username_returns_none() {
        let mut mgr = make_manager();
        mgr.introduce(
            100,
            "same",
            "Same",
            &["#test".to_string()],
            1000,
            DiscordPresence::Online,
        );

        assert_eq!(mgr.rename(100, "same"), None);
        assert_eq!(mgr.get_by_discord_id(100).unwrap().nick, "same");
    }

    #[test]
    fn rename_with_collision_suffixes_nick() {
        let mut mgr = make_manager();
        mgr.introduce(
            100,
            "alice",
            "Alice",
            &["#test".to_string()],
            1000,
            DiscordPresence::Online,
        );
        // Register external nick "bob" so renaming to "bob" collides.
        mgr.register_external_nick("bob");

        let result = mgr.rename(100, "bob");
        assert!(result.is_some());
        let (old, new) = result.unwrap();
        assert_eq!(old, "alice");
        assert_ne!(new, "bob", "should be suffixed due to collision");
        assert!(
            new.starts_with("bob"),
            "should start with 'bob'; got: {new}"
        );
    }

    #[test]
    fn rename_nonexistent_returns_none() {
        let mut mgr = make_manager();
        assert_eq!(mgr.rename(999, "anything"), None);
    }

    // -------------------------------------------------------------------
    // PseudoclientManager — join/part channel
    // -------------------------------------------------------------------

    #[test]
    fn join_channel_generates_sjoin() {
        let mut mgr = make_manager();
        mgr.introduce(
            100,
            "alice",
            "Alice",
            &["#a".to_string()],
            1000,
            DiscordPresence::Online,
        );
        let msg = mgr.join_channel(100, "#b", 1001).unwrap();
        assert_eq!(msg.prefix, Some("0D0".to_string()));
        let IrcCommand::Sjoin(ref s) = msg.command else {
            panic!("expected Sjoin");
        };
        assert_eq!(s.channel, "#b");
        assert_eq!(s.timestamp, 1001);
    }

    #[test]
    fn join_channel_already_in_returns_none() {
        let mut mgr = make_manager();
        mgr.introduce(
            100,
            "alice",
            "Alice",
            &["#a".to_string()],
            1000,
            DiscordPresence::Online,
        );
        assert!(mgr.join_channel(100, "#a", 1001).is_none());
    }

    #[test]
    fn join_channel_unknown_user_returns_none() {
        let mut mgr = make_manager();
        assert!(mgr.join_channel(999, "#a", 1001).is_none());
    }

    #[test]
    fn part_channel_with_remaining() {
        let mut mgr = make_manager();
        mgr.introduce(
            100,
            "alice",
            "Alice",
            &["#a".to_string(), "#b".to_string()],
            1000,
            DiscordPresence::Online,
        );
        let result = mgr.part_channel(100, "#a", "Bridge channel removed");
        let PartResult::Part(ref msg) = result else {
            panic!("expected Part, got {result:?}");
        };
        assert_eq!(
            msg.command,
            IrcCommand::Part {
                channel: "#a".to_string(),
                reason: Some("Bridge channel removed".to_string()),
            }
        );
        // Still in #b
        assert!(mgr.get_by_discord_id(100).is_some());
    }

    #[test]
    fn part_channel_last_channel_quits() {
        let mut mgr = make_manager();
        mgr.introduce(
            100,
            "alice",
            "Alice",
            &["#a".to_string()],
            1000,
            DiscordPresence::Online,
        );
        let result = mgr.part_channel(100, "#a", "Bridge channel removed");
        let PartResult::Quit(ref msg) = result else {
            panic!("expected Quit, got {result:?}");
        };
        assert!(
            matches!(&msg.command, IrcCommand::Quit { reason } if reason == "Bridge channel removed")
        );
        assert!(mgr.get_by_discord_id(100).is_none());
    }

    #[test]
    fn part_channel_not_found() {
        let mut mgr = make_manager();
        assert_eq!(mgr.part_channel(999, "#a", "bye"), PartResult::NotFound);
    }

    #[test]
    fn part_channel_not_in_channel() {
        let mut mgr = make_manager();
        mgr.introduce(
            100,
            "alice",
            "Alice",
            &["#a".to_string()],
            1000,
            DiscordPresence::Online,
        );
        assert_eq!(mgr.part_channel(100, "#b", "bye"), PartResult::NotFound);
    }

    // -------------------------------------------------------------------
    // PseudoclientManager — SVSNICK
    // -------------------------------------------------------------------

    #[test]
    fn svsnick_updates_nick() {
        let mut mgr = make_manager();
        mgr.introduce(
            100,
            "alice",
            "Alice",
            &["#test".to_string()],
            1000,
            DiscordPresence::Online,
        );
        let uid = mgr.get_by_discord_id(100).unwrap().uid.clone();

        assert!(mgr.apply_svsnick(&uid, "forced_nick"));
        assert!(mgr.get_by_nick("forced_nick").is_some());
        assert!(mgr.get_by_nick("alice").is_none());
        assert_eq!(mgr.get_by_discord_id(100).unwrap().nick, "forced_nick");
    }

    #[test]
    fn svsnick_unknown_uid_returns_false() {
        let mut mgr = make_manager();
        assert!(!mgr.apply_svsnick("UNKNOWN", "newnick"));
    }

    #[test]
    fn unregister_external_nick_frees_for_reuse() {
        let mut mgr = make_manager();
        mgr.register_external_nick("bob");
        // bob is taken — collision
        let state = mgr
            .introduce(
                100,
                "bob",
                "Bob",
                &["#test".to_string()],
                1000,
                DiscordPresence::Online,
            )
            .expect("should introduce");
        assert_eq!(state.nick, "bob_");

        // Unregister bob — now bob_ pseudoclient exists, but bob is free
        mgr.unregister_external_nick("bob");
        // Introduce another user named bob — should get "bob" this time
        let state2 = mgr
            .introduce(
                200,
                "bob",
                "Bob2",
                &["#test".to_string()],
                1001,
                DiscordPresence::Online,
            )
            .expect("should introduce");
        assert_eq!(state2.nick, "bob");
    }

    // -------------------------------------------------------------------
    // PseudoclientManager — lookups and reset
    // -------------------------------------------------------------------

    #[test]
    fn is_our_uid() {
        let mut mgr = make_manager();
        mgr.introduce(
            100,
            "alice",
            "Alice",
            &["#test".to_string()],
            1000,
            DiscordPresence::Online,
        );
        let uid = mgr.get_by_discord_id(100).unwrap().uid.clone();
        assert!(mgr.is_our_uid(&uid));
        assert!(!mgr.is_our_uid("UNKNOWN"));
    }

    #[test]
    fn get_by_uid() {
        let mut mgr = make_manager();
        mgr.introduce(
            100,
            "alice",
            "Alice",
            &["#test".to_string()],
            1000,
            DiscordPresence::Online,
        );
        let uid = mgr.get_by_discord_id(100).unwrap().uid.clone();
        let state = mgr.get_by_uid(&uid).unwrap();
        assert_eq!(state.discord_user_id, 100);
    }

    #[test]
    fn reset_clears_everything() {
        let mut mgr = make_manager();
        mgr.introduce(
            100,
            "alice",
            "Alice",
            &["#test".to_string()],
            1000,
            DiscordPresence::Online,
        );
        mgr.register_external_nick("bob");
        mgr.reset();
        assert_eq!(mgr.count(), 0);
        assert!(mgr.get_by_discord_id(100).is_none());
        assert!(mgr.get_by_nick("alice").is_none());
        // External nicks are also cleared (network state rebuilt on reconnect)
        assert!(!mgr.known_nicks.contains("bob"));
    }

    // -------------------------------------------------------------------
    // Proptest
    // -------------------------------------------------------------------

    use proptest::prelude::*;

    proptest! {
        #[test]
        fn sanitize_never_panics(s in "\\PC{0,100}") {
            let _ = sanitize_nick(&s);
        }

        #[test]
        fn sanitize_result_is_valid_irc_nick(s in "[a-zA-Z0-9_@#\\$\\.\\-\\[\\]\\\\\\^\\{\\}\\|`]{1,50}") {
            let nick = sanitize_nick(&s);
            assert!(!nick.is_empty());
            assert!(nick.len() <= 30);
            // Must not start with a digit
            assert!(!nick.starts_with(|c: char| c.is_ascii_digit()));
            // All chars must be valid
            assert!(nick.chars().all(|c| is_valid_nick_char(c) || c == 'd'));
        }

        #[test]
        fn uid_generator_always_unique(ids in proptest::collection::vec(0u64..1_000_000, 1..100)) {
            let mut uid_gen = UidGenerator::new("0D0");
            let mut seen = std::collections::HashSet::new();
            for &id in &ids {
                let uid = uid_gen.get_or_create(id).to_string();
                // Same ID always gets same UID
                let uid2 = uid_gen.get_or_create(id).to_string();
                assert_eq!(uid, uid2);
                seen.insert(uid);
            }
            // Unique IDs → unique UIDs
            let unique_ids: std::collections::HashSet<_> = ids.iter().copied().collect();
            assert_eq!(seen.len(), unique_ids.len());
        }

        #[test]
        fn resolve_nick_never_panics(
            base in "[a-zA-Z_]{1,30}",
            discord_id in 0u64..u64::MAX,
        ) {
            let nicks = NickSet::new();
            let uid = format!("0D0{:06}", discord_id % 1_000_000);
            let _ = resolve_nick(&base, discord_id, &uid, &nicks);
        }

        #[test]
        fn encode_counter_always_6_chars(n in 0u64..2_000_000u64) {
            let result = UidGenerator::encode_counter(n);
            assert_eq!(result.len(), 6);
            assert!(result.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()));
        }
    }
}
