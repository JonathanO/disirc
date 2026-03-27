use std::collections::HashSet;
use std::sync::Arc;

use serenity::async_trait;
use serenity::client::{Context, EventHandler};
use serenity::model::gateway::Ready;
use tokio::sync::{RwLock, mpsc};
use tracing::info;

use crate::discord::types::DiscordEvent;

/// Serenity event handler for the Discord Gateway.
///
/// State shared across handler calls is wrapped in `Arc` so the handler can
/// be cheaply cloned if the client needs to be rebuilt.
#[derive(Clone)]
pub(crate) struct DiscordHandler {
    /// Channel to the processing task; used by all event handlers (tasks 3–7).
    // Fields used in upcoming handler methods; allow until those tasks land.
    #[allow(dead_code)]
    pub(crate) event_tx: mpsc::Sender<DiscordEvent>,
    /// IDs to suppress on `MESSAGE_CREATE` (bot user ID + webhook user IDs).
    pub(crate) self_filter: Arc<RwLock<HashSet<u64>>>,
    /// Discord channel IDs that have an active bridge entry (used in task 4).
    #[allow(dead_code)]
    pub(crate) bridged_channel_ids: Arc<HashSet<u64>>,
}

#[async_trait]
impl EventHandler for DiscordHandler {
    async fn ready(&self, _ctx: Context, ready: Ready) {
        let bot_id = ready.user.id.get();
        self.self_filter.write().await.insert(bot_id);
        info!(
            bot_id,
            tag = %ready.user.tag(),
            "Discord bot ready"
        );
    }
}
