# TODO — spec/01-configuration

Status: **Implemented**

- [x] Define config structs with serde (`DiscordConfig`, `IrcConfig`, `PseudoclientConfig`, `BridgeEntry`, root `Config`)
- [x] Config loading from file (read TOML, deserialize, CLI `--config` flag)
- [x] Validation logic (SID regex, channel names, webhook URL, duplicate detection, at-least-one-bridge)
- [x] Tests: unit tests + proptest for all validation rules
- [x] SIGHUP handler (tokio signal, send reload event into mpsc channel)
- [x] Reload diff logic (compute added/removed entries, apply, log summary; validate before applying)
