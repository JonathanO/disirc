//! Lightweight Discord REST client for e2e test verification.
//!
//! Uses a separate test bot (not the bridge bot) to send messages into
//! Discord channels and poll for messages that arrive via the bridge.

use serde::Deserialize;
use std::time::Duration;

const DISCORD_API: &str = "https://discord.com/api/v10";

/// Minimal representation of a Discord message returned by the REST API.
#[derive(Debug, Deserialize)]
pub struct DiscordMessage {
    pub id: String,
    pub content: String,
    pub author: DiscordAuthor,
}

/// Minimal author info within a Discord message.
#[derive(Debug, Deserialize)]
pub struct DiscordAuthor {
    pub id: String,
    pub username: String,
    /// True when the message was sent via a webhook.
    #[serde(default)]
    pub bot: bool,
}

/// A raw `reqwest`-based Discord REST client used by the test harness bot
/// to send and poll messages. Does not use the Gateway.
pub struct DiscordTestClient {
    http: reqwest::Client,
    token: String,
    channel_id: u64,
}

impl DiscordTestClient {
    /// Create a new client for the given channel.
    pub fn new(token: &str, channel_id: u64) -> Self {
        Self {
            http: reqwest::Client::new(),
            token: token.to_string(),
            channel_id,
        }
    }

    /// Send a message to the channel and return the created message.
    pub async fn send_message(&self, content: &str) -> DiscordMessage {
        let url = format!("{DISCORD_API}/channels/{}/messages", self.channel_id);
        let resp = self
            .http
            .post(&url)
            .header("Authorization", format!("Bot {}", self.token))
            .json(&serde_json::json!({ "content": content }))
            .send()
            .await
            .expect("failed to send Discord message");

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        assert!(
            status.is_success(),
            "Discord send_message failed ({status}): {body}"
        );
        serde_json::from_str(&body).expect("failed to parse Discord message response")
    }

    /// Poll the channel for a message whose `content` contains `needle`,
    /// looking only at messages after `after_id`. Returns the first match.
    /// Panics if `timeout` elapses without finding a match.
    pub async fn poll_messages_containing(
        &self,
        after_id: &str,
        needle: &str,
        timeout: Duration,
    ) -> DiscordMessage {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let url = format!(
                "{DISCORD_API}/channels/{}/messages?limit=10&after={after_id}",
                self.channel_id
            );
            let resp = self
                .http
                .get(&url)
                .header("Authorization", format!("Bot {}", self.token))
                .send()
                .await
                .expect("failed to poll Discord messages");

            if resp.status().is_success() {
                let messages: Vec<DiscordMessage> = resp.json().await.unwrap_or_default();
                if let Some(m) = messages.into_iter().find(|m| m.content.contains(needle)) {
                    return m;
                }
            }

            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out after {timeout:?} waiting for Discord message containing {needle:?}"
            );
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Get a recent message to use as the `after_id` anchor for polling.
    /// Returns the ID of the most recent message in the channel, or "0" if empty.
    pub async fn latest_message_id(&self) -> String {
        let url = format!(
            "{DISCORD_API}/channels/{}/messages?limit=1",
            self.channel_id
        );
        let resp = self
            .http
            .get(&url)
            .header("Authorization", format!("Bot {}", self.token))
            .send()
            .await
            .expect("failed to fetch latest Discord message");

        if resp.status().is_success() {
            let messages: Vec<DiscordMessage> = resp.json().await.unwrap_or_default();
            if let Some(m) = messages.first() {
                return m.id.clone();
            }
        }
        "0".to_string()
    }
}
