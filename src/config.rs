use std::path::{Path, PathBuf};

use serde::Deserialize;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    Parse(toml::de::Error),
    Validation(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "could not read config file: {e}"),
            Self::Parse(e) => write!(f, "config file is not valid TOML: {e}"),
            Self::Validation(msg) => write!(f, "invalid config: {msg}"),
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Parse(e) => Some(e),
            Self::Validation(_) => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Load and deserialize the config file at `path`.
pub fn load(path: impl AsRef<Path>) -> Result<Config, ConfigError> {
    let contents = std::fs::read_to_string(path.as_ref()).map_err(ConfigError::Io)?;
    toml::from_str(&contents).map_err(ConfigError::Parse)
}

/// Return the config file path from a CLI argument iterator.
///
/// Looks for `--config <path>`; defaults to `config.toml` if not found.
/// Accepts an iterator so it can be tested without touching `std::env::args()`.
pub fn config_path_from_iter(mut args: impl Iterator<Item = String>) -> PathBuf {
    args.next(); // skip argv[0] (program name)
    while let Some(arg) = args.next() {
        if arg == "--config"
            && let Some(path) = args.next()
        {
            return PathBuf::from(path);
        }
    }
    PathBuf::from("config.toml")
}

/// Return the config file path from the process's command-line arguments.
pub fn config_path_from_args() -> PathBuf {
    config_path_from_iter(std::env::args())
}

// ---------------------------------------------------------------------------
// Structs
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, PartialEq)]
pub struct Config {
    pub discord: DiscordConfig,
    pub irc: IrcConfig,
    #[serde(default)]
    pub pseudoclients: PseudoclientConfig,
    #[serde(rename = "bridge")]
    pub bridges: Vec<BridgeEntry>,
}

#[derive(Debug, Deserialize, PartialEq)]
pub struct DiscordConfig {
    pub token: String,
}

#[derive(Debug, Deserialize, PartialEq)]
pub struct IrcConfig {
    pub uplink: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_tls")]
    pub tls: bool,
    pub link_name: String,
    pub link_password: String,
    pub sid: String,
    #[serde(default = "default_description")]
    pub description: String,
}

#[derive(Debug, Deserialize, PartialEq)]
pub struct PseudoclientConfig {
    #[serde(default = "default_host_suffix")]
    pub host_suffix: String,
    #[serde(default = "default_ident")]
    pub ident: String,
}

#[derive(Debug, Deserialize, PartialEq, Clone)]
pub struct BridgeEntry {
    pub discord_channel_id: String,
    pub irc_channel: String,
    pub webhook_url: Option<String>,
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

impl Config {
    /// Validate all fields according to the rules in `specs/01-configuration.md`.
    /// Returns `Err(ConfigError::Validation(...))` on the first violation found.
    pub fn validate(&self) -> Result<(), ConfigError> {
        validate_sid(&self.irc.sid)?;
        validate_link_name(&self.irc.link_name)?;

        if self.bridges.is_empty() {
            return Err(ConfigError::Validation(
                "at least one [[bridge]] entry is required".into(),
            ));
        }

        for entry in &self.bridges {
            validate_discord_channel_id(&entry.discord_channel_id)?;
            validate_irc_channel(&entry.irc_channel)?;
            if let Some(url) = &entry.webhook_url {
                validate_webhook_url(url)?;
            }
        }

        validate_no_duplicates(&self.bridges)
    }
}

/// SID must match `[0-9][A-Z0-9]{2}`.
fn validate_sid(sid: &str) -> Result<(), ConfigError> {
    let mut chars = sid.chars();
    let valid = sid.len() == 3
        && chars.next().is_some_and(|c| c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
    if valid {
        Ok(())
    } else {
        Err(ConfigError::Validation(format!(
            "irc.sid {sid:?} is invalid: must match [0-9][A-Z0-9]{{2}}"
        )))
    }
}

/// Server name must be hostname-like: two or more dot-separated labels,
/// each label non-empty, containing only `[A-Za-z0-9-]`, not starting or
/// ending with `-`.
fn validate_link_name(name: &str) -> Result<(), ConfigError> {
    let err = || {
        ConfigError::Validation(format!(
            "irc.link_name {name:?} is invalid: must be a valid server hostname (e.g. discord.example.net)"
        ))
    };

    if name.is_empty() {
        return Err(err());
    }

    let labels: Vec<&str> = name.split('.').collect();
    if labels.len() < 2 {
        return Err(err()); // no dot → not a server name
    }

    for label in &labels {
        if label.is_empty() {
            return Err(err()); // consecutive dots or leading/trailing dot
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(err());
        }
        if !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err(err());
        }
    }

    Ok(())
}

/// Discord channel ID must be a non-empty string of ASCII digits.
fn validate_discord_channel_id(id: &str) -> Result<(), ConfigError> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_digit()) {
        return Err(ConfigError::Validation(format!(
            "bridge discord_channel_id {id:?} is invalid: must be a non-empty string of digits"
        )));
    }
    Ok(())
}

/// IRC channel must start with `#`.
fn validate_irc_channel(channel: &str) -> Result<(), ConfigError> {
    if !channel.starts_with('#') {
        return Err(ConfigError::Validation(format!(
            "bridge irc_channel {channel:?} is invalid: must start with '#'"
        )));
    }
    Ok(())
}

/// Webhook URL must be HTTPS with host `discord.com` or `discordapp.com`.
fn validate_webhook_url(url: &str) -> Result<(), ConfigError> {
    let err = || {
        ConfigError::Validation(format!(
            "bridge webhook_url {url:?} is invalid: must be an HTTPS URL with host discord.com or discordapp.com"
        ))
    };

    let rest = url.strip_prefix("https://").ok_or_else(err)?;
    let host = rest.split('/').next().unwrap_or("");
    if host != "discord.com" && host != "discordapp.com" {
        return Err(err());
    }
    Ok(())
}

/// No two bridge entries may share a `discord_channel_id` or `irc_channel`.
fn validate_no_duplicates(bridges: &[BridgeEntry]) -> Result<(), ConfigError> {
    let mut discord_ids = std::collections::HashSet::new();
    let mut irc_channels = std::collections::HashSet::new();

    for entry in bridges {
        if !discord_ids.insert(entry.discord_channel_id.as_str()) {
            return Err(ConfigError::Validation(format!(
                "duplicate discord_channel_id {:?} in [[bridge]] entries",
                entry.discord_channel_id
            )));
        }
        if !irc_channels.insert(entry.irc_channel.as_str()) {
            return Err(ConfigError::Validation(format!(
                "duplicate irc_channel {:?} in [[bridge]] entries",
                entry.irc_channel
            )));
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Reload diff
// ---------------------------------------------------------------------------

/// Describes the changes between two sets of bridge entries.
#[derive(Debug, PartialEq)]
pub struct BridgeDiff {
    pub added: Vec<BridgeEntry>,
    pub removed: Vec<BridgeEntry>,
    pub webhook_changed: Vec<BridgeEntry>,
}

impl BridgeDiff {
    /// Returns `true` when nothing changed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.webhook_changed.is_empty()
    }
}

/// Compute the diff between `old` and `new` bridge entry slices.
///
/// Entries are keyed on `(discord_channel_id, irc_channel)`.  An entry
/// present in `new` but not `old` is *added*; present in `old` but not `new`
/// is *removed*.  If the key matches but `webhook_url` differs, the entry
/// appears in `webhook_changed` (with the **new** value).
#[must_use]
pub fn diff_bridges(old: &[BridgeEntry], new: &[BridgeEntry]) -> BridgeDiff {
    use std::collections::HashMap;

    type Key<'a> = (&'a str, &'a str);

    let old_map: HashMap<Key<'_>, &BridgeEntry> = old
        .iter()
        .map(|e| ((e.discord_channel_id.as_str(), e.irc_channel.as_str()), e))
        .collect();

    let new_map: HashMap<Key<'_>, &BridgeEntry> = new
        .iter()
        .map(|e| ((e.discord_channel_id.as_str(), e.irc_channel.as_str()), e))
        .collect();

    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut webhook_changed = Vec::new();

    for (key, new_entry) in &new_map {
        match old_map.get(key) {
            None => added.push((*new_entry).clone()),
            Some(old_entry) if old_entry.webhook_url != new_entry.webhook_url => {
                webhook_changed.push((*new_entry).clone());
            }
            Some(_) => {} // unchanged
        }
    }

    for (key, old_entry) in &old_map {
        if !new_map.contains_key(key) {
            removed.push((*old_entry).clone());
        }
    }

    BridgeDiff {
        added,
        removed,
        webhook_changed,
    }
}

/// Check whether any non-reloadable fields differ between two configs.
///
/// Returns a list of human-readable field names that changed.  The caller
/// should log these at `WARN` level.
#[must_use]
pub fn non_reloadable_changes(old: &Config, new: &Config) -> Vec<&'static str> {
    let mut changed = Vec::new();

    if old.discord.token != new.discord.token {
        changed.push("discord.token");
    }
    if old.irc.uplink != new.irc.uplink {
        changed.push("irc.uplink");
    }
    if old.irc.port != new.irc.port {
        changed.push("irc.port");
    }
    if old.irc.tls != new.irc.tls {
        changed.push("irc.tls");
    }
    if old.irc.link_name != new.irc.link_name {
        changed.push("irc.link_name");
    }
    if old.irc.link_password != new.irc.link_password {
        changed.push("irc.link_password");
    }
    if old.irc.sid != new.irc.sid {
        changed.push("irc.sid");
    }
    if old.irc.description != new.irc.description {
        changed.push("irc.description");
    }
    if old.pseudoclients.host_suffix != new.pseudoclients.host_suffix {
        changed.push("pseudoclients.host_suffix");
    }
    if old.pseudoclients.ident != new.pseudoclients.ident {
        changed.push("pseudoclients.ident");
    }

    changed
}

/// Attempt to reload config from `path`, validating before returning the diff.
///
/// On success returns `Ok((new_config, diff))`.  On failure returns an error;
/// the caller should log it and keep the old config.
pub fn reload(
    path: impl AsRef<Path>,
    current: &Config,
) -> Result<(Config, BridgeDiff), ConfigError> {
    let new = load(path)?;
    new.validate()?;

    let diff = diff_bridges(&current.bridges, &new.bridges);
    Ok((new, diff))
}

// ---------------------------------------------------------------------------
// Serde defaults
// ---------------------------------------------------------------------------

fn default_port() -> u16 {
    6900
}

fn default_tls() -> bool {
    true
}

fn default_description() -> String {
    "Discord bridge".to_string()
}

fn default_host_suffix() -> String {
    "discord".to_string()
}

fn default_ident() -> String {
    "discord".to_string()
}

impl Default for PseudoclientConfig {
    fn default() -> Self {
        Self {
            host_suffix: default_host_suffix(),
            ident: default_ident(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml: &str) -> Config {
        toml::from_str(toml).expect("valid config should parse")
    }

    const MINIMAL_TOML: &str = r##"
        [discord]
        token = "Bot abc123"

        [irc]
        uplink = "irc.example.net"
        link_name = "discord.example.net"
        link_password = "secret"
        sid = "0D0"

        [[bridge]]
        discord_channel_id = "123456789012345678"
        irc_channel = "#general"
    "##;

    const FULL_TOML: &str = r##"
        [discord]
        token = "Bot abc123"

        [irc]
        uplink = "irc.example.net"
        port = 7000
        tls = false
        link_name = "discord.example.net"
        link_password = "secret"
        sid = "0D0"
        description = "My bridge"

        [pseudoclients]
        host_suffix = "users.example.net"
        ident = "bridge"

        [[bridge]]
        discord_channel_id = "123456789012345678"
        irc_channel = "#general"
        webhook_url = "https://discord.com/api/webhooks/111/aaa"

        [[bridge]]
        discord_channel_id = "987654321098765432"
        irc_channel = "#dev"
    "##;

    #[test]
    fn minimal_config_parses() {
        let cfg = parse(MINIMAL_TOML);
        assert_eq!(cfg.discord.token, "Bot abc123");
        assert_eq!(cfg.irc.uplink, "irc.example.net");
        assert_eq!(cfg.irc.link_name, "discord.example.net");
        assert_eq!(cfg.irc.link_password, "secret");
        assert_eq!(cfg.irc.sid, "0D0");
        assert_eq!(cfg.bridges.len(), 1);
        assert_eq!(cfg.bridges[0].discord_channel_id, "123456789012345678");
        assert_eq!(cfg.bridges[0].irc_channel, "#general");
        assert_eq!(cfg.bridges[0].webhook_url, None);
    }

    #[test]
    fn optional_fields_have_correct_defaults() {
        let cfg = parse(MINIMAL_TOML);
        assert_eq!(cfg.irc.port, 6900);
        assert!(cfg.irc.tls);
        assert_eq!(cfg.irc.description, "Discord bridge");
        assert_eq!(cfg.pseudoclients.host_suffix, "discord");
        assert_eq!(cfg.pseudoclients.ident, "discord");
    }

    #[test]
    fn full_config_parses() {
        let cfg = parse(FULL_TOML);
        assert_eq!(cfg.irc.port, 7000);
        assert!(!cfg.irc.tls);
        assert_eq!(cfg.irc.description, "My bridge");
        assert_eq!(cfg.pseudoclients.host_suffix, "users.example.net");
        assert_eq!(cfg.pseudoclients.ident, "bridge");
        assert_eq!(cfg.bridges.len(), 2);
        assert_eq!(
            cfg.bridges[0].webhook_url.as_deref(),
            Some("https://discord.com/api/webhooks/111/aaa")
        );
        assert_eq!(cfg.bridges[1].webhook_url, None);
    }

    #[test]
    fn missing_required_discord_token_fails() {
        let toml = r##"
            [irc]
            uplink = "irc.example.net"
            link_name = "discord.example.net"
            link_password = "secret"
            sid = "0D0"

            [[bridge]]
            discord_channel_id = "123456789012345678"
            irc_channel = "#general"
        "##;
        assert!(toml::from_str::<Config>(toml).is_err());
    }

    #[test]
    fn missing_required_irc_fields_fails() {
        // Missing sid
        let toml = r##"
            [discord]
            token = "Bot abc123"

            [irc]
            uplink = "irc.example.net"
            link_name = "discord.example.net"
            link_password = "secret"

            [[bridge]]
            discord_channel_id = "123456789012345678"
            irc_channel = "#general"
        "##;
        assert!(toml::from_str::<Config>(toml).is_err());
    }

    #[test]
    fn missing_bridge_section_fails() {
        let toml = r##"
            [discord]
            token = "Bot abc123"

            [irc]
            uplink = "irc.example.net"
            link_name = "discord.example.net"
            link_password = "secret"
            sid = "0D0"
        "##;
        assert!(toml::from_str::<Config>(toml).is_err());
    }

    #[test]
    fn pseudoclients_section_optional() {
        // Omitting [pseudoclients] entirely should give defaults
        let cfg = parse(MINIMAL_TOML);
        assert_eq!(cfg.pseudoclients, PseudoclientConfig::default());
    }

    // -----------------------------------------------------------------------
    // Validation tests
    // -----------------------------------------------------------------------

    fn valid_config() -> Config {
        parse(MINIMAL_TOML)
    }

    #[test]
    fn valid_config_passes_validation() {
        assert!(valid_config().validate().is_ok());
    }

    // SID

    #[test]
    fn sid_valid_examples() {
        for sid in &["0D0", "0AA", "9ZZ", "1A2", "0A0"] {
            let mut cfg = valid_config();
            cfg.irc.sid = (*sid).to_string();
            assert!(cfg.validate().is_ok(), "expected {sid} to be valid");
        }
    }

    #[test]
    fn sid_invalid_examples() {
        for sid in &[
            "",     // empty
            "0",    // too short
            "0A",   // too short
            "0A0B", // too long
            "AA0",  // starts with letter
            "0a0",  // lowercase
            "0-0",  // invalid char
            "   ",  // spaces
        ] {
            let mut cfg = valid_config();
            cfg.irc.sid = (*sid).to_string();
            assert!(cfg.validate().is_err(), "expected {sid} to be invalid");
        }
    }

    // link_name

    #[test]
    fn link_name_valid_examples() {
        for name in &["discord.example.net", "irc.example.com", "a.b"] {
            let mut cfg = valid_config();
            cfg.irc.link_name = (*name).to_string();
            assert!(cfg.validate().is_ok(), "expected {name} to be valid");
        }
    }

    #[test]
    fn link_name_invalid_examples() {
        for name in &[
            "",           // empty
            "nodot",      // no dot — not a server name
            ".leading",   // leading dot
            "trailing.",  // trailing dot
            "-start.com", // label starts with hyphen
            "end-.com",   // label ends with hyphen
            "a..b",       // empty label
        ] {
            let mut cfg = valid_config();
            cfg.irc.link_name = (*name).to_string();
            assert!(cfg.validate().is_err(), "expected {name} to be invalid");
        }
    }

    // discord_channel_id

    #[test]
    fn discord_channel_id_valid() {
        let mut cfg = valid_config();
        cfg.bridges[0].discord_channel_id = "123456789012345678".to_string();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn discord_channel_id_invalid_examples() {
        for id in &["", "abc", "123abc", "12 34", "-1"] {
            let mut cfg = valid_config();
            cfg.bridges[0].discord_channel_id = (*id).to_string();
            assert!(cfg.validate().is_err(), "expected {id} to be invalid");
        }
    }

    // irc_channel

    #[test]
    fn irc_channel_valid() {
        for ch in &["#general", "#a", "##meta"] {
            let mut cfg = valid_config();
            cfg.bridges[0].irc_channel = (*ch).to_string();
            assert!(cfg.validate().is_ok(), "expected {ch} to be valid");
        }
    }

    #[test]
    fn irc_channel_invalid_examples() {
        for ch in &["", "general", "&general", " #general"] {
            let mut cfg = valid_config();
            cfg.bridges[0].irc_channel = (*ch).to_string();
            assert!(cfg.validate().is_err(), "expected {ch} to be invalid");
        }
    }

    // webhook_url

    #[test]
    fn webhook_url_valid_examples() {
        for url in &[
            "https://discord.com/api/webhooks/111/aaa",
            "https://discordapp.com/api/webhooks/222/bbb",
        ] {
            let mut cfg = valid_config();
            cfg.bridges[0].webhook_url = Some((*url).to_string());
            assert!(cfg.validate().is_ok(), "expected {url} to be valid");
        }
    }

    #[test]
    fn webhook_url_invalid_examples() {
        for url in &[
            "http://discord.com/api/webhooks/111/aaa", // http not https
            "https://evil.com/api/webhooks/111/aaa",   // wrong host
            "https://notdiscord.com/api/webhooks/1/a", // wrong host
            "discord.com/api/webhooks/111/aaa",        // no scheme
            "",                                        // empty
        ] {
            let mut cfg = valid_config();
            cfg.bridges[0].webhook_url = Some((*url).to_string());
            assert!(cfg.validate().is_err(), "expected {url} to be invalid");
        }
    }

    // duplicate detection

    #[test]
    fn duplicate_discord_channel_id_fails() {
        let mut cfg = parse(FULL_TOML);
        cfg.bridges[1].discord_channel_id = cfg.bridges[0].discord_channel_id.clone();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn duplicate_irc_channel_fails() {
        let mut cfg = parse(FULL_TOML);
        cfg.bridges[1].irc_channel = cfg.bridges[0].irc_channel.clone();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn at_least_one_bridge_required() {
        let mut cfg = valid_config();
        cfg.bridges.clear();
        assert!(cfg.validate().is_err());
    }

    // -----------------------------------------------------------------------
    // Reload diff tests
    // -----------------------------------------------------------------------

    fn bridge(discord_id: &str, irc: &str, webhook: Option<&str>) -> BridgeEntry {
        BridgeEntry {
            discord_channel_id: discord_id.to_string(),
            irc_channel: irc.to_string(),
            webhook_url: webhook.map(ToString::to_string),
        }
    }

    #[test]
    fn diff_identical_bridges_is_empty() {
        let bridges = vec![bridge("111", "#a", None)];
        let diff = diff_bridges(&bridges, &bridges);
        assert!(diff.is_empty());
    }

    #[test]
    fn diff_detects_added_entry() {
        let old = vec![bridge("111", "#a", None)];
        let new = vec![bridge("111", "#a", None), bridge("222", "#b", None)];
        let diff = diff_bridges(&old, &new);
        assert_eq!(diff.added.len(), 1);
        assert_eq!(diff.added[0].discord_channel_id, "222");
        assert!(diff.removed.is_empty());
        assert!(diff.webhook_changed.is_empty());
    }

    #[test]
    fn diff_detects_removed_entry() {
        let old = vec![bridge("111", "#a", None), bridge("222", "#b", None)];
        let new = vec![bridge("111", "#a", None)];
        let diff = diff_bridges(&old, &new);
        assert!(diff.added.is_empty());
        assert_eq!(diff.removed.len(), 1);
        assert_eq!(diff.removed[0].discord_channel_id, "222");
        assert!(diff.webhook_changed.is_empty());
    }

    #[test]
    fn diff_detects_webhook_change() {
        let old = vec![bridge("111", "#a", None)];
        let new = vec![bridge(
            "111",
            "#a",
            Some("https://discord.com/api/webhooks/1/x"),
        )];
        let diff = diff_bridges(&old, &new);
        assert!(diff.added.is_empty());
        assert!(diff.removed.is_empty());
        assert_eq!(diff.webhook_changed.len(), 1);
        assert_eq!(
            diff.webhook_changed[0].webhook_url.as_deref(),
            Some("https://discord.com/api/webhooks/1/x")
        );
    }

    #[test]
    fn diff_combined_add_remove_change() {
        let old = vec![
            bridge("111", "#a", None),
            bridge("222", "#b", Some("https://discord.com/api/webhooks/old/o")),
        ];
        let new = vec![
            bridge("222", "#b", Some("https://discord.com/api/webhooks/new/n")),
            bridge("333", "#c", None),
        ];
        let diff = diff_bridges(&old, &new);
        assert_eq!(diff.added.len(), 1);
        assert_eq!(diff.added[0].discord_channel_id, "333");
        assert_eq!(diff.removed.len(), 1);
        assert_eq!(diff.removed[0].discord_channel_id, "111");
        assert_eq!(diff.webhook_changed.len(), 1);
        assert_eq!(diff.webhook_changed[0].discord_channel_id, "222");
    }

    #[test]
    fn diff_empty_to_entries() {
        let diff = diff_bridges(&[], &[bridge("111", "#a", None)]);
        assert_eq!(diff.added.len(), 1);
        assert!(diff.removed.is_empty());
    }

    #[test]
    fn diff_entries_to_empty() {
        let diff = diff_bridges(&[bridge("111", "#a", None)], &[]);
        assert!(diff.added.is_empty());
        assert_eq!(diff.removed.len(), 1);
    }

    #[test]
    fn non_reloadable_changes_none_when_identical() {
        let cfg = valid_config();
        let cfg2 = valid_config();
        assert!(non_reloadable_changes(&cfg, &cfg2).is_empty());
    }

    #[test]
    fn non_reloadable_changes_detects_all_fields() {
        let cfg = valid_config();
        let mut cfg2 = parse(FULL_TOML);
        cfg2.discord.token = "different".to_string();
        cfg2.irc.uplink = "other.example.net".to_string();
        cfg2.irc.sid = "1AA".to_string();

        let changes = non_reloadable_changes(&cfg, &cfg2);
        assert!(changes.contains(&"discord.token"));
        assert!(changes.contains(&"irc.uplink"));
        assert!(changes.contains(&"irc.port"));
        assert!(changes.contains(&"irc.tls"));
        assert!(changes.contains(&"irc.description"));
        assert!(changes.contains(&"irc.sid"));
        assert!(changes.contains(&"pseudoclients.host_suffix"));
        assert!(changes.contains(&"pseudoclients.ident"));
    }

    #[test]
    fn non_reloadable_ignores_bridge_changes() {
        let cfg = valid_config();
        let mut cfg2 = valid_config();
        cfg2.bridges = vec![bridge("999", "#different", None)];
        // Bridge changes are reloadable, so they should NOT appear
        assert!(non_reloadable_changes(&cfg, &cfg2).is_empty());

        // But changing token should appear
        cfg2.discord.token = "new-token".to_string();
        let changes = non_reloadable_changes(&cfg, &cfg2);
        assert_eq!(changes, vec!["discord.token"]);
    }

    #[test]
    fn reload_returns_error_for_invalid_file() {
        let result = reload("nonexistent.toml", &valid_config());
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // Proptest
    // -----------------------------------------------------------------------

    use proptest::prelude::*;

    proptest! {
        #[test]
        fn valid_sid_always_passes(sid in "[0-9][A-Z0-9]{2}") {
            let mut cfg = valid_config();
            cfg.irc.sid = sid.clone();
            prop_assert!(cfg.validate().is_ok(), "sid {sid} should be valid");
        }

        #[test]
        fn wrong_length_sid_always_fails(s in "[0-9][A-Z0-9]{0,1}|[0-9][A-Z0-9]{3,10}") {
            let mut cfg = valid_config();
            cfg.irc.sid = s.clone();
            prop_assert!(cfg.validate().is_err(), "sid {s} of wrong length should fail");
        }

        #[test]
        fn nonempty_digit_string_is_valid_channel_id(s in "[0-9]{1,20}") {
            let mut cfg = valid_config();
            cfg.bridges[0].discord_channel_id = s.clone();
            prop_assert!(cfg.validate().is_ok(), "channel id {s} should be valid");
        }

        #[test]
        fn hash_prefixed_string_is_valid_irc_channel(rest in "[a-z][a-z0-9-]{0,29}") {
            let channel = format!("#{rest}");
            let mut cfg = valid_config();
            cfg.bridges[0].irc_channel = channel.clone();
            prop_assert!(cfg.validate().is_ok(), "irc channel {channel} should be valid");
        }

        #[test]
        fn string_without_hash_prefix_is_invalid_irc_channel(
            s in "[a-z][a-z0-9-]{0,29}",  // valid chars but no leading #
        ) {
            let mut cfg = valid_config();
            cfg.bridges[0].irc_channel = s.clone();
            prop_assert!(cfg.validate().is_err(), "irc channel {s} without # should be invalid");
        }
    }
}
