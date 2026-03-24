# TODO — spec/05-formatting

Status: **Implemented**

- [x] Discord→IRC: mention and emoji resolution (`<@id>`, `<#id>`, `<@&id>`, `<:name:id>`, `<a:name:id>`)
- [x] Discord→IRC: markdown to IRC control codes (bold, italic, underline, strikethrough)
- [x] Discord→IRC: newline splitting, code block handling, length splitting at 400 bytes
- [x] IRC→Discord: control character stripping/conversion (Unicode Cc category)
- [x] IRC→Discord: `@nick` mention conversion, ping-fix zero-width space, length truncation at 2000 chars
- [x] `server-time` ISO 8601 timestamp formatting (chrono) + proptest suite across all transforms
