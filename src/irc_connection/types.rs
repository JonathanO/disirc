use chrono::{DateTime, Utc};

/// Prefix status of a channel member, ordered from highest to lowest privilege.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemberPrefix {
    /// Channel owner (~)
    Owner,
    /// Channel admin (&)
    Admin,
    /// Channel op (@)
    Op,
    /// Half-op (%)
    HalfOp,
    /// Voice (+)
    Voice,
    /// Regular member (no prefix)
    None,
}

/// Protocol-agnostic events emitted by the connection module to the processing task.
///
/// The processing task receives these over an `mpsc` channel and must never
/// see `IrcMessage` or any other UnrealIRCd wire type directly.
#[derive(Debug, Clone, PartialEq)]
pub enum S2SEvent {
    /// Handshake complete; the link is ready for burst.
    LinkUp,

    /// Link lost or closed. The processing task must discard all IRC state.
    LinkDown { reason: String },

    /// A user was introduced to the network (inbound UID during burst or post-burst).
    UserIntroduced {
        /// 9-character UID (`SID` + 6 alphanumeric chars).
        uid: String,
        nick: String,
        ident: String,
        /// Displayed hostname (cloak or real, as received in the UID command).
        host: String,
        /// SID of the server that owns this user.
        server_sid: String,
        realname: String,
    },

    /// A user changed their nick.
    UserNickChanged { uid: String, new_nick: String },

    /// A user disconnected from the network.
    UserQuit { uid: String, reason: String },

    /// A new server was introduced to the network.
    ServerIntroduced { sid: String, name: String },

    /// A server left the network. All users homed to it must be removed.
    ServerQuit { sid: String, reason: String },

    /// A channel's full membership state was received (SJOIN burst).
    ChannelBurst {
        channel: String,
        /// Channel creation timestamp.
        ts: u64,
        /// List of (uid, prefix) pairs for each member.
        members: Vec<(String, MemberPrefix)>,
    },

    /// A single user joined a channel post-burst.
    UserJoined { uid: String, channel: String },

    /// A user left a channel.
    UserParted {
        uid: String,
        channel: String,
        reason: Option<String>,
    },

    /// A user was kicked from a channel.
    UserKicked {
        uid: String,
        channel: String,
        /// UID of the user who issued the kick.
        by_uid: String,
        reason: String,
    },

    /// A PRIVMSG was received from an IRC user.
    MessageReceived {
        from_uid: String,
        /// Channel name or UID (for direct messages).
        target: String,
        text: String,
        /// Parsed from the `@time=` message tag; `None` if the tag was absent.
        timestamp: Option<DateTime<Utc>>,
    },

    /// A NOTICE was received from an IRC user.
    NoticeReceived {
        from_uid: String,
        target: String,
        text: String,
    },

    /// A user set their away status.
    AwaySet { uid: String, reason: String },

    /// A user cleared their away status.
    AwayCleared { uid: String },

    /// Services forced a nick change on a user (SVSNICK).
    ///
    /// The processing task must apply this to `PseudoclientManager` if the
    /// target UID belongs to one of our pseudoclients.
    NickForced { uid: String, new_nick: String },

    /// The uplink has finished sending its burst (EOS received).
    BurstComplete,
}

/// Protocol-agnostic commands sent by the processing task to the connection module.
///
/// The connection module translates these into `IrcMessage` wire types before
/// writing to the socket.
#[derive(Debug, Clone, PartialEq)]
pub enum S2SCommand {
    /// Introduce a new pseudoclient to the network.
    ///
    /// The connection module supplies fixed wire values (hopcount, umodes,
    /// servicestamp, virthost, cloakedhost, ip) that are constant for all
    /// pseudoclients.
    IntroduceUser {
        uid: String,
        nick: String,
        ident: String,
        /// Displayed hostname for this pseudoclient.
        host: String,
        realname: String,
    },

    /// Join a pseudoclient to a channel.
    JoinChannel {
        uid: String,
        channel: String,
        /// Channel timestamp. Should match the channel's known ts, or current
        /// time if unknown.
        ts: u64,
    },

    /// Remove a pseudoclient from the network.
    QuitUser { uid: String, reason: String },

    /// Remove a pseudoclient from a single channel.
    PartChannel {
        uid: String,
        channel: String,
        reason: Option<String>,
    },

    /// Send a PRIVMSG from a pseudoclient.
    SendMessage {
        from_uid: String,
        target: String,
        text: String,
        /// If `Some`, emitted as an `@time=` tag (only when uplink advertised MTAGS).
        timestamp: Option<DateTime<Utc>>,
    },

    /// Send a NOTICE from a pseudoclient.
    SendNotice {
        from_uid: String,
        target: String,
        text: String,
    },

    /// Set a pseudoclient's away status.
    SetAway { uid: String, reason: String },

    /// Clear a pseudoclient's away status.
    ClearAway { uid: String },

    /// Signal that our burst is complete (translated to EOS on the wire).
    BurstComplete,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ts() -> DateTime<Utc> {
        Utc.timestamp_opt(1_700_000_000, 0).unwrap()
    }

    // --- MemberPrefix ---

    #[test]
    fn member_prefix_variants_are_distinct() {
        let prefixes = [
            MemberPrefix::Owner,
            MemberPrefix::Admin,
            MemberPrefix::Op,
            MemberPrefix::HalfOp,
            MemberPrefix::Voice,
            MemberPrefix::None,
        ];
        for (i, a) in prefixes.iter().enumerate() {
            for (j, b) in prefixes.iter().enumerate() {
                assert_eq!(a == b, i == j);
            }
        }
    }

    // --- S2SEvent construction ---

    #[test]
    fn s2s_event_link_up() {
        assert_eq!(S2SEvent::LinkUp, S2SEvent::LinkUp);
    }

    #[test]
    fn s2s_event_link_down() {
        let e = S2SEvent::LinkDown {
            reason: "socket error".into(),
        };
        let S2SEvent::LinkDown { reason } = e else {
            panic!()
        };
        assert_eq!(reason, "socket error");
    }

    #[test]
    fn s2s_event_user_introduced() {
        let e = S2SEvent::UserIntroduced {
            uid: "ABC000001".into(),
            nick: "Alice".into(),
            ident: "alice".into(),
            host: "discord.invalid".into(),
            server_sid: "ABC".into(),
            realname: "Alice Smith".into(),
        };
        let S2SEvent::UserIntroduced {
            uid,
            nick,
            ident,
            host,
            server_sid,
            realname,
        } = e
        else {
            panic!()
        };
        assert_eq!(uid, "ABC000001");
        assert_eq!(nick, "Alice");
        assert_eq!(ident, "alice");
        assert_eq!(host, "discord.invalid");
        assert_eq!(server_sid, "ABC");
        assert_eq!(realname, "Alice Smith");
    }

    #[test]
    fn s2s_event_user_nick_changed() {
        let e = S2SEvent::UserNickChanged {
            uid: "ABC000001".into(),
            new_nick: "Bob".into(),
        };
        let S2SEvent::UserNickChanged { uid, new_nick } = e else {
            panic!()
        };
        assert_eq!(uid, "ABC000001");
        assert_eq!(new_nick, "Bob");
    }

    #[test]
    fn s2s_event_user_quit() {
        let e = S2SEvent::UserQuit {
            uid: "ABC000001".into(),
            reason: "Quit".into(),
        };
        let S2SEvent::UserQuit { uid, reason } = e else {
            panic!()
        };
        assert_eq!(uid, "ABC000001");
        assert_eq!(reason, "Quit");
    }

    #[test]
    fn s2s_event_server_introduced() {
        let e = S2SEvent::ServerIntroduced {
            sid: "DEF".into(),
            name: "irc.example.net".into(),
        };
        let S2SEvent::ServerIntroduced { sid, name } = e else {
            panic!()
        };
        assert_eq!(sid, "DEF");
        assert_eq!(name, "irc.example.net");
    }

    #[test]
    fn s2s_event_server_quit() {
        let e = S2SEvent::ServerQuit {
            sid: "DEF".into(),
            reason: "split".into(),
        };
        let S2SEvent::ServerQuit { sid, reason } = e else {
            panic!()
        };
        assert_eq!(sid, "DEF");
        assert_eq!(reason, "split");
    }

    #[test]
    fn s2s_event_channel_burst() {
        let e = S2SEvent::ChannelBurst {
            channel: "#general".into(),
            ts: 1_700_000_000,
            members: vec![("ABC000001".into(), MemberPrefix::Op)],
        };
        let S2SEvent::ChannelBurst {
            channel,
            ts,
            members,
        } = e
        else {
            panic!()
        };
        assert_eq!(channel, "#general");
        assert_eq!(ts, 1_700_000_000);
        assert_eq!(members, vec![("ABC000001".into(), MemberPrefix::Op)]);
    }

    #[test]
    fn s2s_event_user_joined() {
        let e = S2SEvent::UserJoined {
            uid: "ABC000001".into(),
            channel: "#general".into(),
        };
        let S2SEvent::UserJoined { uid, channel } = e else {
            panic!()
        };
        assert_eq!(uid, "ABC000001");
        assert_eq!(channel, "#general");
    }

    #[test]
    fn s2s_event_user_parted_with_reason() {
        let e = S2SEvent::UserParted {
            uid: "ABC000001".into(),
            channel: "#general".into(),
            reason: Some("goodbye".into()),
        };
        let S2SEvent::UserParted {
            uid,
            channel,
            reason,
        } = e
        else {
            panic!()
        };
        assert_eq!(uid, "ABC000001");
        assert_eq!(channel, "#general");
        assert_eq!(reason, Some("goodbye".into()));
    }

    #[test]
    fn s2s_event_user_parted_no_reason() {
        let e = S2SEvent::UserParted {
            uid: "ABC000001".into(),
            channel: "#general".into(),
            reason: None,
        };
        let S2SEvent::UserParted { reason, .. } = e else {
            panic!()
        };
        assert!(reason.is_none());
    }

    #[test]
    fn s2s_event_user_kicked() {
        let e = S2SEvent::UserKicked {
            uid: "ABC000002".into(),
            channel: "#general".into(),
            by_uid: "ABC000001".into(),
            reason: "spam".into(),
        };
        let S2SEvent::UserKicked {
            uid,
            channel,
            by_uid,
            reason,
        } = e
        else {
            panic!()
        };
        assert_eq!(uid, "ABC000002");
        assert_eq!(channel, "#general");
        assert_eq!(by_uid, "ABC000001");
        assert_eq!(reason, "spam");
    }

    #[test]
    fn s2s_event_message_received_with_timestamp() {
        let t = ts();
        let e = S2SEvent::MessageReceived {
            from_uid: "ABC000001".into(),
            target: "#general".into(),
            text: "hello".into(),
            timestamp: Some(t),
        };
        let S2SEvent::MessageReceived {
            from_uid,
            target,
            text,
            timestamp,
        } = e
        else {
            panic!()
        };
        assert_eq!(from_uid, "ABC000001");
        assert_eq!(target, "#general");
        assert_eq!(text, "hello");
        assert_eq!(timestamp, Some(t));
    }

    #[test]
    fn s2s_event_message_received_no_timestamp() {
        let e = S2SEvent::MessageReceived {
            from_uid: "ABC000001".into(),
            target: "#general".into(),
            text: "hello".into(),
            timestamp: None,
        };
        let S2SEvent::MessageReceived { timestamp, .. } = e else {
            panic!()
        };
        assert!(timestamp.is_none());
    }

    #[test]
    fn s2s_event_notice_received() {
        let e = S2SEvent::NoticeReceived {
            from_uid: "ABC000001".into(),
            target: "#general".into(),
            text: "notice text".into(),
        };
        let S2SEvent::NoticeReceived {
            from_uid,
            target,
            text,
        } = e
        else {
            panic!()
        };
        assert_eq!(from_uid, "ABC000001");
        assert_eq!(target, "#general");
        assert_eq!(text, "notice text");
    }

    #[test]
    fn s2s_event_away_set() {
        let e = S2SEvent::AwaySet {
            uid: "ABC000001".into(),
            reason: "brb".into(),
        };
        let S2SEvent::AwaySet { uid, reason } = e else {
            panic!()
        };
        assert_eq!(uid, "ABC000001");
        assert_eq!(reason, "brb");
    }

    #[test]
    fn s2s_event_away_cleared() {
        let e = S2SEvent::AwayCleared {
            uid: "ABC000001".into(),
        };
        let S2SEvent::AwayCleared { uid } = e else {
            panic!()
        };
        assert_eq!(uid, "ABC000001");
    }

    #[test]
    fn s2s_event_nick_forced() {
        let e = S2SEvent::NickForced {
            uid: "ABC000001".into(),
            new_nick: "Alice_".into(),
        };
        let S2SEvent::NickForced { uid, new_nick } = e else {
            panic!()
        };
        assert_eq!(uid, "ABC000001");
        assert_eq!(new_nick, "Alice_");
    }

    #[test]
    fn s2s_event_burst_complete() {
        assert_eq!(S2SEvent::BurstComplete, S2SEvent::BurstComplete);
    }

    // --- S2SCommand construction ---

    #[test]
    fn s2s_command_introduce_user() {
        let c = S2SCommand::IntroduceUser {
            uid: "ABC000001".into(),
            nick: "Alice".into(),
            ident: "discord".into(),
            host: "Alice.discord.invalid".into(),
            realname: "Alice Smith".into(),
        };
        let S2SCommand::IntroduceUser {
            uid,
            nick,
            ident,
            host,
            realname,
        } = c
        else {
            panic!()
        };
        assert_eq!(uid, "ABC000001");
        assert_eq!(nick, "Alice");
        assert_eq!(ident, "discord");
        assert_eq!(host, "Alice.discord.invalid");
        assert_eq!(realname, "Alice Smith");
    }

    #[test]
    fn s2s_command_join_channel() {
        let c = S2SCommand::JoinChannel {
            uid: "ABC000001".into(),
            channel: "#general".into(),
            ts: 1_700_000_000,
        };
        let S2SCommand::JoinChannel { uid, channel, ts } = c else {
            panic!()
        };
        assert_eq!(uid, "ABC000001");
        assert_eq!(channel, "#general");
        assert_eq!(ts, 1_700_000_000);
    }

    #[test]
    fn s2s_command_quit_user() {
        let c = S2SCommand::QuitUser {
            uid: "ABC000001".into(),
            reason: "Gone".into(),
        };
        let S2SCommand::QuitUser { uid, reason } = c else {
            panic!()
        };
        assert_eq!(uid, "ABC000001");
        assert_eq!(reason, "Gone");
    }

    #[test]
    fn s2s_command_part_channel_with_reason() {
        let c = S2SCommand::PartChannel {
            uid: "ABC000001".into(),
            channel: "#general".into(),
            reason: Some("leaving".into()),
        };
        let S2SCommand::PartChannel {
            uid,
            channel,
            reason,
        } = c
        else {
            panic!()
        };
        assert_eq!(uid, "ABC000001");
        assert_eq!(channel, "#general");
        assert_eq!(reason, Some("leaving".into()));
    }

    #[test]
    fn s2s_command_part_channel_no_reason() {
        let c = S2SCommand::PartChannel {
            uid: "ABC000001".into(),
            channel: "#general".into(),
            reason: None,
        };
        let S2SCommand::PartChannel { reason, .. } = c else {
            panic!()
        };
        assert!(reason.is_none());
    }

    #[test]
    fn s2s_command_send_message_with_timestamp() {
        let t = ts();
        let c = S2SCommand::SendMessage {
            from_uid: "ABC000001".into(),
            target: "#general".into(),
            text: "hello".into(),
            timestamp: Some(t),
        };
        let S2SCommand::SendMessage {
            from_uid,
            target,
            text,
            timestamp,
        } = c
        else {
            panic!()
        };
        assert_eq!(from_uid, "ABC000001");
        assert_eq!(target, "#general");
        assert_eq!(text, "hello");
        assert_eq!(timestamp, Some(t));
    }

    #[test]
    fn s2s_command_send_message_no_timestamp() {
        let c = S2SCommand::SendMessage {
            from_uid: "ABC000001".into(),
            target: "#general".into(),
            text: "hello".into(),
            timestamp: None,
        };
        let S2SCommand::SendMessage { timestamp, .. } = c else {
            panic!()
        };
        assert!(timestamp.is_none());
    }

    #[test]
    fn s2s_command_send_notice() {
        let c = S2SCommand::SendNotice {
            from_uid: "ABC000001".into(),
            target: "#x".into(),
            text: "n".into(),
        };
        let S2SCommand::SendNotice {
            from_uid,
            target,
            text,
        } = c
        else {
            panic!()
        };
        assert_eq!(from_uid, "ABC000001");
        assert_eq!(target, "#x");
        assert_eq!(text, "n");
    }

    #[test]
    fn s2s_command_set_away() {
        let c = S2SCommand::SetAway {
            uid: "ABC000001".into(),
            reason: "brb".into(),
        };
        let S2SCommand::SetAway { uid, reason } = c else {
            panic!()
        };
        assert_eq!(uid, "ABC000001");
        assert_eq!(reason, "brb");
    }

    #[test]
    fn s2s_command_clear_away() {
        let c = S2SCommand::ClearAway {
            uid: "ABC000001".into(),
        };
        let S2SCommand::ClearAway { uid } = c else {
            panic!()
        };
        assert_eq!(uid, "ABC000001");
    }

    #[test]
    fn s2s_command_burst_complete() {
        assert_eq!(S2SCommand::BurstComplete, S2SCommand::BurstComplete);
    }
}
