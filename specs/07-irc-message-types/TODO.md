# TODO — spec/07-irc-message-types

Status: **Pending**

- [ ] Define `IrcCommand`, `UidParams`, `SjoinParams` types in `src/irc_message.rs`
- [ ] Implement `IrcMessage::parse()` with `ParseError`
- [ ] Implement `IrcMessage` serialization (`Display` / `to_wire()`) with `SerializeError`
- [ ] Update `src/pseudoclients.rs` to return `IrcMessage`/`Vec<IrcMessage>`
- [ ] Update `SPECS.md`
