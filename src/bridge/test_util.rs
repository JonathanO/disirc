#[cfg(test)]
pub(crate) struct NullResolver;

#[cfg(test)]
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

#[cfg(test)]
pub(crate) struct NullIrcResolver;

#[cfg(test)]
impl crate::formatting::IrcMentionResolver for NullIrcResolver {
    fn resolve_nick(&self, _: &str) -> Option<String> {
        None
    }
}
