# Serenity 0.12 API Surface

## Summary

Serenity 0.12 uses a builder-pattern `Client::builder()` API, an `#[async_trait]`
`EventHandler` trait with per-event async methods, and `Arc<Http>` for REST calls.
State is shared with the event handler by storing it as a struct field (preferred for
simple cases) or via the `TypeMap` in `ctx.data`.

## Findings

### Client creation

```rust
let intents = GatewayIntents::GUILD_MEMBERS
    | GatewayIntents::GUILD_MESSAGES
    | GatewayIntents::GUILD_PRESENCES
    | GatewayIntents::MESSAGE_CONTENT;

let mut client = Client::builder(&token, intents)
    .event_handler(MyHandler { tx })
    .await?;

client.start().await?;  // single shard; use start_autosharded() for >2500 guilds
```

### EventHandler trait (relevant methods)

```rust
#[async_trait]
impl EventHandler for MyHandler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        let bot_id: UserId = ready.user.id;
    }

    async fn guild_create(&self, ctx: Context, guild: Guild, is_new: Option<bool>) {
        // guild.members: HashMap<UserId, Member>
        // guild.presences: HashMap<UserId, Presence>
    }

    async fn message(&self, ctx: Context, msg: Message) {
        let author_id: UserId   = msg.author.id;
        let channel_id: ChannelId = msg.channel_id;
        let content: &str       = &msg.content;
        let guild_id: Option<GuildId> = msg.guild_id;
        let attachments: &[Attachment] = &msg.attachments;
    }

    async fn presence_update(&self, ctx: Context, new_data: Presence) {
        let user_id: UserId     = new_data.user.id;
        let status: OnlineStatus = new_data.status;  // Online/Idle/DoNotDisturb/Invisible/Offline
        let guild_id: Option<GuildId> = new_data.guild_id;
    }

    async fn guild_member_addition(&self, ctx: Context, new_member: Member) {
        let user_id: UserId  = new_member.user.id;
        let guild_id: GuildId = new_member.guild_id;
    }

    async fn guild_member_removal(
        &self, ctx: Context,
        guild_id: GuildId, user: User,
        member_data_if_available: Option<Member>
    ) {
        let user_id: UserId = user.id;
    }
}
```

### OnlineStatus enum

```rust
// serenity::model::user::OnlineStatus  (non_exhaustive)
OnlineStatus::Online
OnlineStatus::Idle
OnlineStatus::DoNotDisturb   // maps to Discord "Do Not Disturb" / dnd
OnlineStatus::Invisible
OnlineStatus::Offline
```

### Sending messages

```rust
// Plain send
channel_id.say(&ctx.http, "text").await?;

// Builder (embeds, components, etc.)
channel_id.send_message(&ctx.http, CreateMessage::new().content("text")).await?;
```

### Webhook execution

```rust
use serenity::builder::{CreateAllowedMentions, ExecuteWebhook};

let result = webhook.execute(
    &ctx.http,
    false,  // wait=false: fire and forget
    ExecuteWebhook::new()
        .content("message body")
        .username("IRC Nick")
        .allowed_mentions(CreateAllowedMentions::new()),  // parse:[] — no @everyone/@here
).await?;
```

- `Webhook::execute(&Http, wait: bool, ExecuteWebhook) -> Result<Option<Message>>`
- `CreateAllowedMentions::new()` with no `.parse()` call produces `allowed_mentions: { parse: [] }`,
  suppressing @everyone and @here.

### HTTP client access

```rust
// Inside EventHandler — ctx.http is Arc<Http>
ctx.http.get_user(user_id).await?;
```

### ID types

`UserId`, `ChannelId`, `GuildId`, `MessageId` are all newtypes around `u64`:
```rust
UserId::new(123_u64)  // construct
user_id.get()         // extract u64
```
All implement `PartialEq`, `Eq`, `Hash`, `Ord` — safe for use in `HashMap`/`HashSet`.

### Sharing state with EventHandler

Preferred pattern for disirc — store `Sender` directly in the handler struct:

```rust
struct DiscordHandler {
    event_tx: mpsc::Sender<DiscordEvent>,
}

#[async_trait]
impl EventHandler for DiscordHandler {
    async fn message(&self, _ctx: Context, msg: Message) {
        // Non-blocking send; drop the error if the receiver is gone
        let _ = self.event_tx.try_send(DiscordEvent::MessageReceived { ... });
    }
}
```

This avoids the `TypeMap` `Arc<RwLock<_>>` overhead for simple cases.
For state that must be mutated after construction, wrap in `Arc<Mutex<_>>` or
`Arc<RwLock<_>>` and store directly in the handler struct.

### Reconnection

Serenity handles Gateway reconnection and session resumption automatically.
No custom reconnect logic is required; `client.start()` blocks until fatal error.

## References

- [serenity 0.12.4 — Client](https://docs.rs/serenity/0.12.4/serenity/client/struct.Client.html) — accessed 2026-03-26
- [serenity 0.12.4 — EventHandler](https://docs.rs/serenity/0.12.4/serenity/client/trait.EventHandler.html) — accessed 2026-03-26
- [serenity 0.12.4 — Message](https://docs.rs/serenity/0.12.4/serenity/model/channel/struct.Message.html) — accessed 2026-03-26
- [serenity 0.12.4 — Presence](https://docs.rs/serenity/0.12.4/serenity/model/gateway/struct.Presence.html) — accessed 2026-03-26
- [serenity 0.12.4 — Guild](https://docs.rs/serenity/0.12.4/serenity/model/guild/struct.Guild.html) — accessed 2026-03-26
- [serenity 0.12.4 — Ready](https://docs.rs/serenity/0.12.4/serenity/model/gateway/struct.Ready.html) — accessed 2026-03-26
- [serenity 0.12.4 — ExecuteWebhook](https://docs.rs/serenity/0.12.4/serenity/builder/struct.ExecuteWebhook.html) — accessed 2026-03-26
- [serenity 0.12.4 — OnlineStatus](https://docs.rs/serenity/0.12.4/serenity/model/user/enum.OnlineStatus.html) — accessed 2026-03-26
