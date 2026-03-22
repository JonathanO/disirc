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
}
