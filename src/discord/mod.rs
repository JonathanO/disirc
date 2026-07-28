pub mod connection;
mod handler;
pub(crate) mod send;
pub(crate) mod types;

pub(crate) use types::webhook_id_from_url;
pub use types::{DiscordCommand, DiscordEvent, DiscordPresence, MemberInfo};
