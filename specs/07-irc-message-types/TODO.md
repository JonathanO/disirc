# TODO — spec/07-irc-message-types

Status: **Implemented**

- [x] Define `IrcCommand`, `UidParams`, `SjoinParams` types in `src/irc_message.rs`
- [x] Implement `IrcMessage::parse()` with `ParseError`
- [x] Implement `IrcMessage` serialization (`Display` / `to_wire()`) with `SerializeError`
- [x] Update `src/pseudoclients.rs` to return `IrcMessage`/`Vec<IrcMessage>`
- [x] Update `SPECS.md`
