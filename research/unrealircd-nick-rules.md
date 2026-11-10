# UnrealIRCd nick rules for remote users

## Summary
This note records which nicks UnrealIRCd 6 accepts in a `UID` from a linked
server. A nick must not start with a digit or `-`. It must not contain a space,
a control character, or any of `!+%@&~#$:'"?*,.`. It must not be `irc` or
`ircd` in any case. If the nick breaks a rule, the server sends
`KILL <nick> :Bad nick` to the link. The target of that `KILL` is the nick,
not the UID.

## Findings

### The check on a remote nick
`cmd_uid` in `src/modules/nick.c` calls `do_remote_nick_name(nick)`. If that
returns 0, or the nick is `ircd` or `irc` (`strcasecmp`, so in any case), the
server logs `BAD_NICK_REMOTE` and sends `:<me> KILL <nick> :Bad nick` to the
link. The `KILL` names the nick (`parv[1]`), not the UID (`parv[6]`). The user
does not appear on the network. `cmd_nick_remote` does the same check for a `NICK` change, and
kills the user with "Illegal nick name".

`_do_remote_nick_name` in `src/modules/charsys.c`:

```c
/* Don't allow nicks to start with a digit, ever. */
if ((*nick == '-') || isdigit(*nick))
	return 0;

/* Now the other, more relaxed checks.. */
for (c = nick; *c; c++)
	if ((*c <= 32) || strchr(illegalnickchars, *c))
		return 0;
```

`illegalnickchars` is `"!+%@&~#$:'\"?*,."`.

### The check on a local nick
`do_nick_name_standard` (local clients) has the same rule for the first
character. It also allows only the characters in `set::allowed-nickchars`. The
default set is `0123456789-ABCDEFGHIJKLMNOPQRSTUVWXYZ[\]^_`abcdefghijklmnopqrstuvwxyz{|}`.

### What this means for disirc
`sanitize_nick` keeps only the default local set, so every character is legal.
Two rules need their own step (spec 06, "Nick sanitization"):

- A nick that starts with a digit or `-` gets a `d` prefix.
- The nick `irc` or `ircd`, in any case, gets a `_` suffix.

disirc reads the target of an inbound `KILL` as a UID. So it does not see the
`KILL` for a bad nick, and it keeps the pseudoclient. The rules above make sure
that disirc never sends such a nick.

## References
- [unrealircd/unrealircd `src/modules/charsys.c` at 548d66d](https://github.com/unrealircd/unrealircd/blob/548d66d26c3e8e52ada71de84f07569fcee33f69/src/modules/charsys.c) — `_do_remote_nick_name`, `do_nick_name_standard`, `illegalnickchars`, `charsys_reset` — accessed 2026-11-10
- [unrealircd/unrealircd `src/modules/nick.c` (branch `unreal60_dev`)](https://github.com/unrealircd/unrealircd/blob/unreal60_dev/src/modules/nick.c) — `cmd_uid`, `cmd_nick_remote` — accessed 2026-11-10
