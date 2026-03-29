//! Typed IRC message representation.
//!
//! All code that produces or consumes IRC messages uses these types rather than
//! raw protocol strings.  A single serializer turns an [`IrcMessage`] into a
//! wire-format line; a single parser turns a wire-format line into an
//! [`IrcMessage`].

#![allow(clippy::module_name_repetitions)]

use thiserror::Error;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A single IRC message with optional `IRCv3` tags, an optional source prefix,
/// and a typed command.
#[derive(Debug, Clone, PartialEq)]
pub struct IrcMessage {
    /// `IRCv3` message tags (key, optional value).  Order is preserved.
    pub tags: Vec<(String, Option<String>)>,
    /// Source prefix (server name or UID), without the leading `:`.
    pub prefix: Option<String>,
    /// The typed command and its parameters.
    pub command: IrcCommand,
}

/// Parameters for the `UID` command (user introduction burst).
///
/// Corresponds to the 12-field `UID` command in the `UnrealIRCd` S2S protocol.
/// Field order matches the wire format exactly.
#[derive(Debug, Clone, PartialEq)]
pub struct UidParams {
    /// IRC nickname.
    pub nick: String,
    /// Hop count from the introducing server.  Always `1` for pseudoclients
    /// we introduce ourselves.
    pub hop_count: u32,
    /// UNIX timestamp at which the user was introduced (seconds).
    pub timestamp: u64,
    /// Ident / username (the `~user` part of a hostmask).
    pub ident: String,
    /// Real hostname of the user.
    pub host: String,
    /// Globally unique user ID (SID prefix + 6 alphanumeric chars).
    pub uid: String,
    /// Services account stamp.  `"0"` means the user is not logged in to
    /// any services account.
    pub services_stamp: String,
    /// User mode string (e.g. `"+i"`).
    pub umodes: String,
    /// Displayed virtual hostname.  `"*"` means no vhost is set and the
    /// real hostname is shown.
    pub vhost: String,
    /// Cloaked hostname used for host-hiding.  `"*"` if not set.
    pub cloaked_host: String,
    /// Real IP address, or `"*"` if hidden / not applicable.
    pub ip: String,
    /// GECOS / realname field (the trailing parameter on the wire).
    pub realname: String,
}

/// Parameters for the `SJOIN` command (channel burst).
#[derive(Debug, Clone, PartialEq)]
pub struct SjoinParams {
    /// Channel creation / burst timestamp (seconds).
    pub timestamp: u64,
    /// Channel name, including the `#` sigil.
    pub channel: String,
    /// Channel mode string at burst time (e.g. `"+"` for no modes, `"+nt"`).
    /// Does not include mode parameters; those are not preserved by this type.
    pub modes: String,
    /// UIDs of members being introduced into the channel.  Each entry may be
    /// prefixed with one or more status-mode characters (`@` for op, `+` for
    /// voice, etc.).  Empty string means the channel is being created with no
    /// initial members (mode-only SJOIN).
    pub members: Vec<String>,
}

/// Typed IRC command variants used within `disirc`.
#[derive(Debug, Clone, PartialEq)]
pub enum IrcCommand {
    // ---- Authentication / handshake ----
    /// `PASS` — link password, sent before `SERVER`/`SID` during handshake.
    Pass {
        /// The link password.
        password: String,
    },
    /// `SERVER` — server announcement (used by leaf servers without a SID).
    Server {
        /// Fully-qualified server name.
        name: String,
        /// Number of hops from the origin.
        hop_count: u32,
        /// Human-readable server description.
        description: String,
    },
    /// `SID` — server announcement with a numeric SID (`UnrealIRCd` TS6).
    Sid {
        /// Fully-qualified server name.
        name: String,
        /// Number of hops from the origin.
        hop_count: u32,
        /// Three-character numeric server ID.
        sid: String,
        /// Human-readable server description.
        description: String,
    },
    // ---- Capability negotiation ----
    /// `PROTOCTL` — capability tokens exchanged during the S2S handshake.
    Protoctl {
        /// Space-separated capability tokens (e.g. `NOQUIT`, `EAUTH=…`, `SID`).
        tokens: Vec<String>,
    },
    // ---- User introduction ----
    /// `UID` — introduce a user to the network.
    Uid(UidParams),
    // ---- Channel membership ----
    /// `SJOIN` — burst channel state (members + modes).
    Sjoin(SjoinParams),
    /// `PART` — leave a channel.
    Part {
        /// Channel name.
        channel: String,
        /// Optional part message.
        reason: Option<String>,
    },
    /// `KICK` — forcibly remove a user from a channel.
    Kick {
        /// Channel the kick applies to.
        channel: String,
        /// Nick or UID of the user being kicked.
        target: String,
        /// Optional kick message.
        reason: Option<String>,
    },
    // ---- Nick / presence ----
    /// `NICK` — nick change for an existing user.
    Nick {
        /// The new nick.
        new_nick: String,
        /// Timestamp of the nick change (seconds).
        timestamp: u64,
    },
    /// `QUIT` — user disconnected.
    Quit {
        /// Quit message.
        reason: String,
    },
    /// `AWAY` — set or unset away status.
    Away {
        /// Away message, or `None` to unset away.
        reason: Option<String>,
    },
    /// `SVSNICK` — services-forced nick change.
    Svsnick {
        /// UID of the user whose nick is being changed.
        target_uid: String,
        /// The new nick being forced.
        new_nick: String,
    },
    // ---- Messaging ----
    /// `PRIVMSG` — message to a channel or user.
    Privmsg {
        /// Channel name or target nick / UID.
        target: String,
        /// Message body (the trailing parameter).
        text: String,
    },
    /// `NOTICE` — notice to a channel or user.
    Notice {
        /// Channel name or target nick / UID.
        target: String,
        /// Notice body (the trailing parameter).
        text: String,
    },
    // ---- Keepalive ----
    /// `PING` — keepalive request.
    Ping {
        /// Token the peer should echo back in its `PONG`.
        token: String,
    },
    /// `PONG` — keepalive reply.
    Pong {
        /// Name of the server sending the pong.
        server: String,
        /// Token echoed from the corresponding `PING`.
        token: String,
    },
    // ---- End of burst ----
    /// `EOS` — signals the end of the server burst.
    Eos,
    // ---- Error ----
    /// `ERROR` — fatal error from the remote server; connection will be closed.
    Error {
        /// Human-readable error message.
        message: String,
    },
    // ---- Fallback ----
    /// Any command not listed above.  Preserved for logging and pass-through.
    Raw {
        /// The command name as it appeared on the wire.
        command: String,
        /// Parameters in order.  The last parameter is the trailing one (its
        /// leading `:` has been stripped by the parser).
        params: Vec<String>,
    },
}

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

/// Error returned by [`IrcMessage::parse`].
#[derive(Debug, Clone, PartialEq, Error)]
pub enum ParseError {
    /// Line is empty or contains only whitespace / CRLF.
    #[error("line is empty")]
    Empty,
    /// A known command did not have enough parameters to be fully parsed.
    #[error("{command} requires at least {required} parameter(s), got {got}")]
    MissingParams {
        command: String,
        required: usize,
        got: usize,
    },
}

/// Error returned by [`IrcMessage::to_wire`].
#[derive(Debug, Clone, PartialEq, Error)]
pub enum SerializeError {
    /// A non-trailing parameter was empty or contained a space.
    #[error("non-trailing parameter {index} {value:?} is invalid (empty or contains space)")]
    InvalidParam { index: usize, value: String },
    /// The serialized line exceeded 4096 bytes (excluding `\r\n`).
    #[error("serialized line is {len} bytes, exceeds 4096-byte limit")]
    LineTooLong { len: usize },
}

// ---------------------------------------------------------------------------
// IrcMessage impl
// ---------------------------------------------------------------------------

impl IrcMessage {
    /// Parse a single IRC wire-format line into an [`IrcMessage`].
    ///
    /// The trailing `\r\n` is stripped before parsing if present.
    ///
    /// # Errors
    ///
    /// - [`ParseError::Empty`] if the line is empty or whitespace-only.
    /// - [`ParseError::MissingParams`] if a known command has fewer parameters than required.
    pub fn parse(line: &str) -> Result<Self, ParseError> {
        let line = line.trim_end_matches('\n').trim_end_matches('\r');
        if line.trim_start().is_empty() {
            return Err(ParseError::Empty);
        }

        let mut rest = line;

        // Tags: "@key=val;key2 ..."
        let tags = if rest.starts_with('@') {
            let (tag_str, remainder) = rest[1..].split_once(' ').unwrap_or((&rest[1..], ""));
            rest = remainder;
            parse_tags(tag_str)
        } else {
            Vec::new()
        };

        rest = rest.trim_start();

        // Prefix: ":server ..."
        let prefix = if rest.starts_with(':') {
            let (pfx, remainder) = rest[1..].split_once(' ').unwrap_or((&rest[1..], ""));
            rest = remainder;
            Some(pfx.to_string())
        } else {
            None
        };

        rest = rest.trim_start();
        if rest.is_empty() {
            return Err(ParseError::Empty);
        }

        // Command token and remaining params
        let (cmd_str, params_str) = rest.split_once(' ').unwrap_or((rest, ""));
        let params = parse_params(params_str.trim_start());
        let command = build_command(cmd_str, params)?;

        Ok(IrcMessage {
            tags,
            prefix,
            command,
        })
    }

    /// Serialize this message to a wire-format line including the trailing `\r\n`.
    ///
    /// # Errors
    ///
    /// - [`SerializeError::InvalidParam`] if a non-trailing parameter is empty or
    ///   contains a space.
    /// - [`SerializeError::LineTooLong`] if the line (excluding `\r\n`) exceeds
    ///   4096 bytes.
    pub fn to_wire(&self) -> Result<String, SerializeError> {
        let mut out = String::new();

        // Tags
        if !self.tags.is_empty() {
            out.push('@');
            for (i, (key, value)) in self.tags.iter().enumerate() {
                if i > 0 {
                    out.push(';');
                }
                out.push_str(key);
                if let Some(v) = value {
                    out.push('=');
                    out.push_str(&escape_tag_value(v));
                }
            }
            out.push(' ');
        }

        // Prefix
        if let Some(prefix) = &self.prefix {
            out.push(':');
            out.push_str(prefix);
            out.push(' ');
        }

        // Command and parameters
        write_command(&self.command, &mut out)?;

        // Length check (before \r\n)
        let len = out.len();
        if len > 4096 {
            return Err(SerializeError::LineTooLong { len });
        }

        out.push_str("\r\n");
        Ok(out)
    }
}

/// Renders the message as a complete wire-format line (including `\r\n`).
///
/// # Panics
///
/// Panics if the message cannot be serialized (e.g., a non-trailing parameter
/// contains a space or the line exceeds 4096 bytes).  Prefer
/// [`IrcMessage::to_wire`] in code paths where serialization errors must be
/// handled gracefully.
impl std::fmt::Display for IrcMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let wire = self
            .to_wire()
            .expect("IrcMessage::fmt called on an unserializable message");
        f.write_str(&wire)
    }
}

// ---------------------------------------------------------------------------
// Private helpers — parsing
// ---------------------------------------------------------------------------

fn parse_tags(tag_str: &str) -> Vec<(String, Option<String>)> {
    tag_str
        .split(';')
        .filter(|s| !s.is_empty())
        .map(|tag| {
            if let Some((key, val)) = tag.split_once('=') {
                (key.to_string(), Some(unescape_tag_value(val)))
            } else {
                (tag.to_string(), None)
            }
        })
        .collect()
}

fn unescape_tag_value(val: &str) -> String {
    let mut result = String::with_capacity(val.len());
    let mut chars = val.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some(':') => result.push(';'),
                Some('s') => result.push(' '),
                Some('\\') => result.push('\\'),
                Some('r') => result.push('\r'),
                Some('n') => result.push('\n'),
                Some(c) => result.push(c), // unrecognised escape: drop backslash
                None => {}                 // trailing backslash: drop
            }
        } else {
            result.push(ch);
        }
    }
    result
}

fn parse_params(s: &str) -> Vec<String> {
    if s.is_empty() {
        return Vec::new();
    }
    let mut params = Vec::new();
    let mut rest = s;
    while !rest.is_empty() {
        if let Some(trailing) = rest.strip_prefix(':') {
            params.push(trailing.to_string());
            break;
        }
        let (param, remainder) = rest.split_once(' ').unwrap_or((rest, ""));
        if !param.is_empty() {
            params.push(param.to_string());
        }
        rest = remainder.trim_start();
    }
    params
}

/// Returns `Err(ParseError::MissingParams)` when `params` has fewer than `required` entries.
fn require_params(command: &str, params: &[String], required: usize) -> Result<(), ParseError> {
    if params.len() < required {
        Err(ParseError::MissingParams {
            command: command.to_string(),
            required,
            got: params.len(),
        })
    } else {
        Ok(())
    }
}

/// Emits a `WARN`-level log when `params` has more entries than `expected`.
///
/// Extra parameters are discarded; this is the forward-compatibility policy for
/// protocol extensions that insert new non-trailing fields before the trailing one.
fn warn_extra_params(command: &str, params: &[String], expected: usize) {
    let got = params.len();
    if got > expected {
        tracing::warn!(
            command,
            expected,
            got,
            "ignoring {} unexpected extra parameter(s)",
            got - expected
        );
    }
}

fn build_command(name: &str, params: Vec<String>) -> Result<IrcCommand, ParseError> {
    let upper = name.to_ascii_uppercase();
    match upper.as_str() {
        "PASS" => {
            require_params("PASS", &params, 1)?;
            warn_extra_params("PASS", &params, 1);
            Ok(IrcCommand::Pass {
                password: params.last().unwrap().clone(),
            })
        }
        "SERVER" => {
            require_params("SERVER", &params, 3)?;
            warn_extra_params("SERVER", &params, 3);
            Ok(IrcCommand::Server {
                name: params[0].clone(),
                hop_count: params[1].parse().unwrap_or(0),
                description: params.last().unwrap().clone(),
            })
        }
        "SID" => {
            require_params("SID", &params, 4)?;
            warn_extra_params("SID", &params, 4);
            Ok(IrcCommand::Sid {
                name: params[0].clone(),
                hop_count: params[1].parse().unwrap_or(0),
                sid: params[2].clone(),
                description: params.last().unwrap().clone(),
            })
        }
        "PROTOCTL" => Ok(IrcCommand::Protoctl { tokens: params }),
        "UID" => {
            require_params("UID", &params, 12)?;
            warn_extra_params("UID", &params, 12);
            Ok(IrcCommand::Uid(UidParams {
                nick: params[0].clone(),
                hop_count: params[1].parse().unwrap_or(0),
                timestamp: params[2].parse().unwrap_or(0),
                ident: params[3].clone(),
                host: params[4].clone(),
                uid: params[5].clone(),
                services_stamp: params[6].clone(),
                umodes: params[7].clone(),
                vhost: params[8].clone(),
                cloaked_host: params[9].clone(),
                ip: params[10].clone(),
                realname: params.last().unwrap().clone(),
            }))
        }
        "SJOIN" => {
            require_params("SJOIN", &params, 4)?;
            // No warn_extra_params: extra params between modes and the member list are
            // valid mode parameters (e.g. "+l 10 :@UID"), so excess is intentional.
            let members: Vec<String> = params
                .last()
                .unwrap()
                .split_whitespace()
                .map(str::to_string)
                .collect();
            Ok(IrcCommand::Sjoin(SjoinParams {
                timestamp: params[0].parse().unwrap_or(0),
                channel: params[1].clone(),
                modes: params[2].clone(),
                members,
            }))
        }
        "PART" => {
            require_params("PART", &params, 1)?;
            warn_extra_params("PART", &params, 2);
            Ok(IrcCommand::Part {
                channel: params[0].clone(),
                reason: if params.len() >= 2 {
                    params.last().cloned()
                } else {
                    None
                },
            })
        }
        "KICK" => {
            require_params("KICK", &params, 2)?;
            warn_extra_params("KICK", &params, 3);
            Ok(IrcCommand::Kick {
                channel: params[0].clone(),
                target: params[1].clone(),
                reason: if params.len() >= 3 {
                    params.last().cloned()
                } else {
                    None
                },
            })
        }
        "NICK" => {
            require_params("NICK", &params, 2)?;
            warn_extra_params("NICK", &params, 2);
            Ok(IrcCommand::Nick {
                new_nick: params[0].clone(),
                timestamp: params[1].parse().unwrap_or(0),
            })
        }
        "QUIT" => {
            warn_extra_params("QUIT", &params, 1);
            Ok(IrcCommand::Quit {
                reason: params.last().cloned().unwrap_or_default(),
            })
        }
        "AWAY" => {
            warn_extra_params("AWAY", &params, 1);
            Ok(IrcCommand::Away {
                reason: params.last().cloned(),
            })
        }
        "SVSNICK" => {
            require_params("SVSNICK", &params, 2)?;
            warn_extra_params("SVSNICK", &params, 2);
            Ok(IrcCommand::Svsnick {
                target_uid: params[0].clone(),
                new_nick: params[1].clone(),
            })
        }
        "PRIVMSG" => {
            require_params("PRIVMSG", &params, 2)?;
            warn_extra_params("PRIVMSG", &params, 2);
            Ok(IrcCommand::Privmsg {
                target: params[0].clone(),
                text: params.last().unwrap().clone(),
            })
        }
        "NOTICE" => {
            require_params("NOTICE", &params, 2)?;
            warn_extra_params("NOTICE", &params, 2);
            Ok(IrcCommand::Notice {
                target: params[0].clone(),
                text: params.last().unwrap().clone(),
            })
        }
        "PING" => {
            require_params("PING", &params, 1)?;
            warn_extra_params("PING", &params, 1);
            Ok(IrcCommand::Ping {
                token: params.last().unwrap().clone(),
            })
        }
        "PONG" => {
            require_params("PONG", &params, 2)?;
            warn_extra_params("PONG", &params, 2);
            Ok(IrcCommand::Pong {
                server: params[0].clone(),
                token: params.last().unwrap().clone(),
            })
        }
        "EOS" => {
            warn_extra_params("EOS", &params, 0);
            Ok(IrcCommand::Eos)
        }
        "ERROR" => {
            warn_extra_params("ERROR", &params, 1);
            Ok(IrcCommand::Error {
                message: params.last().cloned().unwrap_or_default(),
            })
        }
        _ => Ok(IrcCommand::Raw {
            command: name.to_string(),
            params,
        }),
    }
}

// ---------------------------------------------------------------------------
// Private helpers — serialization
// ---------------------------------------------------------------------------

fn escape_tag_value(val: &str) -> String {
    let mut result = String::with_capacity(val.len());
    for ch in val.chars() {
        match ch {
            ';' => result.push_str("\\:"),
            ' ' => result.push_str("\\s"),
            '\\' => result.push_str("\\\\"),
            '\r' => result.push_str("\\r"),
            '\n' => result.push_str("\\n"),
            c => result.push(c),
        }
    }
    result
}

fn append_param(out: &mut String, param: &str, index: usize) -> Result<(), SerializeError> {
    if param.is_empty() || param.contains(' ') {
        return Err(SerializeError::InvalidParam {
            index,
            value: param.to_string(),
        });
    }
    out.push(' ');
    out.push_str(param);
    Ok(())
}

fn append_trailing(out: &mut String, trailing: &str) {
    out.push_str(" :");
    out.push_str(trailing);
}

fn write_command(cmd: &IrcCommand, out: &mut String) -> Result<(), SerializeError> {
    match cmd {
        IrcCommand::Pass { password } => {
            out.push_str("PASS");
            append_trailing(out, password);
        }
        IrcCommand::Server {
            name,
            hop_count,
            description,
        } => {
            out.push_str("SERVER");
            append_param(out, name, 0)?;
            let hc = hop_count.to_string();
            append_param(out, &hc, 1)?;
            append_trailing(out, description);
        }
        IrcCommand::Sid {
            name,
            hop_count,
            sid,
            description,
        } => {
            out.push_str("SID");
            append_param(out, name, 0)?;
            let hc = hop_count.to_string();
            append_param(out, &hc, 1)?;
            append_param(out, sid, 2)?;
            append_trailing(out, description);
        }
        IrcCommand::Protoctl { tokens } => {
            out.push_str("PROTOCTL");
            for (i, token) in tokens.iter().enumerate() {
                append_param(out, token, i)?;
            }
        }
        IrcCommand::Uid(p) => {
            out.push_str("UID");
            append_param(out, &p.nick, 0)?;
            let hc = p.hop_count.to_string();
            append_param(out, &hc, 1)?;
            let ts = p.timestamp.to_string();
            append_param(out, &ts, 2)?;
            append_param(out, &p.ident, 3)?;
            append_param(out, &p.host, 4)?;
            append_param(out, &p.uid, 5)?;
            append_param(out, &p.services_stamp, 6)?;
            append_param(out, &p.umodes, 7)?;
            append_param(out, &p.vhost, 8)?;
            append_param(out, &p.cloaked_host, 9)?;
            append_param(out, &p.ip, 10)?;
            append_trailing(out, &p.realname);
        }
        IrcCommand::Sjoin(p) => {
            out.push_str("SJOIN");
            let ts = p.timestamp.to_string();
            append_param(out, &ts, 0)?;
            append_param(out, &p.channel, 1)?;
            append_param(out, &p.modes, 2)?;
            let members = p.members.join(" ");
            append_trailing(out, &members);
        }
        IrcCommand::Part { channel, reason } => {
            out.push_str("PART");
            append_param(out, channel, 0)?;
            if let Some(r) = reason {
                append_trailing(out, r);
            }
        }
        IrcCommand::Kick {
            channel,
            target,
            reason,
        } => {
            out.push_str("KICK");
            append_param(out, channel, 0)?;
            append_param(out, target, 1)?;
            if let Some(r) = reason {
                append_trailing(out, r);
            }
        }
        IrcCommand::Nick {
            new_nick,
            timestamp,
        } => {
            out.push_str("NICK");
            append_param(out, new_nick, 0)?;
            let ts = timestamp.to_string();
            append_param(out, &ts, 1)?;
        }
        IrcCommand::Quit { reason } => {
            out.push_str("QUIT");
            append_trailing(out, reason);
        }
        IrcCommand::Away { reason } => {
            out.push_str("AWAY");
            if let Some(r) = reason {
                append_trailing(out, r);
            }
        }
        IrcCommand::Svsnick {
            target_uid,
            new_nick,
        } => {
            out.push_str("SVSNICK");
            append_param(out, target_uid, 0)?;
            append_param(out, new_nick, 1)?;
        }
        IrcCommand::Privmsg { target, text } => {
            out.push_str("PRIVMSG");
            append_param(out, target, 0)?;
            append_trailing(out, text);
        }
        IrcCommand::Notice { target, text } => {
            out.push_str("NOTICE");
            append_param(out, target, 0)?;
            append_trailing(out, text);
        }
        IrcCommand::Ping { token } => {
            out.push_str("PING");
            append_trailing(out, token);
        }
        IrcCommand::Pong { server, token } => {
            out.push_str("PONG");
            append_param(out, server, 0)?;
            append_trailing(out, token);
        }
        IrcCommand::Eos => {
            out.push_str("EOS");
        }
        IrcCommand::Error { message } => {
            out.push_str("ERROR");
            append_trailing(out, message);
        }
        IrcCommand::Raw { command, params } => {
            out.push_str(command);
            if let Some((last, rest)) = params.split_last() {
                for (i, param) in rest.iter().enumerate() {
                    append_param(out, param, i)?;
                }
                append_trailing(out, last);
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Helpers -----------------------------------------------------------

    fn uid() -> UidParams {
        UidParams {
            nick: "Alice".to_string(),
            hop_count: 1,
            timestamp: 1_700_000_000,
            ident: "alice".to_string(),
            host: "discord.invalid".to_string(),
            uid: "ABC000001".to_string(),
            services_stamp: "0".to_string(),
            umodes: "+i".to_string(),
            vhost: "*".to_string(),
            cloaked_host: "*".to_string(),
            ip: "*".to_string(),
            realname: "Alice Smith".to_string(),
        }
    }

    fn sjoin() -> SjoinParams {
        SjoinParams {
            timestamp: 1_700_000_000,
            channel: "#general".to_string(),
            modes: "+".to_string(),
            members: vec!["@ABC000001".to_string(), "ABC000002".to_string()],
        }
    }

    fn msg(command: IrcCommand) -> IrcMessage {
        IrcMessage {
            tags: vec![],
            prefix: None,
            command,
        }
    }

    // ---- Parsing: error cases ----------------------------------------------

    #[test]
    fn parse_empty_string_is_error() {
        assert_eq!(IrcMessage::parse(""), Err(ParseError::Empty));
    }

    #[test]
    fn parse_whitespace_only_is_error() {
        assert_eq!(IrcMessage::parse("   "), Err(ParseError::Empty));
    }

    #[test]
    fn parse_crlf_only_is_error() {
        assert_eq!(IrcMessage::parse("\r\n"), Err(ParseError::Empty));
    }

    #[test]
    fn parse_uid_too_few_params_is_error() {
        let result = IrcMessage::parse("UID Alice 1 :only three params");
        assert_eq!(
            result,
            Err(ParseError::MissingParams {
                command: "UID".to_string(),
                required: 12,
                got: 3
            })
        );
    }

    #[test]
    fn parse_uid_zero_params_is_error() {
        let result = IrcMessage::parse("UID");
        assert_eq!(
            result,
            Err(ParseError::MissingParams {
                command: "UID".to_string(),
                required: 12,
                got: 0
            })
        );
    }

    #[test]
    fn parse_uid_extra_params_succeeds() {
        // 13 params: a hypothetical future field inserted between ip and realname.
        // Greedy parsing takes the first 11 positional fields and uses the trailing
        // (last) param as realname, so the extra non-trailing param is ignored.
        let line =
            "UID Alice 1 1700000000 alice discord.invalid ABC000001 0 +i * * * future :Alice Smith";
        let result = IrcMessage::parse(line).unwrap();
        let IrcCommand::Uid(ref p) = result.command else {
            panic!("expected Uid");
        };
        assert_eq!(p.nick, "Alice");
        assert_eq!(p.ip, "*");
        assert_eq!(p.realname, "Alice Smith"); // trailing is still the realname
    }

    #[test]
    fn parse_server_extra_params_succeeds() {
        // SERVER with 4 params (1 extra before the trailing description).
        let result =
            IrcMessage::parse("SERVER irc.example.net 1 future_field :IRC network").unwrap();
        let IrcCommand::Server {
            ref description, ..
        } = result.command
        else {
            panic!("expected Server");
        };
        assert_eq!(description, "IRC network"); // trailing is still description
    }

    #[test]
    fn parse_privmsg_extra_params_succeeds() {
        // PRIVMSG with 3 params (1 extra between target and text).
        let result = IrcMessage::parse("PRIVMSG #general future_field :hello").unwrap();
        let IrcCommand::Privmsg {
            ref target,
            ref text,
        } = result.command
        else {
            panic!("expected Privmsg");
        };
        assert_eq!(target, "#general");
        assert_eq!(text, "hello"); // trailing is still the message text
    }

    #[test]
    fn parse_sjoin_too_few_params_is_error() {
        let result = IrcMessage::parse("SJOIN 12345 #test +");
        assert_eq!(
            result,
            Err(ParseError::MissingParams {
                command: "SJOIN".to_string(),
                required: 4,
                got: 3
            })
        );
    }

    // ---- Parsing: CRLF stripping -------------------------------------------

    #[test]
    fn parse_strips_crlf() {
        let result = IrcMessage::parse("PING :token\r\n").unwrap();
        assert_eq!(
            result.command,
            IrcCommand::Ping {
                token: "token".to_string()
            }
        );
    }

    #[test]
    fn parse_strips_lf_only() {
        let result = IrcMessage::parse("PING :token\n").unwrap();
        assert_eq!(
            result.command,
            IrcCommand::Ping {
                token: "token".to_string()
            }
        );
    }

    // ---- Parsing: tags -----------------------------------------------------

    #[test]
    fn parse_tag_with_value() {
        let msg = IrcMessage::parse("@time=2024-01-01T00:00:00.000Z PING :x").unwrap();
        assert_eq!(
            msg.tags,
            vec![(
                "time".to_string(),
                Some("2024-01-01T00:00:00.000Z".to_string())
            )]
        );
    }

    #[test]
    fn parse_tag_without_value() {
        let msg = IrcMessage::parse("@draft/typing PING :x").unwrap();
        assert_eq!(msg.tags, vec![("draft/typing".to_string(), None)]);
    }

    #[test]
    fn parse_multiple_tags() {
        let msg = IrcMessage::parse("@time=123;msgid=abc PING :x").unwrap();
        assert_eq!(
            msg.tags,
            vec![
                ("time".to_string(), Some("123".to_string())),
                ("msgid".to_string(), Some("abc".to_string())),
            ]
        );
    }

    #[test]
    fn parse_tag_value_unescapes_space() {
        let msg = IrcMessage::parse("@key=hello\\sworld PING :x").unwrap();
        assert_eq!(msg.tags[0].1, Some("hello world".to_string()));
    }

    #[test]
    fn parse_tag_value_unescapes_semicolon() {
        let msg = IrcMessage::parse("@key=a\\:b PING :x").unwrap();
        assert_eq!(msg.tags[0].1, Some("a;b".to_string()));
    }

    #[test]
    fn parse_tag_value_unescapes_backslash() {
        let msg = IrcMessage::parse("@key=a\\\\b PING :x").unwrap();
        assert_eq!(msg.tags[0].1, Some("a\\b".to_string()));
    }

    // ---- Parsing: prefix ---------------------------------------------------

    #[test]
    fn parse_prefix() {
        let msg = IrcMessage::parse(":server.example PING :x").unwrap();
        assert_eq!(msg.prefix, Some("server.example".to_string()));
    }

    #[test]
    fn parse_tags_and_prefix() {
        let msg = IrcMessage::parse("@time=1 :server.example PRIVMSG #ch :hello").unwrap();
        assert_eq!(msg.tags.len(), 1);
        assert_eq!(msg.prefix, Some("server.example".to_string()));
        assert_eq!(
            msg.command,
            IrcCommand::Privmsg {
                target: "#ch".to_string(),
                text: "hello".to_string(),
            }
        );
    }

    // ---- Parsing: individual commands --------------------------------------

    #[test]
    fn parse_privmsg_text_with_spaces() {
        let msg = IrcMessage::parse("PRIVMSG #general :hello world and more").unwrap();
        assert_eq!(
            msg.command,
            IrcCommand::Privmsg {
                target: "#general".to_string(),
                text: "hello world and more".to_string(),
            }
        );
    }

    #[test]
    fn parse_ping() {
        let msg = IrcMessage::parse("PING :irc.example.net").unwrap();
        assert_eq!(
            msg.command,
            IrcCommand::Ping {
                token: "irc.example.net".to_string()
            }
        );
    }

    #[test]
    fn parse_pong() {
        let msg = IrcMessage::parse("PONG discord.invalid :irc.example.net").unwrap();
        assert_eq!(
            msg.command,
            IrcCommand::Pong {
                server: "discord.invalid".to_string(),
                token: "irc.example.net".to_string(),
            }
        );
    }

    #[test]
    fn parse_pass() {
        let msg = IrcMessage::parse("PASS :s3cr3t").unwrap();
        assert_eq!(
            msg.command,
            IrcCommand::Pass {
                password: "s3cr3t".to_string()
            }
        );
    }

    #[test]
    fn parse_server() {
        let msg = IrcMessage::parse("SERVER irc.example.net 1 :IRC network").unwrap();
        assert_eq!(
            msg.command,
            IrcCommand::Server {
                name: "irc.example.net".to_string(),
                hop_count: 1,
                description: "IRC network".to_string(),
            }
        );
    }

    #[test]
    fn parse_sid() {
        let msg = IrcMessage::parse("SID irc.example.net 1 001 :IRC network").unwrap();
        assert_eq!(
            msg.command,
            IrcCommand::Sid {
                name: "irc.example.net".to_string(),
                hop_count: 1,
                sid: "001".to_string(),
                description: "IRC network".to_string(),
            }
        );
    }

    #[test]
    fn parse_protoctl() {
        let msg = IrcMessage::parse("PROTOCTL NOQUIT EAUTH=server.net,1.0 SID").unwrap();
        assert_eq!(
            msg.command,
            IrcCommand::Protoctl {
                tokens: vec![
                    "NOQUIT".to_string(),
                    "EAUTH=server.net,1.0".to_string(),
                    "SID".to_string(),
                ]
            }
        );
    }

    #[test]
    fn parse_uid_full() {
        let line = "UID Alice 1 1700000000 alice discord.invalid ABC000001 0 +i * * * :Alice Smith";
        let msg = IrcMessage::parse(line).unwrap();
        assert_eq!(msg.command, IrcCommand::Uid(uid()));
    }

    #[test]
    fn parse_uid_realname_with_spaces() {
        let line = "UID Bob 1 1700000000 bob discord.invalid ABC000002 0 +i * * * :Bob the Builder";
        let msg = IrcMessage::parse(line).unwrap();
        if let IrcCommand::Uid(p) = msg.command {
            assert_eq!(p.realname, "Bob the Builder");
        } else {
            panic!("expected Uid");
        }
    }

    #[test]
    fn parse_sjoin_basic() {
        let line = "SJOIN 1700000000 #general + :@ABC000001 ABC000002";
        let msg = IrcMessage::parse(line).unwrap();
        assert_eq!(msg.command, IrcCommand::Sjoin(sjoin()));
    }

    #[test]
    fn parse_sjoin_single_member() {
        let line = "SJOIN 1700000000 #general + :ABC000001";
        let msg = IrcMessage::parse(line).unwrap();
        if let IrcCommand::Sjoin(p) = msg.command {
            assert_eq!(p.members, vec!["ABC000001"]);
        } else {
            panic!("expected Sjoin");
        }
    }

    #[test]
    fn parse_quit() {
        let msg = IrcMessage::parse("QUIT :Leaving").unwrap();
        assert_eq!(
            msg.command,
            IrcCommand::Quit {
                reason: "Leaving".to_string()
            }
        );
    }

    #[test]
    fn parse_part_without_reason() {
        let msg = IrcMessage::parse("PART #general").unwrap();
        assert_eq!(
            msg.command,
            IrcCommand::Part {
                channel: "#general".to_string(),
                reason: None
            }
        );
    }

    #[test]
    fn parse_part_with_reason() {
        let msg = IrcMessage::parse("PART #general :Goodbye").unwrap();
        assert_eq!(
            msg.command,
            IrcCommand::Part {
                channel: "#general".to_string(),
                reason: Some("Goodbye".to_string()),
            }
        );
    }

    #[test]
    fn parse_kick_without_reason() {
        let msg = IrcMessage::parse("KICK #general ABC000001").unwrap();
        assert_eq!(
            msg.command,
            IrcCommand::Kick {
                channel: "#general".to_string(),
                target: "ABC000001".to_string(),
                reason: None,
            }
        );
    }

    #[test]
    fn parse_kick_with_reason() {
        let msg = IrcMessage::parse("KICK #general ABC000001 :spamming").unwrap();
        assert_eq!(
            msg.command,
            IrcCommand::Kick {
                channel: "#general".to_string(),
                target: "ABC000001".to_string(),
                reason: Some("spamming".to_string()),
            }
        );
    }

    #[test]
    fn parse_nick() {
        let msg = IrcMessage::parse("NICK Alice2 1700000001").unwrap();
        assert_eq!(
            msg.command,
            IrcCommand::Nick {
                new_nick: "Alice2".to_string(),
                timestamp: 1_700_000_001
            }
        );
    }

    #[test]
    fn parse_away_set() {
        let msg = IrcMessage::parse("AWAY :Be right back").unwrap();
        assert_eq!(
            msg.command,
            IrcCommand::Away {
                reason: Some("Be right back".to_string())
            }
        );
    }

    #[test]
    fn parse_away_unset() {
        let msg = IrcMessage::parse("AWAY").unwrap();
        assert_eq!(msg.command, IrcCommand::Away { reason: None });
    }

    #[test]
    fn parse_svsnick() {
        let msg = IrcMessage::parse("SVSNICK ABC000001 Alice2").unwrap();
        assert_eq!(
            msg.command,
            IrcCommand::Svsnick {
                target_uid: "ABC000001".to_string(),
                new_nick: "Alice2".to_string(),
            }
        );
    }

    #[test]
    fn parse_notice() {
        let msg = IrcMessage::parse("NOTICE Alice :welcome").unwrap();
        assert_eq!(
            msg.command,
            IrcCommand::Notice {
                target: "Alice".to_string(),
                text: "welcome".to_string()
            }
        );
    }

    #[test]
    fn parse_eos() {
        let msg = IrcMessage::parse("EOS").unwrap();
        assert_eq!(msg.command, IrcCommand::Eos);
    }

    #[test]
    fn parse_error() {
        let msg = IrcMessage::parse("ERROR :Closing link").unwrap();
        assert_eq!(
            msg.command,
            IrcCommand::Error {
                message: "Closing link".to_string()
            }
        );
    }

    #[test]
    fn parse_unknown_command_becomes_raw() {
        let msg = IrcMessage::parse("FOOBAR param1 :trailing value").unwrap();
        assert_eq!(
            msg.command,
            IrcCommand::Raw {
                command: "FOOBAR".to_string(),
                params: vec!["param1".to_string(), "trailing value".to_string()],
            }
        );
    }

    #[test]
    fn parse_command_case_insensitive() {
        // Commands on the wire are uppercase but the parser should be lenient.
        let msg = IrcMessage::parse("eos").unwrap();
        assert_eq!(msg.command, IrcCommand::Eos);
    }

    // ---- Serialization: basic ----------------------------------------------

    #[test]
    fn serialize_eos() {
        assert_eq!(msg(IrcCommand::Eos).to_wire().unwrap(), "EOS\r\n");
    }

    #[test]
    fn serialize_ping() {
        let m = msg(IrcCommand::Ping {
            token: "irc.example.net".to_string(),
        });
        assert_eq!(m.to_wire().unwrap(), "PING :irc.example.net\r\n");
    }

    #[test]
    fn serialize_privmsg_trailing_can_have_spaces() {
        let m = msg(IrcCommand::Privmsg {
            target: "#general".to_string(),
            text: "hello world".to_string(),
        });
        assert_eq!(m.to_wire().unwrap(), "PRIVMSG #general :hello world\r\n");
    }

    #[test]
    fn serialize_away_none() {
        let m = msg(IrcCommand::Away { reason: None });
        assert_eq!(m.to_wire().unwrap(), "AWAY\r\n");
    }

    #[test]
    fn serialize_away_some() {
        let m = msg(IrcCommand::Away {
            reason: Some("afk".to_string()),
        });
        assert_eq!(m.to_wire().unwrap(), "AWAY :afk\r\n");
    }

    #[test]
    fn serialize_part_no_reason() {
        let m = msg(IrcCommand::Part {
            channel: "#general".to_string(),
            reason: None,
        });
        assert_eq!(m.to_wire().unwrap(), "PART #general\r\n");
    }

    #[test]
    fn serialize_part_with_reason() {
        let m = msg(IrcCommand::Part {
            channel: "#general".to_string(),
            reason: Some("Goodbye".to_string()),
        });
        assert_eq!(m.to_wire().unwrap(), "PART #general :Goodbye\r\n");
    }

    #[test]
    fn serialize_error() {
        let m = msg(IrcCommand::Error {
            message: "Closing link".to_string(),
        });
        assert_eq!(m.to_wire().unwrap(), "ERROR :Closing link\r\n");
    }

    // ---- Serialization: prefix and tags ------------------------------------

    #[test]
    fn serialize_with_prefix() {
        let m = IrcMessage {
            tags: vec![],
            prefix: Some("discord.invalid".to_string()),
            command: IrcCommand::Eos,
        };
        assert_eq!(m.to_wire().unwrap(), ":discord.invalid EOS\r\n");
    }

    #[test]
    fn serialize_with_single_tag() {
        let m = IrcMessage {
            tags: vec![(
                "time".to_string(),
                Some("2024-01-01T00:00:00.000Z".to_string()),
            )],
            prefix: None,
            command: IrcCommand::Ping {
                token: "x".to_string(),
            },
        };
        assert_eq!(
            m.to_wire().unwrap(),
            "@time=2024-01-01T00:00:00.000Z PING :x\r\n"
        );
    }

    #[test]
    fn serialize_with_valueless_tag() {
        let m = IrcMessage {
            tags: vec![("draft/typing".to_string(), None)],
            prefix: None,
            command: IrcCommand::Eos,
        };
        assert_eq!(m.to_wire().unwrap(), "@draft/typing EOS\r\n");
    }

    #[test]
    fn serialize_tag_value_escapes_space() {
        let m = IrcMessage {
            tags: vec![("key".to_string(), Some("hello world".to_string()))],
            prefix: None,
            command: IrcCommand::Eos,
        };
        let wire = m.to_wire().unwrap();
        assert!(wire.starts_with("@key=hello\\sworld "), "got: {wire:?}");
    }

    #[test]
    fn serialize_tag_value_escapes_semicolon() {
        let m = IrcMessage {
            tags: vec![("key".to_string(), Some("a;b".to_string()))],
            prefix: None,
            command: IrcCommand::Eos,
        };
        let wire = m.to_wire().unwrap();
        assert!(wire.starts_with("@key=a\\:b "), "got: {wire:?}");
    }

    #[test]
    fn serialize_multiple_tags_separated_by_semicolons() {
        let m = IrcMessage {
            tags: vec![
                ("time".to_string(), Some("123".to_string())),
                ("msgid".to_string(), Some("abc".to_string())),
            ],
            prefix: None,
            command: IrcCommand::Eos,
        };
        let wire = m.to_wire().unwrap();
        assert!(wire.starts_with("@time=123;msgid=abc "), "got: {wire:?}");
    }

    // ---- Serialization: UID and SJOIN --------------------------------------

    #[test]
    fn serialize_uid() {
        let m = msg(IrcCommand::Uid(uid()));
        let wire = m.to_wire().unwrap();
        assert_eq!(
            wire,
            "UID Alice 1 1700000000 alice discord.invalid ABC000001 0 +i * * * :Alice Smith\r\n"
        );
    }

    #[test]
    fn serialize_sjoin() {
        let m = msg(IrcCommand::Sjoin(sjoin()));
        let wire = m.to_wire().unwrap();
        assert_eq!(
            wire,
            "SJOIN 1700000000 #general + :@ABC000001 ABC000002\r\n"
        );
    }

    // ---- Serialization: error cases ----------------------------------------

    #[test]
    fn serialize_invalid_param_with_space_is_error() {
        let m = msg(IrcCommand::Privmsg {
            target: "has space".to_string(),
            text: "ok".to_string(),
        });
        assert_eq!(
            m.to_wire(),
            Err(SerializeError::InvalidParam {
                index: 0,
                value: "has space".to_string(),
            })
        );
    }

    #[test]
    fn serialize_invalid_param_empty_is_error() {
        let m = msg(IrcCommand::Privmsg {
            target: String::new(),
            text: "ok".to_string(),
        });
        assert_eq!(
            m.to_wire(),
            Err(SerializeError::InvalidParam {
                index: 0,
                value: String::new()
            })
        );
    }

    #[test]
    fn serialize_line_too_long_is_error() {
        // Build a PRIVMSG whose body pushes the line past 4096 bytes.
        let text = "x".repeat(4090);
        let m = msg(IrcCommand::Privmsg {
            target: "#ch".to_string(),
            text,
        });
        match m.to_wire() {
            Err(SerializeError::LineTooLong { len }) => assert!(len > 4096),
            other => panic!("expected LineTooLong, got {other:?}"),
        }
    }

    // ---- Serialization: Raw ------------------------------------------------

    #[test]
    fn serialize_raw_no_params() {
        let m = msg(IrcCommand::Raw {
            command: "FOO".to_string(),
            params: vec![],
        });
        assert_eq!(m.to_wire().unwrap(), "FOO\r\n");
    }

    #[test]
    fn serialize_raw_single_param_becomes_trailing() {
        let m = msg(IrcCommand::Raw {
            command: "FOO".to_string(),
            params: vec!["bar".to_string()],
        });
        assert_eq!(m.to_wire().unwrap(), "FOO :bar\r\n");
    }

    #[test]
    fn serialize_raw_last_param_is_trailing() {
        let m = msg(IrcCommand::Raw {
            command: "FOO".to_string(),
            params: vec!["a".to_string(), "b c".to_string()],
        });
        assert_eq!(m.to_wire().unwrap(), "FOO a :b c\r\n");
    }

    // ---- Display -----------------------------------------------------------

    #[test]
    fn display_produces_wire_format() {
        let m = msg(IrcCommand::Eos);
        assert_eq!(m.to_string(), "EOS\r\n");
    }

    // ---- Round-trips -------------------------------------------------------

    #[test]
    fn roundtrip_privmsg() {
        let original = IrcMessage {
            tags: vec![],
            prefix: Some("ABC000001".to_string()),
            command: IrcCommand::Privmsg {
                target: "#general".to_string(),
                text: "hello world".to_string(),
            },
        };
        let wire = original.to_wire().unwrap();
        let parsed = IrcMessage::parse(&wire).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn roundtrip_uid() {
        let original = msg(IrcCommand::Uid(uid()));
        let wire = original.to_wire().unwrap();
        let parsed = IrcMessage::parse(&wire).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn roundtrip_sjoin() {
        let original = IrcMessage {
            tags: vec![],
            prefix: Some("001".to_string()),
            command: IrcCommand::Sjoin(sjoin()),
        };
        let wire = original.to_wire().unwrap();
        let parsed = IrcMessage::parse(&wire).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn roundtrip_with_tags() {
        let original = IrcMessage {
            tags: vec![
                (
                    "time".to_string(),
                    Some("2024-01-01T00:00:00.000Z".to_string()),
                ),
                ("msgid".to_string(), Some("abc;def".to_string())),
            ],
            prefix: Some("ABC000001".to_string()),
            command: IrcCommand::Privmsg {
                target: "#test".to_string(),
                text: "message with spaces".to_string(),
            },
        };
        let wire = original.to_wire().unwrap();
        let parsed = IrcMessage::parse(&wire).unwrap();
        assert_eq!(parsed, original);
    }

    // ---- Property-based round-trips ----------------------------------------

    use proptest::prelude::*;

    proptest! {
        #[test]
        fn proptest_privmsg_roundtrip(
            target in "#[a-zA-Z0-9]{1,30}",
            text in "[a-zA-Z0-9 ,.!?]{0,200}",
        ) {
            let original = msg(IrcCommand::Privmsg { target, text });
            let wire = original.to_wire().expect("valid privmsg should serialize");
            let parsed = IrcMessage::parse(&wire).expect("serialized wire should parse");
            prop_assert_eq!(parsed, original);
        }

        #[test]
        fn proptest_uid_roundtrip(
            nick in "[a-zA-Z][a-zA-Z0-9]{0,15}",
            realname in "[a-zA-Z0-9 ,.!?]{1,50}",
            ts in 0u64..=u64::MAX,
        ) {
            let mut p = uid();
            p.nick = nick;
            p.realname = realname;
            p.timestamp = ts;
            let original = msg(IrcCommand::Uid(p));
            let wire = original.to_wire().expect("valid uid should serialize");
            let parsed = IrcMessage::parse(&wire).expect("serialized wire should parse");
            prop_assert_eq!(parsed, original);
        }

        #[test]
        fn proptest_tag_value_roundtrip(
            // Characters that need escaping and printable ASCII
            val in "[a-zA-Z0-9 ;\\\\]{0,50}",
        ) {
            let original = IrcMessage {
                tags: vec![("key".to_string(), Some(val))],
                prefix: None,
                command: IrcCommand::Eos,
            };
            let wire = original.to_wire().expect("should serialize");
            let parsed = IrcMessage::parse(&wire).expect("should parse back");
            prop_assert_eq!(parsed, original);
        }
    }

    // ---- Boundary: 4096-byte line limit ------------------------------------

    /// Exactly 4096 bytes (excluding \r\n) must succeed — the limit is exclusive.
    #[test]
    fn serialize_exactly_4096_bytes_succeeds() {
        // "PRIVMSG #ch :" = 13 bytes; pad text to reach exactly 4096.
        let text = "x".repeat(4096 - 13);
        let m = msg(IrcCommand::Privmsg {
            target: "#ch".to_string(),
            text,
        });
        assert!(m.to_wire().is_ok(), "4096-byte line should succeed");
    }

    /// 4097 bytes must fail.
    #[test]
    fn serialize_4097_bytes_fails() {
        let text = "x".repeat(4096 - 13 + 1);
        let m = msg(IrcCommand::Privmsg {
            target: "#ch".to_string(),
            text,
        });
        assert!(
            matches!(m.to_wire(), Err(SerializeError::LineTooLong { .. })),
            "4097-byte line should fail"
        );
    }

    // ---- MissingParams errors when known commands have too few params ------

    #[test]
    fn parse_server_too_few_params_is_error() {
        let result = IrcMessage::parse("SERVER irc.example.net");
        assert_eq!(
            result,
            Err(ParseError::MissingParams {
                command: "SERVER".to_string(),
                required: 3,
                got: 1
            })
        );
    }

    #[test]
    fn parse_sid_too_few_params_is_error() {
        let result = IrcMessage::parse("SID irc.example.net 1 001");
        assert_eq!(
            result,
            Err(ParseError::MissingParams {
                command: "SID".to_string(),
                required: 4,
                got: 3
            })
        );
    }

    #[test]
    fn parse_part_no_params_is_error() {
        let result = IrcMessage::parse("PART");
        assert_eq!(
            result,
            Err(ParseError::MissingParams {
                command: "PART".to_string(),
                required: 1,
                got: 0
            })
        );
    }

    #[test]
    fn parse_kick_too_few_params_is_error() {
        let result = IrcMessage::parse("KICK #general");
        assert_eq!(
            result,
            Err(ParseError::MissingParams {
                command: "KICK".to_string(),
                required: 2,
                got: 1
            })
        );
    }

    #[test]
    fn parse_nick_too_few_params_is_error() {
        let result = IrcMessage::parse("NICK Alice2");
        assert_eq!(
            result,
            Err(ParseError::MissingParams {
                command: "NICK".to_string(),
                required: 2,
                got: 1
            })
        );
    }

    #[test]
    fn parse_svsnick_too_few_params_is_error() {
        let result = IrcMessage::parse("SVSNICK ABC000001");
        assert_eq!(
            result,
            Err(ParseError::MissingParams {
                command: "SVSNICK".to_string(),
                required: 2,
                got: 1
            })
        );
    }

    #[test]
    fn parse_privmsg_too_few_params_is_error() {
        let result = IrcMessage::parse("PRIVMSG #general");
        assert_eq!(
            result,
            Err(ParseError::MissingParams {
                command: "PRIVMSG".to_string(),
                required: 2,
                got: 1
            })
        );
    }

    #[test]
    fn parse_notice_too_few_params_is_error() {
        let result = IrcMessage::parse("NOTICE Alice");
        assert_eq!(
            result,
            Err(ParseError::MissingParams {
                command: "NOTICE".to_string(),
                required: 2,
                got: 1
            })
        );
    }

    // ---- warn_extra_params behaviour ---------------------------------------

    /// Parsing with *more* params than expected must emit a tracing WARN.
    ///
    /// This test (and the companion below) are the only tests that need
    /// `tracing-test`; they kill the four `warn_extra_params` mutations that
    /// otherwise survive (the function only produces a log side-effect).
    #[test]
    #[tracing_test::traced_test]
    fn parse_extra_params_logs_warn() {
        // SERVER normally takes 3 params; supply 4 (one extra before trailing).
        IrcMessage::parse("SERVER irc.example.net 1 future_field :IRC network").unwrap();
        assert!(
            logs_contain("ignoring"),
            "expected a WARN about extra params"
        );
    }

    /// Parsing with *exactly* the expected param count must NOT emit a WARN.
    #[test]
    #[tracing_test::traced_test]
    fn parse_exact_params_no_warn() {
        IrcMessage::parse("SERVER irc.example.net 1 :IRC network").unwrap();
        assert!(
            !logs_contain("ignoring"),
            "unexpected WARN for exact param count"
        );
    }

    #[test]
    fn parse_pong_too_few_params_is_error() {
        let result = IrcMessage::parse("PONG discord.invalid");
        assert_eq!(
            result,
            Err(ParseError::MissingParams {
                command: "PONG".to_string(),
                required: 2,
                got: 1
            })
        );
    }
}
