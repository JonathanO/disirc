# IRC oper commands

## Goal

Allow IRC operators to manage the bridge at runtime by sending commands to
the bridge bot pseudoclient via `/msg`.  This avoids editing `config.toml`
and sending SIGHUP for routine operational tasks like adding or removing
bridge channel pairs.

## Overview

The bridge bot pseudoclient (the first user introduced from `MemberSnapshot`,
typically the Discord bot itself) acts as a command interface.  IRC opers
send it private messages; the bot parses the command, validates the sender's
oper status, executes the action, and replies with a confirmation or error.

Non-opers who message the bot receive a DM via the existing DM bridging
path (if enabled).  The command handler only intercepts messages from users
with oper status.

## Oper detection

### From the burst (UID command)

The UnrealIRCd `UID` command includes a `umodes` field (parameter 8) which
contains the user's mode string.  Oper status is indicated by the `+o` flag
(e.g. `"+io"` for invisible + oper).

We already parse `umodes` into `UidParams.umodes` but discard it during
translation to `S2SEvent::UserIntroduced`.  To detect opers:

1. Add a `is_oper: bool` field to `S2SEvent::UserIntroduced`.
2. Set it to `true` if `umodes` contains `'o'`.
3. Track oper UIDs in `IrcState`.

### From MODE changes

After the burst, oper status can change via MODE commands (e.g.
`:uid MODE uid +o`).  We currently drop MODE as `IrcCommand::Raw`.

1. Add `IrcCommand::UserMode { target: String, modes: String }` for
   user MODE changes (as distinct from channel MODE).
2. Translate inbound user MODE to a new `S2SEvent::UserModeChanged {
   uid: String, modes: String }`.
3. In `apply_irc_event`, update `IrcState` oper tracking when `+o` or
   `-o` appears in the mode string.

### Oper tracking in IrcState

Add a `HashSet<String>` of oper UIDs to `IrcState`.  Updated by:

- `UserIntroduced` with `is_oper: true` → insert
- `UserModeChanged` with `+o` → insert, `-o` → remove
- `UserQuit` / `UserKilled` → remove

On `LinkDown`, the set is cleared (rebuilt from the next burst).

## Command routing

Currently, PRIVMSGs to our pseudoclient UIDs are routed to Discord DMs
via `route_irc_to_dm`.  The command handler intercepts messages to the
**bot** pseudoclient before DM routing:

```
PRIVMSG <bot_uid> :bridge add #irc-channel 123456789012345678
```

In `handle_irc_event`, when a `MessageReceived` targets the bot UID:

1. Check if `from_uid` is in the oper set.
2. If yes, parse the message as a command.  On success or error, reply
   via `S2SCommand::SendMessage` from the bot UID back to the sender.
3. If no, fall through to the existing DM relay path.

## Commands

All commands are prefixed with a verb.  Arguments are space-separated.
Unknown commands return an error reply.

### `bridge list`

List all active bridge channel pairs.

```
← bridge list
→ #general ↔ 123456789012345678 (webhook)
→ #dev ↔ 987654321098765432
```

### `bridge add <irc_channel> <discord_channel_id> [webhook_url]`

Add a new bridge channel pair.  The IRC channel must start with `#`.
The Discord channel ID must be a numeric snowflake.  The webhook URL is
optional.

Validates:
- IRC channel name starts with `#`.
- Discord channel ID is a valid u64.
- No existing bridge for this IRC channel or Discord channel ID.
- If webhook URL provided, it looks like a Discord webhook URL.

On success, updates `config.bridges`, calls `reload_config`, and writes
the updated config to disk.

```
← bridge add #new-channel 111222333444555666
→ Bridge added: #new-channel ↔ 111222333444555666
```

### `bridge remove <irc_channel>`

Remove a bridge channel pair by IRC channel name.

On success, parts all pseudoclients from the channel (or quits those with
no remaining channels), updates config, and writes to disk.

```
← bridge remove #old-channel
→ Bridge removed: #old-channel
```

### `bridge set-webhook <irc_channel> <webhook_url>`

Set or update the webhook URL for an existing bridge.

```
← bridge set-webhook #general https://discord.com/api/webhooks/...
→ Webhook updated for #general
```

### `bridge clear-webhook <irc_channel>`

Remove the webhook URL for an existing bridge (fall back to plain send).

```
← bridge clear-webhook #general
→ Webhook cleared for #general
```

### `status`

Show bridge status summary.

```
← status
→ IRC: connected (uptime 2d 5h)
→ Pseudoclients: 42 active, 3 idle
→ Bridges: 5 configured
```

### `reload`

Trigger a config reload from disk (equivalent to SIGHUP).

```
← reload
→ Config reloaded
```

## Config persistence

When a command modifies bridge configuration, the changes must be written
back to the config file so they survive a restart.  This requires:

1. A `save_config(path, &Config)` function in `config.rs` that serializes
   the config back to TOML and writes it atomically (same temp+rename
   pattern as state persistence).
2. The bridge loop passes `config_path` to the command handler.

### Preserving comments and formatting

TOML serialization via `toml::to_string` will not preserve comments or
manual formatting from the original file.  This is an acceptable trade-off
for v1 — the config file is machine-managed once oper commands modify it.
Document this in the config example.

Alternatively, a future version could use `toml_edit` to preserve
formatting, but that adds a dependency for a cosmetic concern.

## Error handling

- Unknown command → reply with usage help.
- Invalid arguments → reply with specific error (e.g. "invalid Discord
  channel ID: not a number").
- Config write failure → reply with error, keep in-memory changes
  (they'll be lost on restart, but the bridge continues operating).
- Duplicate bridge → reply "bridge already exists for #channel".

## Security

- **Oper-only**: all commands require `+o` (IRC oper) status.  Regular
  users messaging the bot get DM relay, not command access.
- **Webhook URLs are secrets**: `bridge list` should NOT display webhook
  URLs.  Only show "(webhook)" to indicate one is configured.
- **No token exposure**: `status` must not reveal the Discord token, link
  password, or any other secret.

## Scope boundaries

This spec covers:
- Oper detection (UID umodes + MODE tracking)
- Command parsing and dispatch for the bot pseudoclient
- Bridge add/remove/modify commands
- Config file write-back

This spec does NOT cover:
- Discord-side bot commands (slash commands, message commands)
- Non-bridge config changes via oper commands (ident, timeouts, etc.)
  — these could be added later as additional commands
- Channel operator (chanop) commands — only IRC opers (server operators)
  are authorized

## Implementation order

1. **Oper detection**: expose `umodes` in `UserIntroduced`, parse MODE,
   track opers in `IrcState`.  Small, self-contained, prerequisite for
   everything else.
2. **Command handler skeleton**: intercept bot PRIVMSGs from opers,
   parse commands, reply.  Start with `status` and `reload` (read-only,
   no config mutation).
3. **Bridge commands**: `bridge list`, `bridge add`, `bridge remove`.
   Requires config write-back.
4. **Webhook commands**: `bridge set-webhook`, `bridge clear-webhook`.
