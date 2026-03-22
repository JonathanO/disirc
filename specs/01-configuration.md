# Configuration

## Format

Configuration is a single TOML file, passed to `disirc` via `--config <path>` (default: `config.toml` in the working directory).

## Example

```toml
[discord]
token = "Bot <your-discord-bot-token>"

[irc]
# The UnrealIRCd server to link to
uplink         = "irc.example.net"
port           = 6900
tls            = true
# Link credentials — must match a link{} block in unrealircd.conf
link_name      = "discord.example.net"   # our server name as seen by the network
link_password  = "secret"
# Our server identity on the IRC network
sid            = "0D0"                   # unique 3-char alphanumeric SID
description    = "Discord bridge"

[pseudoclients]
# Hostname suffix for Discord users on IRC
# Alice on Discord appears as Alice!discord@alice.users.discord.example.net
host_suffix    = "users.discord.example.net"
# Ident field for all pseudoclients
ident          = "discord"

[[bridge]]
discord_channel_id = "123456789012345678"
irc_channel        = "#general"
webhook_url        = "https://discord.com/api/webhooks/111/aaabbb"   # optional but preferred

[[bridge]]
discord_channel_id = "987654321098765432"
irc_channel        = "#dev"
# no webhook_url — falls back to plain channel.send()
```

## Required fields

| Field | Description |
|-------|-------------|
| `discord.token` | Discord bot token |
| `irc.uplink` | Hostname or IP of the UnrealIRCd server to link to |
| `irc.link_name` | Our server name as configured in UnrealIRCd's `link{}` block |
| `irc.link_password` | Link password matching UnrealIRCd's `link{}` block |
| `irc.sid` | Our Server ID — 3 characters matching `[0-9][A-Z0-9]{2}`, unique on the network |
| `bridge[].discord_channel_id` | Discord channel snowflake ID (string of digits) |
| `bridge[].irc_channel` | IRC channel name (must start with `#`) |

## Optional fields

| Field | Default | Description |
|-------|---------|-------------|
| `irc.port` | `6900` | Port to connect to on the uplink |
| `irc.tls` | `true` | Use TLS for the server link |
| `irc.description` | `"Discord bridge"` | Server description shown in `/links` and `/map` |
| `pseudoclients.host_suffix` | `"discord"` | Hostname suffix for pseudoclient hostmasks |
| `pseudoclients.ident` | `"discord"` | Ident (username) field for all pseudoclients |
| `bridge[].webhook_url` | _(none)_ | Discord webhook URL for this channel. When set, IRC messages are delivered via webhook so each IRC user appears with their own nick and avatar. Falls back to plain `channel.send()` if absent or on failure. |

## Validation rules

- `irc.sid` must match `[0-9][A-Z0-9]{2}`.
- `irc.link_name` must be a valid IRC server name (hostname format).
- `bridge[].discord_channel_id` must be a non-empty string of ASCII digits.
- `bridge[].irc_channel` must start with `#`.
- `bridge[].webhook_url` if present must be a valid HTTPS URL with host `discord.com` or `discordapp.com`.
- At least one `[[bridge]]` entry must be present.
- Duplicate `discord_channel_id` or `irc_channel` values across bridge entries are an error.

## Error behaviour

On startup, `disirc` validates the config fully and exits with a descriptive error message if any required field is missing or any validation rule is violated. No partial startup.

## Runtime config reload

`disirc` supports reloading a subset of the config without restarting, triggered by `SIGHUP` (Unix). On Windows, `SIGHUP` is not available; reload is not supported on that platform in v1.

### Reloadable fields

Only `[[bridge]]` entries and their `webhook_url` values may change at runtime:

| Change | Action |
|--------|--------|
| New `[[bridge]]` entry added | Fetch Discord members for the new channel, SJOIN existing pseudoclients to the new IRC channel, introduce any new pseudoclients, begin relaying |
| `[[bridge]]` entry removed | Pseudoclients only in that channel receive `QUIT`; pseudoclients in other bridged channels `PART` the removed IRC channel; stop relaying |
| `bridge[].webhook_url` changed | Update in memory; no IRC action required |

### Non-reloadable fields

All other fields require a full restart. Changing them at runtime is ignored with a `WARN` log:

- `discord.token`
- All `[irc]` fields (uplink, port, tls, link_name, link_password, sid, description)
- All `[pseudoclients]` fields (host_suffix, ident)

### Reload procedure

1. Re-read and fully validate the new config file. If validation fails, log at `ERROR`, discard the new config, and continue with the existing config unchanged.
2. Compute the diff between the old and new `[[bridge]]` entries.
3. Apply additions and removals as described above.
4. Log at `INFO` summarising what changed (channels added, channels removed, webhooks updated).

### Reload error handling

- If the config file cannot be read (e.g. permissions error), log at `ERROR` and continue with existing config.
- If the new config is invalid, log at `ERROR` with the validation error and continue with existing config.
- A failed reload never affects the running bridge.

## Secrets

`config.toml` contains credentials and must never be committed. Provide `config.example.toml` with placeholder values for documentation.
