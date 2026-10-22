//! Notifications: rule matching per channel and delivery to Discord, ntfy and email.

pub mod discord;
pub mod email;
pub mod ntfy;

use crate::config::{ChannelConfig, ChannelTarget, NotifyRules, SourcesConfig};
use crate::fetch::Http;
use crate::model::Item;

#[derive(Debug, thiserror::Error)]
pub enum NotifyError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("{0}")]
    Other(String),
}

/// Context shared by notifiers for rendering messages.
pub struct NotifyContext<'a> {
    pub http: &'a Http,
    pub sources: &'a SourcesConfig,
    /// Public base URL of the GEN site, for "view on GEN" links.
    pub base_url: &'a str,
}

/// Whether `item` should be sent to a channel with these rules.
pub fn matches(_rules: &NotifyRules, _item: &Item) -> bool {
    false
}

/// Sends `items` (already filtered by `matches`) to one channel.
pub async fn send(
    ctx: &NotifyContext<'_>,
    channel: &ChannelConfig,
    items: &[Item],
) -> Result<(), NotifyError> {
    match &channel.target {
        ChannelTarget::Discord { webhook_url } => discord::send(ctx, webhook_url, items).await,
        ChannelTarget::Ntfy {
            server,
            topic,
            token,
        } => ntfy::send(ctx, server, topic, token.as_deref(), items).await,
        ChannelTarget::Email { .. } => email::send(ctx, &channel.target, items).await,
    }
}
