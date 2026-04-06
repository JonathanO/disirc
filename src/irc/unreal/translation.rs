// Translation functions are called by the connection loop (next task).
#![allow(dead_code)]

use chrono::{DateTime, Utc};

use super::irc_message::{IrcCommand, IrcMessage, SjoinParams, UidParams};
use crate::irc::types::{MemberPrefix, S2SCommand, S2SEvent};

/// Parse the `@time=` message tag value into a `DateTime<Utc>`.
///
/// Returns `None` if the tag is absent, malformed, or out of range.
fn parse_time_tag(tags: &[(String, Option<String>)]) -> Option<DateTime<Utc>> {
    tags.iter()
        .find(|(k, _)| k == "time")
        .and_then(|(_, v)| v.as_deref())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
}

/// Parse a SJOIN member list entry into a (uid, prefix) pair.
///
/// SJOIN entries use leading sigils for prefix status: `*` owner, `~` admin,
/// `@` op, `%` half-op, `+` voice. Entries starting with `&`, `"`, or `'`
/// are list-mode entries (bans, exceptions, invex) and are skipped.
fn parse_sjoin_member(entry: &str) -> Option<(String, MemberPrefix)> {
    // Skip list-mode entries (ban / exception / invex sigils).
    if entry.starts_with(['&', '"', '\'']) {
        return None;
    }

    let (prefix, uid) = if let Some(rest) = entry.strip_prefix('*') {
        (MemberPrefix::Owner, rest)
    } else if let Some(rest) = entry.strip_prefix('~') {
        (MemberPrefix::Admin, rest)
    } else if let Some(rest) = entry.strip_prefix('@') {
        (MemberPrefix::Op, rest)
    } else if let Some(rest) = entry.strip_prefix('%') {
        (MemberPrefix::HalfOp, rest)
    } else if let Some(rest) = entry.strip_prefix('+') {
        (MemberPrefix::Voice, rest)
    } else {
        (MemberPrefix::None, entry)
    };

    if uid.is_empty() {
        return None;
    }

    Some((uid.to_owned(), prefix))
}

/// Translate an inbound `IrcMessage` into an `S2SEvent`.
///
/// Returns `None` for messages that are handled internally by the connection
/// loop (PING/PONG, handshake commands) or are unknown / safely ignorable.
///
/// PING must be handled by the caller (which sends PONG) before this is called;
/// this function returns `None` for PING.
///
/// `ERROR` messages must be handled specially by the caller (they trigger a
/// link-down transition); this function returns `None` for them.
pub fn translate_inbound(msg: &IrcMessage) -> Option<S2SEvent> {
    let prefix = msg.prefix.as_deref().unwrap_or("");

    match &msg.command {
        IrcCommand::Uid(p) => {
            let server_sid = if prefix.len() >= 3 { prefix[..3].to_owned() } else { prefix.to_owned() };
            Some(S2SEvent::UserIntroduced {
                uid: p.uid.clone(),
                nick: p.nick.clone(),
                ident: p.ident.clone(),
                host: p.host.clone(),
                server_sid,
                realname: p.realname.clone(),
            })
        }

        IrcCommand::Nick { new_nick, .. } => Some(S2SEvent::UserNickChanged {
            uid: prefix.to_owned(),
            new_nick: new_nick.clone(),
        }),

        IrcCommand::Quit { reason } => {
            Some(S2SEvent::UserQuit { uid: prefix.to_owned(), reason: reason.clone() })
        }

        IrcCommand::Sid { name, sid, .. } => {
            Some(S2SEvent::ServerIntroduced { sid: sid.clone(), name: name.clone() })
        }

        IrcCommand::Sjoin(p) => {
            let members: Vec<(String, MemberPrefix)> =
                p.members.iter().filter_map(|m| parse_sjoin_member(m)).collect();
            Some(S2SEvent::ChannelBurst { channel: p.channel.clone(), ts: p.timestamp, members })
        }

        IrcCommand::Part { channel, reason } => Some(S2SEvent::UserParted {
            uid: prefix.to_owned(),
            channel: channel.clone(),
            reason: reason.clone(),
        }),

        IrcCommand::Kill { target, reason } => Some(S2SEvent::UserKilled {
            uid: target.clone(),
            reason: reason.clone(),
        }),

        IrcCommand::Kick {
            channel,
            target,
            reason,
        } => Some(S2SEvent::UserKicked {
            uid: target.clone(),
            channel: channel.clone(),
            by_uid: prefix.to_owned(),
            reason: reason.clone().unwrap_or_default(),
        }),

        IrcCommand::Privmsg { target, text } => Some(S2SEvent::MessageReceived {
            from_uid: prefix.to_owned(),
            target: target.clone(),
            text: text.clone(),
            timestamp: parse_time_tag(&msg.tags),
        }),

        IrcCommand::Notice { target, text } => Some(S2SEvent::NoticeReceived {
            from_uid: prefix.to_owned(),
            target: target.clone(),
            text: text.clone(),
        }),

        IrcCommand::Away { reason } => {
            if let Some(r) = reason {
                Some(S2SEvent::AwaySet { uid: prefix.to_owned(), reason: r.clone() })
            } else {
                Some(S2SEvent::AwayCleared { uid: prefix.to_owned() })
            }
        }

        IrcCommand::Svsnick { target_uid, new_nick } => Some(S2SEvent::NickForced {
            uid: target_uid.clone(),
            new_nick: new_nick.clone(),
        }),

        IrcCommand::Eos => Some(S2SEvent::BurstComplete),

        // SQUIT is not a typed variant; handle it from the Raw fallthrough.
        IrcCommand::Raw { command, params } if command == "SQUIT" => {
            if params.len() >= 2 {
                Some(S2SEvent::ServerQuit {
                    sid: params[0].clone(),
                    reason: params.last().cloned().unwrap_or_default(),
                })
            } else {
                None
            }
        }

        // Handled internally by the connection loop before reaching here;
        // silently return None without logging.
        IrcCommand::Ping { .. }
        | IrcCommand::Pong { .. }
        | IrcCommand::Pass { .. }
        | IrcCommand::Server { .. }
        | IrcCommand::Protoctl { .. }
        // ERROR is handled by the connection loop and triggers LinkDown.
        | IrcCommand::Error { .. } => None,

        // Any command not covered above is unknown. Emit a trace log so it
        // can be inspected when diagnosing unexpected uplink behaviour, but
        // do not treat it as an error — live networks send many S2S commands
        // (MODE, TOPIC, TKL, NETINFO, …) that don't map to an S2SEvent.
        IrcCommand::Raw { command, .. } => {
            tracing::debug!(command, "Unrecognised inbound command — ignored");
            None
        }
    }
}

/// Fixed wire values applied to all pseudoclient UID introductions.
const HOP_COUNT: u32 = 1;
const SERVICESTAMP: &str = "0";
const UMODES: &str = "+i";
const VIRTHOST: &str = "*";
const CLOAKEDHOST: &str = "*";
const IP: &str = "*";

/// Translate an outbound `S2SCommand` into one or more `IrcMessage` values.
///
/// - `our_sid`: the SID configured for this bridge server.
/// - `mtags_active`: whether `@time=` tags should be emitted on `SendMessage`.
/// - `now_ts`: the current Unix timestamp (seconds), used for user introductions.
pub fn translate_outbound(
    cmd: &S2SCommand,
    our_sid: &str,
    mtags_active: bool,
    now_ts: u64,
) -> Vec<IrcMessage> {
    match cmd {
        S2SCommand::IntroduceUser {
            uid,
            nick,
            ident,
            host,
            realname,
        } => {
            vec![IrcMessage {
                tags: vec![],
                prefix: Some(our_sid.to_owned()),
                command: IrcCommand::Uid(UidParams {
                    nick: nick.clone(),
                    hop_count: HOP_COUNT,
                    timestamp: now_ts,
                    ident: ident.clone(),
                    host: host.clone(),
                    uid: uid.clone(),
                    services_stamp: SERVICESTAMP.to_owned(),
                    umodes: UMODES.to_owned(),
                    vhost: VIRTHOST.to_owned(),
                    cloaked_host: CLOAKEDHOST.to_owned(),
                    ip: IP.to_owned(),
                    realname: realname.clone(),
                }),
            }]
        }

        S2SCommand::JoinChannel { uid, channel, ts } => {
            vec![IrcMessage {
                tags: vec![],
                prefix: Some(our_sid.to_owned()),
                command: IrcCommand::Sjoin(SjoinParams {
                    timestamp: *ts,
                    channel: channel.clone(),
                    modes: "+".to_owned(),
                    members: vec![uid.clone()],
                }),
            }]
        }

        S2SCommand::QuitUser { uid, reason } => {
            vec![IrcMessage {
                tags: vec![],
                prefix: Some(uid.clone()),
                command: IrcCommand::Quit {
                    reason: reason.clone(),
                },
            }]
        }

        S2SCommand::PartChannel {
            uid,
            channel,
            reason,
        } => {
            vec![IrcMessage {
                tags: vec![],
                prefix: Some(uid.clone()),
                command: IrcCommand::Part {
                    channel: channel.clone(),
                    reason: reason.clone(),
                },
            }]
        }

        S2SCommand::SendMessage {
            from_uid,
            target,
            text,
            timestamp,
        } => {
            let tags = if mtags_active {
                timestamp
                    .map(|t| {
                        vec![(
                            "time".to_owned(),
                            Some(t.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()),
                        )]
                    })
                    .unwrap_or_default()
            } else {
                vec![]
            };
            vec![IrcMessage {
                tags,
                prefix: Some(from_uid.clone()),
                command: IrcCommand::Privmsg {
                    target: target.clone(),
                    text: text.clone(),
                },
            }]
        }

        S2SCommand::SendNotice {
            from_uid,
            target,
            text,
        } => {
            vec![IrcMessage {
                tags: vec![],
                prefix: Some(from_uid.clone()),
                command: IrcCommand::Notice {
                    target: target.clone(),
                    text: text.clone(),
                },
            }]
        }

        S2SCommand::ChangeNick { uid, new_nick } => {
            vec![IrcMessage {
                tags: vec![],
                prefix: Some(uid.clone()),
                command: IrcCommand::Nick {
                    new_nick: new_nick.clone(),
                    timestamp: now_ts,
                },
            }]
        }

        S2SCommand::SetAway { uid, reason } => {
            vec![IrcMessage {
                tags: vec![],
                prefix: Some(uid.clone()),
                command: IrcCommand::Away {
                    reason: Some(reason.clone()),
                },
            }]
        }

        S2SCommand::ClearAway { uid } => {
            vec![IrcMessage {
                tags: vec![],
                prefix: Some(uid.clone()),
                command: IrcCommand::Away { reason: None },
            }]
        }

        S2SCommand::BurstComplete => {
            vec![IrcMessage {
                tags: vec![],
                prefix: Some(our_sid.to_owned()),
                command: IrcCommand::Eos,
            }]
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::irc::unreal::irc_message::{IrcCommand, IrcMessage, SjoinParams, UidParams};

    const SID: &str = "ABC";
    const UID: &str = "ABC000001";

    fn uid_params() -> UidParams {
        UidParams {
            nick: "Alice".into(),
            hop_count: 1,
            timestamp: 1_700_000_000,
            ident: "alice".into(),
            host: "discord.invalid".into(),
            uid: UID.into(),
            services_stamp: "0".into(),
            umodes: "+i".into(),
            vhost: "*".into(),
            cloaked_host: "*".into(),
            ip: "*".into(),
            realname: "Alice Smith".into(),
        }
    }

    fn uid_msg(p: UidParams) -> IrcMessage {
        IrcMessage {
            tags: vec![],
            prefix: Some(SID.into()),
            command: IrcCommand::Uid(p),
        }
    }

    // -----------------------------------------------------------------------
    // translate_inbound — UID
    // -----------------------------------------------------------------------

    #[test]
    fn inbound_uid_produces_user_introduced() {
        let event = translate_inbound(&uid_msg(uid_params())).unwrap();
        assert_eq!(
            event,
            S2SEvent::UserIntroduced {
                uid: UID.into(),
                nick: "Alice".into(),
                ident: "alice".into(),
                host: "discord.invalid".into(),
                server_sid: SID.into(),
                realname: "Alice Smith".into(),
            }
        );
    }

    #[test]
    fn inbound_uid_extracts_server_sid_from_prefix() {
        let msg = IrcMessage {
            tags: vec![],
            prefix: Some("XYZ".into()),
            command: IrcCommand::Uid(uid_params()),
        };
        let S2SEvent::UserIntroduced { server_sid, .. } = translate_inbound(&msg).unwrap() else {
            panic!()
        };
        assert_eq!(server_sid, "XYZ");
    }

    #[test]
    fn inbound_uid_server_sid_is_first_three_chars_of_long_prefix() {
        // A real UID prefix is 9 chars (e.g. "XYZ123ABC"). server_sid must be
        // the 3-char SID portion only, not the full prefix.
        let msg = IrcMessage {
            tags: vec![],
            prefix: Some("XYZ123ABC".into()),
            command: IrcCommand::Uid(uid_params()),
        };
        let S2SEvent::UserIntroduced { server_sid, .. } = translate_inbound(&msg).unwrap() else {
            panic!()
        };
        assert_eq!(server_sid, "XYZ");
    }

    // -----------------------------------------------------------------------
    // translate_inbound — NICK
    // -----------------------------------------------------------------------

    #[test]
    fn inbound_nick_produces_user_nick_changed() {
        let msg = IrcMessage {
            tags: vec![],
            prefix: Some(UID.into()),
            command: IrcCommand::Nick {
                new_nick: "Bob".into(),
                timestamp: 1_700_000_001,
            },
        };
        assert_eq!(
            translate_inbound(&msg).unwrap(),
            S2SEvent::UserNickChanged {
                uid: UID.into(),
                new_nick: "Bob".into()
            }
        );
    }

    // -----------------------------------------------------------------------
    // translate_inbound — QUIT
    // -----------------------------------------------------------------------

    #[test]
    fn inbound_quit_produces_user_quit() {
        let msg = IrcMessage {
            tags: vec![],
            prefix: Some(UID.into()),
            command: IrcCommand::Quit {
                reason: "Leaving".into(),
            },
        };
        assert_eq!(
            translate_inbound(&msg).unwrap(),
            S2SEvent::UserQuit {
                uid: UID.into(),
                reason: "Leaving".into()
            }
        );
    }

    // -----------------------------------------------------------------------
    // translate_inbound — SID
    // -----------------------------------------------------------------------

    #[test]
    fn inbound_sid_produces_server_introduced() {
        let msg = IrcMessage {
            tags: vec![],
            prefix: Some(SID.into()),
            command: IrcCommand::Sid {
                name: "irc.example.net".into(),
                hop_count: 2,
                sid: "DEF".into(),
                description: "Example network".into(),
            },
        };
        assert_eq!(
            translate_inbound(&msg).unwrap(),
            S2SEvent::ServerIntroduced {
                sid: "DEF".into(),
                name: "irc.example.net".into()
            }
        );
    }

    // -----------------------------------------------------------------------
    // translate_inbound — SQUIT (via Raw)
    // -----------------------------------------------------------------------

    #[test]
    fn inbound_squit_produces_server_quit() {
        let msg = IrcMessage {
            tags: vec![],
            prefix: Some(SID.into()),
            command: IrcCommand::Raw {
                command: "SQUIT".into(),
                params: vec!["DEF".into(), "netsplit".into()],
            },
        };
        assert_eq!(
            translate_inbound(&msg).unwrap(),
            S2SEvent::ServerQuit {
                sid: "DEF".into(),
                reason: "netsplit".into()
            }
        );
    }

    #[test]
    fn inbound_squit_too_few_params_returns_none() {
        let msg = IrcMessage {
            tags: vec![],
            prefix: Some(SID.into()),
            command: IrcCommand::Raw {
                command: "SQUIT".into(),
                params: vec!["DEF".into()], // missing reason
            },
        };
        assert!(translate_inbound(&msg).is_none());
    }

    // -----------------------------------------------------------------------
    // translate_inbound — SJOIN
    // -----------------------------------------------------------------------

    #[test]
    fn inbound_sjoin_produces_channel_burst_with_members() {
        let msg = IrcMessage {
            tags: vec![],
            prefix: Some(SID.into()),
            command: IrcCommand::Sjoin(SjoinParams {
                timestamp: 1_700_000_000,
                channel: "#general".into(),
                modes: "+n".into(),
                members: vec!["@ABC000001".into(), "+ABC000002".into(), "ABC000003".into()],
            }),
        };
        let S2SEvent::ChannelBurst {
            channel,
            ts,
            members,
        } = translate_inbound(&msg).unwrap()
        else {
            panic!()
        };
        assert_eq!(channel, "#general");
        assert_eq!(ts, 1_700_000_000);
        assert_eq!(
            members,
            vec![
                ("ABC000001".into(), MemberPrefix::Op),
                ("ABC000002".into(), MemberPrefix::Voice),
                ("ABC000003".into(), MemberPrefix::None),
            ]
        );
    }

    #[test]
    fn inbound_sjoin_skips_list_mode_entries() {
        let msg = IrcMessage {
            tags: vec![],
            prefix: Some(SID.into()),
            command: IrcCommand::Sjoin(SjoinParams {
                timestamp: 1_700_000_000,
                channel: "#general".into(),
                modes: "+nb".into(),
                members: vec!["&*!*@spam.invalid".into(), "ABC000001".into()],
            }),
        };
        let S2SEvent::ChannelBurst { members, .. } = translate_inbound(&msg).unwrap() else {
            panic!()
        };
        assert_eq!(members, vec![("ABC000001".into(), MemberPrefix::None)]);
    }

    #[test]
    fn inbound_sjoin_all_prefix_types() {
        let msg = IrcMessage {
            tags: vec![],
            prefix: Some(SID.into()),
            command: IrcCommand::Sjoin(SjoinParams {
                timestamp: 1_700_000_000,
                channel: "#test".into(),
                modes: "+".into(),
                members: vec![
                    "*ABC000001".into(),
                    "~ABC000002".into(),
                    "@ABC000003".into(),
                    "%ABC000004".into(),
                    "+ABC000005".into(),
                    "ABC000006".into(),
                ],
            }),
        };
        let S2SEvent::ChannelBurst { members, .. } = translate_inbound(&msg).unwrap() else {
            panic!()
        };
        assert_eq!(
            members,
            vec![
                ("ABC000001".into(), MemberPrefix::Owner),
                ("ABC000002".into(), MemberPrefix::Admin),
                ("ABC000003".into(), MemberPrefix::Op),
                ("ABC000004".into(), MemberPrefix::HalfOp),
                ("ABC000005".into(), MemberPrefix::Voice),
                ("ABC000006".into(), MemberPrefix::None),
            ]
        );
    }

    // -----------------------------------------------------------------------
    // translate_inbound — PART / KICK
    // -----------------------------------------------------------------------

    #[test]
    fn inbound_part_with_reason() {
        let msg = IrcMessage {
            tags: vec![],
            prefix: Some(UID.into()),
            command: IrcCommand::Part {
                channel: "#general".into(),
                reason: Some("bye".into()),
            },
        };
        assert_eq!(
            translate_inbound(&msg).unwrap(),
            S2SEvent::UserParted {
                uid: UID.into(),
                channel: "#general".into(),
                reason: Some("bye".into())
            }
        );
    }

    #[test]
    fn inbound_part_no_reason() {
        let msg = IrcMessage {
            tags: vec![],
            prefix: Some(UID.into()),
            command: IrcCommand::Part {
                channel: "#general".into(),
                reason: None,
            },
        };
        let S2SEvent::UserParted { reason, .. } = translate_inbound(&msg).unwrap() else {
            panic!()
        };
        assert!(reason.is_none());
    }

    #[test]
    fn inbound_kick() {
        let msg = IrcMessage {
            tags: vec![],
            prefix: Some(UID.into()),
            command: IrcCommand::Kick {
                channel: "#general".into(),
                target: "ABC000002".into(),
                reason: Some("spam".into()),
            },
        };
        let event = translate_inbound(&msg).unwrap();
        // uid = the kicked user (target), by_uid = the kicker (prefix)
        assert_eq!(
            event,
            S2SEvent::UserKicked {
                uid: "ABC000002".into(),
                channel: "#general".into(),
                by_uid: UID.into(),
                reason: "spam".into(),
            }
        );
    }

    #[test]
    fn inbound_kick_distinguishes_kicker_from_target() {
        // Use distinct values for prefix and target to ensure they are not conflated.
        let msg = IrcMessage {
            tags: vec![],
            prefix: Some("001KICKER".into()),
            command: IrcCommand::Kick {
                channel: "#test".into(),
                target: "001KICKED".into(),
                reason: Some("bye".into()),
            },
        };
        let event = translate_inbound(&msg).unwrap();
        let S2SEvent::UserKicked {
            uid,
            by_uid,
            channel,
            reason,
        } = event
        else {
            panic!("expected UserKicked");
        };
        assert_eq!(uid, "001KICKED", "uid must be the kicked user (target)");
        assert_eq!(by_uid, "001KICKER", "by_uid must be the kicker (prefix)");
        assert_eq!(channel, "#test");
        assert_eq!(reason, "bye");
    }

    #[test]
    fn inbound_kick_no_reason_defaults_empty() {
        let msg = IrcMessage {
            tags: vec![],
            prefix: Some(UID.into()),
            command: IrcCommand::Kick {
                channel: "#general".into(),
                target: "ABC000002".into(),
                reason: None,
            },
        };
        let S2SEvent::UserKicked { reason, .. } = translate_inbound(&msg).unwrap() else {
            panic!()
        };
        assert_eq!(reason, "");
    }

    #[test]
    fn inbound_kill_translates_to_user_killed() {
        let msg = IrcMessage {
            tags: vec![],
            prefix: Some("001OPER01".into()),
            command: IrcCommand::Kill {
                target: "002AAAAAA".into(),
                reason: "Killed (abuse)".into(),
            },
        };
        let event = translate_inbound(&msg).unwrap();
        assert_eq!(
            event,
            S2SEvent::UserKilled {
                uid: "002AAAAAA".to_string(),
                reason: "Killed (abuse)".to_string(),
            }
        );
    }

    // -----------------------------------------------------------------------
    // translate_inbound — PRIVMSG / NOTICE
    // -----------------------------------------------------------------------

    #[test]
    fn inbound_privmsg_no_tags() {
        let msg = IrcMessage {
            tags: vec![],
            prefix: Some(UID.into()),
            command: IrcCommand::Privmsg {
                target: "#general".into(),
                text: "hello".into(),
            },
        };
        assert_eq!(
            translate_inbound(&msg).unwrap(),
            S2SEvent::MessageReceived {
                from_uid: UID.into(),
                target: "#general".into(),
                text: "hello".into(),
                timestamp: None,
            }
        );
    }

    #[test]
    fn inbound_privmsg_with_time_tag() {
        let msg = IrcMessage {
            tags: vec![("time".into(), Some("2023-11-14T22:13:20.000Z".into()))],
            prefix: Some(UID.into()),
            command: IrcCommand::Privmsg {
                target: "#general".into(),
                text: "timed".into(),
            },
        };
        let S2SEvent::MessageReceived { timestamp, .. } = translate_inbound(&msg).unwrap() else {
            panic!()
        };
        assert_eq!(
            timestamp,
            Some(Utc.timestamp_opt(1_700_000_000, 0).unwrap())
        );
    }

    #[test]
    fn inbound_notice() {
        let msg = IrcMessage {
            tags: vec![],
            prefix: Some(UID.into()),
            command: IrcCommand::Notice {
                target: "#general".into(),
                text: "notice".into(),
            },
        };
        assert_eq!(
            translate_inbound(&msg).unwrap(),
            S2SEvent::NoticeReceived {
                from_uid: UID.into(),
                target: "#general".into(),
                text: "notice".into(),
            }
        );
    }

    // -----------------------------------------------------------------------
    // translate_inbound — AWAY
    // -----------------------------------------------------------------------

    #[test]
    fn inbound_away_set() {
        let msg = IrcMessage {
            tags: vec![],
            prefix: Some(UID.into()),
            command: IrcCommand::Away {
                reason: Some("brb".into()),
            },
        };
        assert_eq!(
            translate_inbound(&msg).unwrap(),
            S2SEvent::AwaySet {
                uid: UID.into(),
                reason: "brb".into()
            }
        );
    }

    #[test]
    fn inbound_away_cleared() {
        let msg = IrcMessage {
            tags: vec![],
            prefix: Some(UID.into()),
            command: IrcCommand::Away { reason: None },
        };
        assert_eq!(
            translate_inbound(&msg).unwrap(),
            S2SEvent::AwayCleared { uid: UID.into() }
        );
    }

    // -----------------------------------------------------------------------
    // translate_inbound — SVSNICK / EOS
    // -----------------------------------------------------------------------

    #[test]
    fn inbound_svsnick_produces_nick_forced() {
        let msg = IrcMessage {
            tags: vec![],
            prefix: Some(SID.into()),
            command: IrcCommand::Svsnick {
                target_uid: UID.into(),
                new_nick: "Alice_".into(),
            },
        };
        assert_eq!(
            translate_inbound(&msg).unwrap(),
            S2SEvent::NickForced {
                uid: UID.into(),
                new_nick: "Alice_".into()
            }
        );
    }

    #[test]
    fn inbound_eos_produces_burst_complete() {
        let msg = IrcMessage {
            tags: vec![],
            prefix: Some(SID.into()),
            command: IrcCommand::Eos,
        };
        assert_eq!(translate_inbound(&msg).unwrap(), S2SEvent::BurstComplete);
    }

    // -----------------------------------------------------------------------
    // translate_inbound — commands that return None
    // -----------------------------------------------------------------------

    #[test]
    fn inbound_ping_returns_none() {
        let msg = IrcMessage {
            tags: vec![],
            prefix: None,
            command: IrcCommand::Ping {
                token: "ABC".into(),
            },
        };
        assert!(translate_inbound(&msg).is_none());
    }

    #[test]
    fn inbound_protoctl_returns_none() {
        let msg = IrcMessage {
            tags: vec![],
            prefix: None,
            command: IrcCommand::Protoctl {
                tokens: vec!["NOQUIT".into()],
            },
        };
        assert!(translate_inbound(&msg).is_none());
    }

    #[test]
    fn inbound_unknown_raw_returns_none() {
        let msg = IrcMessage::parse("UNKNOWNCMD something :else").unwrap();
        assert!(translate_inbound(&msg).is_none());
    }

    // -----------------------------------------------------------------------
    // translate_outbound — IntroduceUser → UID
    // -----------------------------------------------------------------------

    #[test]
    fn outbound_introduce_user_produces_uid_with_fixed_fields() {
        let cmd = S2SCommand::IntroduceUser {
            uid: UID.into(),
            nick: "Alice".into(),
            ident: "discord".into(),
            host: "Alice.discord.invalid".into(),
            realname: "Alice Smith".into(),
        };
        let msgs = translate_outbound(&cmd, SID, false, 1_700_000_000);
        assert_eq!(msgs.len(), 1);
        let IrcCommand::Uid(ref p) = msgs[0].command else {
            panic!()
        };
        assert_eq!(p.uid, UID);
        assert_eq!(p.nick, "Alice");
        assert_eq!(p.ident, "discord");
        assert_eq!(p.host, "Alice.discord.invalid");
        assert_eq!(p.realname, "Alice Smith");
        // Fixed wire values
        assert_eq!(p.hop_count, 1);
        assert_eq!(p.services_stamp, "0");
        assert_eq!(p.umodes, "+i");
        assert_eq!(p.vhost, "*");
        assert_eq!(p.cloaked_host, "*");
        assert_eq!(p.ip, "*");
        assert_eq!(p.timestamp, 1_700_000_000);
        assert_eq!(msgs[0].prefix, Some(SID.into()));
    }

    // -----------------------------------------------------------------------
    // translate_outbound — JoinChannel → SJOIN
    // -----------------------------------------------------------------------

    #[test]
    fn outbound_join_channel_produces_sjoin() {
        let cmd = S2SCommand::JoinChannel {
            uid: UID.into(),
            channel: "#general".into(),
            ts: 1_700_000_000,
        };
        let msgs = translate_outbound(&cmd, SID, false, 0);
        assert_eq!(msgs.len(), 1);
        let IrcCommand::Sjoin(ref p) = msgs[0].command else {
            panic!()
        };
        assert_eq!(p.channel, "#general");
        assert_eq!(p.timestamp, 1_700_000_000);
        assert_eq!(p.members, vec![UID]);
        assert_eq!(msgs[0].prefix, Some(SID.into()));
    }

    // -----------------------------------------------------------------------
    // translate_outbound — QuitUser → QUIT
    // -----------------------------------------------------------------------

    #[test]
    fn outbound_quit_user_produces_quit_with_uid_prefix() {
        let cmd = S2SCommand::QuitUser {
            uid: UID.into(),
            reason: "Gone".into(),
        };
        let msgs = translate_outbound(&cmd, SID, false, 0);
        assert_eq!(msgs.len(), 1);
        let IrcCommand::Quit { ref reason } = msgs[0].command else {
            panic!()
        };
        assert_eq!(reason, "Gone");
        assert_eq!(msgs[0].prefix, Some(UID.into()));
    }

    // -----------------------------------------------------------------------
    // translate_outbound — PartChannel → PART
    // -----------------------------------------------------------------------

    #[test]
    fn outbound_part_with_reason() {
        let cmd = S2SCommand::PartChannel {
            uid: UID.into(),
            channel: "#x".into(),
            reason: Some("bye".into()),
        };
        let msgs = translate_outbound(&cmd, SID, false, 0);
        let IrcCommand::Part {
            ref channel,
            ref reason,
        } = msgs[0].command
        else {
            panic!()
        };
        assert_eq!(channel, "#x");
        assert_eq!(reason.as_deref(), Some("bye"));
    }

    #[test]
    fn outbound_part_no_reason() {
        let cmd = S2SCommand::PartChannel {
            uid: UID.into(),
            channel: "#x".into(),
            reason: None,
        };
        let msgs = translate_outbound(&cmd, SID, false, 0);
        let IrcCommand::Part { ref reason, .. } = msgs[0].command else {
            panic!()
        };
        assert!(reason.is_none());
    }

    // -----------------------------------------------------------------------
    // translate_outbound — SendMessage → PRIVMSG (with / without @time=)
    // -----------------------------------------------------------------------

    #[test]
    fn outbound_message_no_timestamp_has_no_tags() {
        let cmd = S2SCommand::SendMessage {
            from_uid: UID.into(),
            target: "#general".into(),
            text: "hello".into(),
            timestamp: None,
        };
        let msgs = translate_outbound(&cmd, SID, true, 0);
        assert!(msgs[0].tags.is_empty());
    }

    #[test]
    fn outbound_message_with_timestamp_and_mtags_emits_time_tag() {
        let t = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let cmd = S2SCommand::SendMessage {
            from_uid: UID.into(),
            target: "#general".into(),
            text: "hi".into(),
            timestamp: Some(t),
        };
        let msgs = translate_outbound(&cmd, SID, true, 0);
        let time_tag = msgs[0].tags.iter().find(|(k, _)| k == "time");
        assert!(time_tag.is_some(), "expected @time= tag when mtags_active");
    }

    #[test]
    fn outbound_message_timestamp_suppressed_without_mtags() {
        let t = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let cmd = S2SCommand::SendMessage {
            from_uid: UID.into(),
            target: "#general".into(),
            text: "hi".into(),
            timestamp: Some(t),
        };
        let msgs = translate_outbound(&cmd, SID, false, 0);
        assert!(
            msgs[0].tags.is_empty(),
            "expected no tags when mtags not active"
        );
    }

    // -----------------------------------------------------------------------
    // translate_outbound — SetAway / ClearAway → AWAY
    // -----------------------------------------------------------------------

    #[test]
    fn outbound_set_away_has_reason() {
        let cmd = S2SCommand::SetAway {
            uid: UID.into(),
            reason: "brb".into(),
        };
        let msgs = translate_outbound(&cmd, SID, false, 0);
        let IrcCommand::Away { ref reason } = msgs[0].command else {
            panic!()
        };
        assert_eq!(reason.as_deref(), Some("brb"));
    }

    #[test]
    fn outbound_clear_away_has_no_reason() {
        let cmd = S2SCommand::ClearAway { uid: UID.into() };
        let msgs = translate_outbound(&cmd, SID, false, 0);
        let IrcCommand::Away { ref reason } = msgs[0].command else {
            panic!()
        };
        assert!(reason.is_none());
    }

    // -----------------------------------------------------------------------
    // translate_outbound — BurstComplete → EOS
    // -----------------------------------------------------------------------

    #[test]
    fn outbound_burst_complete_produces_eos_with_sid_prefix() {
        let msgs = translate_outbound(&S2SCommand::BurstComplete, SID, false, 0);
        assert_eq!(msgs.len(), 1);
        assert!(matches!(msgs[0].command, IrcCommand::Eos));
        assert_eq!(msgs[0].prefix, Some(SID.into()));
    }

    // -----------------------------------------------------------------------
    // translate_outbound — ChangeNick → NICK
    // -----------------------------------------------------------------------

    #[test]
    fn outbound_change_nick_produces_nick_with_uid_prefix() {
        let cmd = S2SCommand::ChangeNick {
            uid: UID.into(),
            new_nick: "newnick".into(),
        };
        let msgs = translate_outbound(&cmd, SID, false, 1_700_000_000);
        assert_eq!(msgs.len(), 1);
        let IrcCommand::Nick {
            new_nick,
            timestamp,
        } = &msgs[0].command
        else {
            panic!("expected Nick command");
        };
        assert_eq!(new_nick, "newnick");
        assert_eq!(*timestamp, 1_700_000_000);
        assert_eq!(msgs[0].prefix, Some(UID.into()));
    }

    // -----------------------------------------------------------------------
    // parse_time_tag
    // -----------------------------------------------------------------------

    #[test]
    fn parse_time_tag_absent_returns_none() {
        assert!(parse_time_tag(&[]).is_none());
    }

    #[test]
    fn parse_time_tag_malformed_returns_none() {
        let tags = vec![("time".into(), Some("not-a-date".into()))];
        assert!(parse_time_tag(&tags).is_none());
    }

    #[test]
    fn parse_time_tag_valueless_returns_none() {
        let tags = vec![("time".into(), None)];
        assert!(parse_time_tag(&tags).is_none());
    }

    #[test]
    fn parse_time_tag_valid_rfc3339() {
        let tags = vec![("time".into(), Some("2023-11-14T22:13:20.000Z".into()))];
        assert_eq!(
            parse_time_tag(&tags),
            Some(Utc.timestamp_opt(1_700_000_000, 0).unwrap())
        );
    }

    // -----------------------------------------------------------------------
    // parse_sjoin_member
    // -----------------------------------------------------------------------

    #[test]
    fn sjoin_member_ban_sigil_skipped() {
        assert!(parse_sjoin_member("&*!*@spam.invalid").is_none());
    }

    #[test]
    fn sjoin_member_exception_sigil_skipped() {
        assert!(parse_sjoin_member("\"*!*@allowed.invalid").is_none());
    }

    #[test]
    fn sjoin_member_invex_sigil_skipped() {
        assert!(parse_sjoin_member("'*!*@invited.invalid").is_none());
    }

    #[test]
    fn sjoin_member_empty_after_prefix_skipped() {
        assert!(parse_sjoin_member("@").is_none());
    }
}
