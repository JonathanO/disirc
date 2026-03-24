use disirc::config::{config_path_from_iter, load};
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

// ---------------------------------------------------------------------------
// load()
// ---------------------------------------------------------------------------

#[test]
fn load_valid_file_succeeds() {
    let cfg = load(fixture("valid.toml")).expect("valid fixture should load");
    assert_eq!(cfg.discord.token, "Bot abc123");
    assert_eq!(cfg.irc.sid, "0D0");
    assert_eq!(cfg.bridges.len(), 2);
}

#[test]
fn load_nonexistent_file_returns_io_error() {
    let result = load(fixture("does_not_exist.toml"));
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        matches!(err, disirc::config::ConfigError::Io(_)),
        "expected Io error, got {err:?}"
    );
}

#[test]
fn load_invalid_toml_returns_parse_error() {
    let result = load(fixture("invalid_toml.toml"));
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        matches!(err, disirc::config::ConfigError::Parse(_)),
        "expected Parse error, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// config_path_from_iter()
// ---------------------------------------------------------------------------

#[test]
fn config_path_defaults_to_config_toml() {
    let path = config_path_from_iter(["disirc"].iter().map(|s| s.to_string()));
    assert_eq!(path, PathBuf::from("config.toml"));
}

#[test]
fn config_path_explicit_flag() {
    let args = ["disirc", "--config", "/etc/disirc/prod.toml"]
        .iter()
        .map(|s| s.to_string());
    let path = config_path_from_iter(args);
    assert_eq!(path, PathBuf::from("/etc/disirc/prod.toml"));
}

#[test]
fn config_path_ignores_unknown_flags() {
    let args = ["disirc", "--verbose", "--config", "custom.toml"]
        .iter()
        .map(|s| s.to_string());
    let path = config_path_from_iter(args);
    assert_eq!(path, PathBuf::from("custom.toml"));
}

#[test]
fn config_path_flag_without_value_defaults() {
    // --config with no following argument falls back to default
    let args = ["disirc", "--config"].iter().map(|s| s.to_string());
    let path = config_path_from_iter(args);
    assert_eq!(path, PathBuf::from("config.toml"));
}
