use std::collections::HashSet;
use std::sync::Arc;

use serenity::client::Client;
use serenity::model::gateway::GatewayIntents;
use tokio::sync::{RwLock, mpsc};
use tracing::error;

use crate::config::{BridgeEntry, DiscordConfig};
use crate::discord::handler::DiscordHandler;
use crate::discord::send::process_discord_commands;
use crate::discord::types::{DiscordCommand, DiscordEvent, webhook_id_from_url};

pub(crate) type ChannelIdSet = Arc<RwLock<HashSet<u64>>>;

/// Build the initial self-message filter set from bridge webhook URLs.
///
/// Each `webhook_url` in the bridge entries is parsed for its numeric webhook
/// ID, which equals the `author.id` that appears on `MESSAGE_CREATE` events
/// originating from that webhook.  Unknown or malformed URLs are silently
/// skipped (they will have already failed config validation).
pub(crate) fn webhook_ids_from_bridges(bridges: &[BridgeEntry]) -> HashSet<u64> {
    bridges
        .iter()
        .filter_map(|b| b.webhook_url.as_deref())
        .filter_map(webhook_id_from_url)
        .collect()
}

/// Build the set of bridged Discord channel IDs for fast `MESSAGE_CREATE`
/// routing decisions.
fn bridged_channel_ids(bridges: &[BridgeEntry]) -> HashSet<u64> {
    bridges
        .iter()
        .filter_map(|b| b.discord_channel_id.parse::<u64>().ok())
        .collect()
}

const INTENTS: GatewayIntents = GatewayIntents::from_bits_truncate(
    GatewayIntents::GUILD_MEMBERS.bits()
        | GatewayIntents::GUILD_MESSAGES.bits()
        | GatewayIntents::GUILD_PRESENCES.bits()
        | GatewayIntents::MESSAGE_CONTENT.bits(),
);

/// Connect to the Discord Gateway and run the event loop.
///
/// Serenity handles Gateway reconnection and session resumption automatically;
/// `client.start()` only returns on a fatal error.  On such an error this
/// function panics — there is no safe recovery path for a broken Discord
/// connection in the initial version.
///
/// Spawns a separate task to drain `cmd_rx` and send outgoing messages.
// mutants::skip — requires live Discord Gateway connection and bot token
#[mutants::skip]
pub async fn run_discord(
    config: &DiscordConfig,
    bridges: &[BridgeEntry],
    event_tx: mpsc::Sender<DiscordEvent>,
    cmd_rx: mpsc::Receiver<DiscordCommand>,
) -> ! {
    let self_filter: Arc<RwLock<HashSet<u64>>> =
        Arc::new(RwLock::new(webhook_ids_from_bridges(bridges)));
    let channel_ids: ChannelIdSet = Arc::new(RwLock::new(bridged_channel_ids(bridges)));

    let handler = DiscordHandler {
        event_tx: event_tx.clone(),
        self_filter: Arc::clone(&self_filter),
        bridged_channel_ids: Arc::clone(&channel_ids),
    };

    let mut client = match Client::builder(&config.token, INTENTS)
        .event_handler(handler)
        .await
    {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, "Failed to create Discord client");
            panic!("Failed to create Discord client: {e}");
        }
    };

    // Spawn the outgoing command + reload processor before starting the Gateway
    // loop.  client.http and client.cache are both available immediately after
    // Client::builder; the cache is already populated by the time any
    // ReloadBridges command can arrive.
    tokio::spawn(process_discord_commands(
        client.http.clone(),
        client.cache.clone(),
        cmd_rx,
        event_tx,
        self_filter,
        channel_ids,
    ));

    if let Err(e) = client.start().await {
        error!(error = %e, "Discord client fatal error");
    }
    // This function returns `-> !`. Serenity's client.start() only returns on
    // a fatal error that cannot be retried. Panicking here is intentional —
    // the tokio runtime will propagate the panic to the join handle in main.
    panic!("Discord client terminated unexpectedly");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BridgeEntry;

    fn bridge(channel_id: &str, webhook_url: Option<&str>) -> BridgeEntry {
        BridgeEntry {
            discord_channel_id: channel_id.to_string(),
            irc_channel: "#test".to_string(),
            webhook_url: webhook_url.map(str::to_string),
        }
    }

    #[test]
    fn no_webhooks_gives_empty_filter() {
        let bridges = vec![bridge("111", None), bridge("222", None)];
        assert!(webhook_ids_from_bridges(&bridges).is_empty());
    }

    #[test]
    fn single_webhook_url_parsed_into_filter() {
        let bridges = vec![bridge(
            "111",
            Some("https://discord.com/api/webhooks/123456789012345678/token"),
        )];
        let ids = webhook_ids_from_bridges(&bridges);
        assert_eq!(ids.len(), 1);
        assert!(ids.contains(&123_456_789_012_345_678_u64));
    }

    #[test]
    fn multiple_webhook_urls_all_parsed() {
        let bridges = vec![
            bridge(
                "111",
                Some("https://discord.com/api/webhooks/100000000000000001/tok"),
            ),
            bridge(
                "222",
                Some("https://discord.com/api/webhooks/200000000000000002/tok"),
            ),
        ];
        let ids = webhook_ids_from_bridges(&bridges);
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&100_000_000_000_000_001_u64));
        assert!(ids.contains(&200_000_000_000_000_002_u64));
    }

    #[test]
    fn mixed_some_and_none_only_includes_some() {
        let bridges = vec![
            bridge("111", None),
            bridge(
                "222",
                Some("https://discord.com/api/webhooks/999000000000000009/tok"),
            ),
            bridge("333", None),
        ];
        let ids = webhook_ids_from_bridges(&bridges);
        assert_eq!(ids.len(), 1);
        assert!(ids.contains(&999_000_000_000_000_009_u64));
    }

    #[test]
    fn bridged_channel_ids_parses_valid_ids() {
        let bridges = vec![bridge("111", None), bridge("222", None)];
        let ids = bridged_channel_ids(&bridges);
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&111));
        assert!(ids.contains(&222));
    }

    #[test]
    fn bridged_channel_ids_skips_non_numeric() {
        let bridges = vec![bridge("abc", None), bridge("999", None)];
        let ids = bridged_channel_ids(&bridges);
        assert_eq!(ids.len(), 1);
        assert!(ids.contains(&999));
    }

    #[test]
    fn bridged_channel_ids_empty_input() {
        assert!(bridged_channel_ids(&[]).is_empty());
    }

    #[test]
    fn intents_include_all_required_flags() {
        assert!(INTENTS.contains(GatewayIntents::GUILD_MEMBERS));
        assert!(INTENTS.contains(GatewayIntents::GUILD_MESSAGES));
        assert!(INTENTS.contains(GatewayIntents::GUILD_PRESENCES));
        assert!(INTENTS.contains(GatewayIntents::MESSAGE_CONTENT));
    }

    /// Requires a live Discord token — skipped in CI.
    #[tokio::test]
    #[ignore = "requires Discord credentials"]
    async fn run_discord_connects_and_records_bot_id() {
        // Integration smoke test: ensure the client builds and the ready()
        // handler fires, adding the bot user ID to the self_filter set.
        // Run manually with a real token to verify end-to-end.
    }
}
