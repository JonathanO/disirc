//! Shared no-op resolvers for bridge tests.  The module itself is declared
//! `#[cfg(test)]` in `bridge/mod.rs`.

pub(crate) struct NullResolver;

impl crate::formatting::DiscordResolver for NullResolver {
    fn resolve_user(&self, _: &str) -> Option<String> {
        None
    }
    fn resolve_channel(&self, _: &str) -> Option<String> {
        None
    }
    fn resolve_role(&self, _: &str) -> Option<String> {
        None
    }
}

pub(crate) struct NullIrcResolver;

impl crate::formatting::IrcMentionResolver for NullIrcResolver {
    fn resolve_nick(&self, _: &str) -> Option<String> {
        None
    }
}
