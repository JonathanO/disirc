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
    ///
    /// Used for both burst-time full-channel SJOIN and post-burst single-member
    /// SJOIN. The processing task updates its membership state from the member list.
    ChannelBurst {
        channel: String,
        /// Channel creation timestamp.
        ts: u64,
        /// List of (uid, prefix) pairs for each member.
        members: Vec<(String, MemberPrefix)>,
    },

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

// No unit tests: S2SEvent, S2SCommand, and MemberPrefix are plain data types
// with no logic to test. Behaviour is covered by the translation layer tests in
// translation.rs.
