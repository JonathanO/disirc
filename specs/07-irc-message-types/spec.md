# IRC Message Types

## Purpose

This module defines the Rust types used to represent IRC messages throughout
`disirc`. Rather than passing raw protocol strings between modules, all code
that produces or consumes IRC messages uses these types. A single serializer
turns an `IrcMessage` into a wire-format line; a single parser turns a
wire-format line into an `IrcMessage`.

This design:
- Gives named fields to multi-parameter commands such as `UID` (12 fields)
- Keeps the wire format in one place — only the serializer/parser touches `\r\n`, `:`, and tag escaping
- Allows other modules (pseudoclients, connection) to be tested without string matching

## Wire format

Lines follow the IRCv3 message grammar:

```
line       = [tags SP] [":" prefix SP] command *(SP param) [SP ":" trailing] CR LF
tags       = "@" tag *(";" tag)
tag        = key ["=" escaped-value]
prefix     = servername / uid
command    = 1*ALPHA / 3DIGIT
param      = *( %x01-09 / %x0B-0C / %x0E-1F / %x21-FF )  ; no NUL, CR, LF, SPACE
trailing   = *( %x01-09 / %x0B-0C / %x0E-FF )             ; no NUL, CR, LF
```

Maximum line length: 512 bytes without tags; 4096 bytes when `MTAGS` is active.
Lines exceeding the limit are truncated at the last valid UTF-8 boundary before
the limit, before the final `\r\n`.

## `IrcMessage`

```rust
pub struct IrcMessage {
    /// IRCv3 message tags (key, optional value). Order is preserved.
    pub tags:    Vec<(String, Option<String>)>,
    /// Source prefix (server name or UID), without the leading `:`.
    pub prefix:  Option<String>,
    /// The typed command and its parameters.
    pub command: IrcCommand,
}
```

## `IrcCommand`

Commands are split into two groups: **known** commands (typed variants with
named fields) and **unknown** commands (raw fallback).

```rust
pub enum IrcCommand {
    // ---- Authentication / handshake ----
    Pass  { password: String },
    Server { name: String, hop_count: u32, description: String },
    Sid   { name: String, hop_count: u32, sid: String, description: String },

    // ---- Capability negotiation ----
    Protoctl { tokens: Vec<String> },

    // ---- User introduction ----
    Uid(UidParams),

    // ---- Channel membership ----
    Sjoin(SjoinParams),
    Part  { channel: String, reason: Option<String> },
    Kick  { channel: String, target: String, reason: Option<String> },

    // ---- Nick / presence ----
    Nick  { new_nick: String, timestamp: u64 },
    Quit  { reason: String },
    Away  { reason: Option<String> },    // None = unset away
    Svsnick { target_uid: String, new_nick: String },

    // ---- Messaging ----
    Privmsg { target: String, text: String },
    Notice  { target: String, text: String },

    // ---- Keepalive ----
    Ping { token: String },
    Pong { server: String, token: String },

    // ---- End of burst ----
    Eos,

    // ---- Error ----
    Error { message: String },

    // ---- Fallback ----
    /// Any command not listed above. Preserved for logging and pass-through.
    Raw { command: String, params: Vec<String> },
}
```

## `UidParams`

Corresponds to the 12-field `UID` command used in the UnrealIRCd S2S burst.

| Field | Position | Notes |
|-------|----------|-------|
| `nick` | 1 | IRC nick |
| `hop_count` | 2 | Always `1` for direct pseudoclients |
| `timestamp` | 3 | Unix timestamp of user introduction |
| `ident` | 4 | Ident / username |
| `host` | 5 | Displayed hostname |
| `uid` | 6 | 9-char UID (`<SID>` + 6 alphanumeric) |
| `services_stamp` | 7 | `0` for non-services users |
| `umodes` | 8 | User mode string e.g. `+i` |
| `vhost` | 9 | Virtual host; `*` if none |
| `cloaked_host` | 10 | Cloaked host; `*` if not cloaked |
| `ip` | 11 | IP address; `*` for services/pseudoclients |
| `realname` | 12 | GECOS / display name |

## `SjoinParams`

Corresponds to the `SJOIN` command used to introduce users to channels.

| Field | Notes |
|-------|-------|
| `timestamp` | Channel creation timestamp |
| `channel` | Channel name including `#` |
| `modes` | Channel mode string (may be empty `+`) |
| `members` | List of UID strings, optionally prefixed with mode chars (`@`, `+`, etc.) |

## Serialization

`IrcMessage` implements `Display` (and a `to_wire()` convenience method) that
produces a complete wire-format line including `\r\n`. Rules:

- Tags are serialized first: `@key=value;key2 ` — values are escaped per IRCv3
  tag escaping (`;` → `\:`, space → `\s`, `\` → `\\`, CR → `\r`, LF → `\n`).
- The prefix is serialized as `:<prefix> ` if present.
- The final parameter is serialized with a `:` prefix (the trailing form) so
  that it may contain spaces. All other parameters must not contain spaces or
  be empty — if they do, serialization returns an error.
- The `UidParams` realname and `SjoinParams` members list are always the
  trailing parameter.
- The whole line (excluding `\r\n`) must not exceed 4096 bytes. If it does,
  the trailing parameter is truncated to fit.

## Parsing

`IrcMessage::parse(line: &str) -> Result<IrcMessage, ParseError>` parses a
single wire-format line (with or without the trailing `\r\n`). Rules:

- Tags are parsed first if the line begins with `@`.
- The prefix is parsed if the next token begins with `:`.
- The command is the next whitespace-delimited token.
- Parameters are split on whitespace; the final parameter may begin with `:`
  (trailing form) in which case it consumes the rest of the line including spaces.
- Unknown commands produce `IrcCommand::Raw`.
- Parsing is infallible for well-formed lines; malformed lines that cannot be
  split into at least a command return `ParseError::Empty`.

## Tag handling

Tags are preserved on `IrcMessage` as received. The connection layer (see
`specs/02-irc-connection.md`) is responsible for filtering sensitive tags
(`s2s-md/*`, `@unrealircd.org/userhost`) before passing messages to other
modules. Message types do not filter tags.

## Error types

```rust
pub enum ParseError {
    /// Line is empty or contains only whitespace.
    Empty,
    /// A `UID` command did not have exactly 12 parameters.
    UidParamCount { got: usize },
    /// A `SJOIN` command did not have at least 4 parameters.
    SjoinParamCount { got: usize },
}

pub enum SerializeError {
    /// A non-trailing parameter contained a space or was empty.
    InvalidParam { index: usize, value: String },
}
```

## Relationship to other specs

- **`specs/06-pseudoclients.md`**: `PseudoclientManager::introduce()`,
  `quit()`, `join_channel()`, and `part_channel()` return `IrcMessage`/
  `Vec<IrcMessage>` instead of `String`/`Vec<String>`.
- **`specs/02-irc-connection.md`**: The connection layer parses incoming lines
  into `IrcMessage` and serializes outgoing `IrcMessage` values to the socket.
  It also applies tag filtering before dispatching parsed messages.

## Tasks

- [ ] Define `IrcCommand`, `UidParams`, `SjoinParams` types in `src/irc_message.rs`
- [ ] Implement `IrcMessage::parse()` with `ParseError`
- [ ] Implement `IrcMessage` serialization (`Display` / `to_wire()`) with `SerializeError`
- [ ] Update `src/pseudoclients.rs` to return `IrcMessage`/`Vec<IrcMessage>`
- [ ] Update `SPECS.md`

## References

- [research/unreal-ircd-s2s-protocol.md](../../research/unreal-ircd-s2s-protocol.md) — UID/SJOIN field layout, PROTOCTL tokens
- [research/unrealircd-ircv3-s2s.md](../../research/unrealircd-ircv3-s2s.md) — tag propagation rules, MTAGS gate
- [IRCv3 message tags specification](https://ircv3.net/specs/extensions/message-tags) — tag grammar and escaping — accessed 2026-03-24
- [UnrealIRCd Server Protocol — UID command](https://www.unrealircd.org/docs/Server_protocol:UID_command) — accessed 2026-03-22
- [UnrealIRCd Server Protocol — SJOIN command](https://www.unrealircd.org/docs/Server_protocol:SJOIN_command) — accessed 2026-03-22
