# disirc

A Discord-IRC bridge daemon that connects to UnrealIRCd as a peer server, presenting Discord users as real IRC pseudoclients.

This was almost entirely implemented by Claude, just to see what all the fuss is about. I have attempted to get it to correct it's most ridiculous mistakes, but I make no promises that any of this works (or that the docs match reality.)

## How it works

disirc links to an UnrealIRCd network using the server-to-server (S2S) protocol. Each online Discord user in a bridged channel gets their own IRC pseudoclient with a real nick, ident, and hostname. From the IRC side, Discord users look and behave like ordinary IRC users.

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

Messages flow bidirectionally:
- **Discord to IRC**: Messages from Discord users appear as PRIVMSGs from their pseudoclient — other IRC users see them as regular channel messages.
- **IRC to Discord**: Messages from IRC users are forwarded to the mapped Discord channel, either via webhooks (each IRC user gets their own username) or plain bot messages with a `**[nick]**` prefix.

## Features

- **Pseudoclient model** — Discord users appear as real IRC users with nicks, joins, quits, and presence (away/back).
- **Bidirectional message relay** — channel messages, notices, and actions (`/me`) bridged in both directions.
- **Webhook support** — IRC users appear with their own nick as the webhook username in Discord, giving each user a distinct visual identity.
- **Formatting conversion** — Discord markdown (bold, italic, underline, code, strikethrough) converted to IRC control codes and vice versa.
- **Mention resolution** — Discord `<@user>`, `<#channel>`, `<@&role>` mentions resolved to readable names on IRC. IRC `@nick` converted to Discord mentions. Leading `nick: ` addressing (common IRC convention) also converted to mentions.
- **DM bridging** (opt-in) — IRC `/msg` to a pseudoclient forwarded as a Discord DM. Discord DMs to the bridge bot relayed to the addressed IRC user via reply context or `nick: ` addressing.
- **Automatic reconnection** — exponential backoff with jitter on S2S link failure. Pseudoclient state preserved across reconnections for instant re-burst.
- **Hot-reloadable config** — add or remove bridge mappings without restarting (send SIGHUP or use the control channel).
- **Safety** — `@everyone` and `@here` suppressed on all IRC-to-Discord paths. Ping-fix zero-width space prevents IRC nicks from triggering Discord highlights.

## Requirements

- **UnrealIRCd 6.x** — disirc uses the UnrealIRCd S2S protocol. Other IRC daemons (InspIRCd, charybdis, etc.) are not supported.
- **Discord bot account** — with Server Members, Message Content, and Presence privileged intents enabled.
- **Rust** (stable) — for building from source.

## Quick start

1. Clone and build:

   ```sh
   git clone https://github.com/JonathanO/disirc.git
   cd disirc
   cargo build --release
   ```

2. Configure UnrealIRCd with a link block for the bridge:

   ```
   link bridge.example.net {
       incoming { mask *; };
       password "your-link-password";
       class servers;
       hub *;
   };
   ```

3. Create a Discord bot in the [Developer Portal](https://discord.com/developers/applications):

   - Under **Bot**, enable these Privileged Gateway Intents: **Server Members**, **Message Content**, **Presence**.
   - Copy the bot token.

4. Invite the bot to your Discord server using this OAuth2 URL (replace `YOUR_CLIENT_ID` with the Application ID from the Developer Portal):

   ```
   https://discord.com/oauth2/authorize?client_id=YOUR_CLIENT_ID&scope=bot&permissions=536874048
   ```

   This grants: View Channels, Send Messages, Read Message History, Manage Webhooks.

5. Copy and edit the config:

   ```sh
   cp config.example.toml config.toml
   ```

   Fill in your Discord bot token, IRC uplink address, link password, SID, and channel mappings. See the comments in `config.example.toml`.

6. Run:

   ```sh
   cargo run --release
   ```

   Or with debug logging:

   ```sh
   RUST_LOG=disirc=debug cargo run --release
   ```

## Configuration

See [`config.example.toml`](config.example.toml) for all options with comments. The key sections are:

- **`[discord]`** — bot token
- **`[irc]`** — uplink address, link credentials, SID
- **`[pseudoclients]`** — ident, DM bridging, KILL reintroduction
- **`[formatting]`** — nick-colon mention conversion
- **`[[bridge]]`** — one entry per Discord↔IRC channel pair, with optional webhook URL

See [DEVELOPING.md](DEVELOPING.md) for development setup, testing, and local UnrealIRCd Docker instructions.

## License

MIT
