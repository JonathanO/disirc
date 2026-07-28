mod connect;
mod connection;
pub(crate) mod framing;
pub(crate) mod irc_message;
mod translation;

pub use connection::run_connection;
pub(crate) use irc_message::{IrcCommand, IrcMessage};
