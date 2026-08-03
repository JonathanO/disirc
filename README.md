# disirc

disirc is a Discord-IRC bridge daemon. It connects to UnrealIRCd as a peer server. It presents each Discord user as a real IRC pseudoclient.

This was almost entirely implemented by Claude, just to see what all the fuss is about. I have attempted to get it to correct it's most ridiculous mistakes, but I make no promises that any of this works (or that the docs match reality.)

## How it works

disirc links to an UnrealIRCd network with the server-to-server (S2S) protocol. Each online Discord user in a bridged channel gets one IRC pseudoclient. The pseudoclient has a real nick, a real ident, and a real hostname. From the IRC side, a Discord user looks like an ordinary IRC user.

```
IRC network                          Discord
  |                                    |
  |  S2S link                          |  Gateway
  |                                    |
  +-- disirc (pseudo-server) ----------+
  |     |                              |
  |     +-- Alice (pseudoclient)       +-- Alice (Discord user)
  |     +-- Bob   (pseudoclient)       +-- Bob   (Discord user)
  |                                    |
  +-- real IRC users                   +-- other Discord users
```

Messages travel in both directions:

- **Discord to IRC**: disirc sends each Discord message as a PRIVMSG from the pseudoclient of the sender. Other IRC users see a regular channel message.
- **IRC to Discord**: disirc forwards each IRC message to the mapped Discord channel. It uses a webhook, which gives each IRC user a separate username. If the channel has no webhook, disirc sends a bot message with a `**[nick]**` prefix.

## Features

- **Pseudoclient model** — Each Discord user appears as a real IRC user. Nicks, joins, quits, and presence (away and back) all work.
- **Message relay in both directions** — disirc bridges channel messages, notices, and actions (`/me`).
- **Webhook support** — Each IRC user appears in Discord under their own nick, because disirc sets the webhook username. Every user therefore keeps a separate visual identity.
- **Formatting conversion** — disirc converts Discord markdown to IRC control codes. It converts IRC control codes back to Discord markdown. This covers bold, italic, underline, code, and strikethrough.
- **Mention resolution** — disirc converts Discord `<@user>`, `<#channel>`, and `<@&role>` mentions to readable names on IRC. It converts IRC `@nick` to a Discord mention. It also converts a leading `nick: `, which is the usual way to address a user on IRC.
- **DM bridging** (you must enable it) — disirc forwards an IRC `/msg` to a pseudoclient as a Discord DM. It relays a Discord DM back to the addressed IRC user. The addressed user comes from the reply context or from a leading `nick: `.
- **Automatic reconnection** — If the S2S link fails, disirc reconnects. It uses exponential backoff with jitter. It keeps the pseudoclient state, so the next burst is immediate.
- **Configuration reload** — You can add or remove a bridge mapping without a restart. Send SIGHUP, or use the control channel.
- **Safety** — disirc suppresses `@everyone` and `@here` on every IRC-to-Discord path. It also inserts a zero-width space into IRC nicks, which stops Discord from sending a highlight.

## Requirements

- **UnrealIRCd 6.x** — disirc uses the UnrealIRCd S2S protocol. It does not support other IRC daemons, such as InspIRCd or charybdis.
- **Discord bot account** — You must enable the Server Members, Message Content, and Presence privileged intents.
- **Rust** (stable) — to build from source.

## Quick start

1. Clone and build:

   ```sh
   git clone https://github.com/JonathanO/disirc.git
   cd disirc
   cargo build --release
   ```

2. Add a link block for the bridge to the UnrealIRCd configuration:

   ```
   link bridge.example.net {
       incoming { mask *; };
       password "your-link-password";
       class servers;
       hub *;
   };
   ```

3. Create a Discord bot in the [Developer Portal](https://discord.com/developers/applications):

   - Under **Bot**, enable these privileged gateway intents: **Server Members**, **Message Content**, and **Presence**.
   - Copy the bot token.

4. Invite the bot to your Discord server with this OAuth2 URL. Replace `YOUR_CLIENT_ID` with the Application ID from the Developer Portal:

   ```
   https://discord.com/oauth2/authorize?client_id=YOUR_CLIENT_ID&scope=bot&permissions=536874048
   ```

   This URL grants these permissions: View Channels, Send Messages, Read Message History, and Manage Webhooks.

5. Copy and edit the configuration:

   ```sh
   cp config.example.toml config.toml
   ```

   Enter your Discord bot token, the IRC uplink address, the link password, the SID, and the channel mappings. The comments in `config.example.toml` describe each field.

6. Run disirc:

   ```sh
   cargo run --release
   ```

   To include debug logs:

   ```sh
   RUST_LOG=disirc=debug cargo run --release
   ```

## Configuration

[`config.example.toml`](config.example.toml) lists every option with comments. These are the main sections:

- **`[discord]`** — the bot token
- **`[irc]`** — the uplink address, the link credentials, and the SID
- **`[pseudoclients]`** — the ident, DM bridging, and KILL reintroduction
- **`[formatting]`** — the conversion of a leading `nick: ` to a mention
- **`[[bridge]]`** — one entry for each pair of Discord and IRC channels, with an optional webhook URL

## Running in production

**Run disirc under a process supervisor that restarts it.** Use systemd with
`Restart=always`, a Docker `--restart=unless-stopped` policy, or an equivalent.

If disirc reaches a state it cannot recover from, it exits. It does not continue
with corrupt data. There is one such state today: the UID space is exhausted.

disirc allocates each pseudoclient UID from a per-process counter of 36^6
suffixes. If the counter passes that limit, the suffix wraps. disirc would then
reissue a UID that belongs to a live pseudoclient. Two Discord users would share
one IRC identity, and disirc would attribute their messages to the wrong person.
disirc aborts instead. The counter is per-process, so a restart clears it.

To reach the limit, disirc must allocate 2,176,782,336 distinct UIDs in one
process lifetime. This is a safety net, not an expected event.

Pseudoclient state survives a restart. disirc stores the channel memberships and
the activity timestamps in the state file that `pseudoclients.state_file` names.

[DEVELOPING.md](DEVELOPING.md) describes the development setup, the tests, and how to run UnrealIRCd in Docker.

## License

MIT
