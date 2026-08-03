#![deny(unsafe_code)]

pub mod bridge;
pub mod config;
pub mod discord;
pub(crate) mod formatting;
pub mod irc;
pub(crate) mod persist;
pub(crate) mod pseudoclients;
pub mod signal;

#[cfg(test)]
pub(crate) mod test_util;
