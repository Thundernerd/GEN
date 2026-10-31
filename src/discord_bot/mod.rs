//! The GEN Discord bot. It connects to the gateway for slash commands, with which server
//! admins subscribe channels to engine topics (`/gen setup` also creates the channels).
//! Subscriptions are notification channels (see [`subs`]); the notifier posts to them.

mod commands;
mod layout;
mod subs;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use serenity::all::{
    ChannelType, ConnectionStage, Context, EventHandler, GatewayIntents, GuildChannel, Interaction,
    Ready, ShardStageUpdateEvent, UnavailableGuild,
};
use serenity::async_trait;

use crate::config::AppConfig;
use crate::pipeline::Pipeline;

/// Permissions the bot asks for when invited: View Channels, Send Messages, Embed Links and
/// Manage Channels (to create the channels `/gen setup` sets up).
const INVITE_PERMISSIONS: u64 = (1 << 10) | (1 << 11) | (1 << 14) | (1 << 4);
/// Longest wait before reconnecting after the client stopped with an error.
const MAX_RESTART_WAIT: Duration = Duration::from_secs(600);
/// A client that ran at least this long starts over with a short wait.
const STABLE_RUN: Duration = Duration::from_secs(300);

/// How the bot is doing, shown on the Notifications page.
#[derive(Debug, Clone, Default)]
pub struct BotStatus {
    pub connected: bool,
    /// The bot's user name, once logged in.
    pub user: Option<String>,
    pub application_id: Option<u64>,
    /// Servers the bot is in.
    pub guilds: usize,
    pub last_error: Option<String>,
}

impl BotStatus {
    /// The link to add the bot to a server, once its application id is known.
    pub fn invite_url(&self) -> Option<String> {
        self.application_id.map(|id| {
            format!(
                "https://discord.com/oauth2/authorize?client_id={id}\
                 &scope=bot+applications.commands&permissions={INVITE_PERMISSIONS}"
            )
        })
    }
}

pub type BotStatusHandle = Arc<RwLock<BotStatus>>;

fn update(status: &BotStatusHandle, f: impl FnOnce(&mut BotStatus)) {
    if let Ok(mut s) = status.write() {
        f(&mut s);
    }
}

/// Starts the bot when gen.toml has a `[discord]` token. It reconnects by itself, so the
/// task only ends when aborted.
pub fn spawn(
    pipeline: Pipeline,
    app: &AppConfig,
    status: BotStatusHandle,
) -> Option<tokio::task::JoinHandle<()>> {
    let discord = app.discord.clone().filter(|d| !d.token.is_empty())?;
    Some(tokio::spawn(async move {
        let mut wait = Duration::from_secs(5);
        loop {
            let handler = Handler {
                pipeline: pipeline.clone(),
                status: status.clone(),
                dev_guild_id: discord.dev_guild_id,
                guilds: Mutex::default(),
                panels: commands::Panels::default(),
            };
            let started = std::time::Instant::now();
            let result = async {
                let mut client = serenity::Client::builder(&discord.token, GatewayIntents::GUILDS)
                    .event_handler(handler)
                    .await?;
                client.start().await
            }
            .await;
            if started.elapsed() >= STABLE_RUN {
                wait = Duration::from_secs(5);
            }
            let error = match result {
                Ok(()) => "the connection closed".to_string(),
                Err(err) => err.to_string(),
            };
            tracing::error!("discord bot stopped: {error}; reconnecting in {wait:?}");
            update(&status, |s| {
                s.connected = false;
                s.last_error = Some(error);
            });
            tokio::time::sleep(wait).await;
            wait = (wait * 2).min(MAX_RESTART_WAIT);
        }
    }))
}

struct Handler {
    pipeline: Pipeline,
    status: BotStatusHandle,
    dev_guild_id: Option<u64>,
    /// Names of the servers the bot is in, by id.
    guilds: Mutex<HashMap<u64, String>>,
    panels: commands::Panels,
}

impl Handler {
    fn guild_name(&self, id: u64) -> String {
        self.guilds
            .lock()
            .ok()
            .and_then(|g| g.get(&id).cloned())
            .unwrap_or_default()
    }

    fn count_guilds(&self) {
        let count = self.guilds.lock().map(|g| g.len()).unwrap_or_default();
        update(&self.status, |s| s.guilds = count);
    }

    /// Runs a store change and wakes the notifier, logging failures.
    async fn change<T: Send + 'static>(
        &self,
        what: &str,
        f: impl FnOnce(&mut crate::store::Store) -> anyhow::Result<T> + Send + 'static,
    ) -> anyhow::Result<T> {
        let result = self.pipeline.with_store(f).await;
        match &result {
            Ok(_) => self.pipeline.notify_wake.notify_one(),
            Err(err) => tracing::error!("discord bot: {what} failed: {err:#}"),
        }
        result
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        tracing::info!(user = %ready.user.name, guilds = ready.guilds.len(), "discord bot connected");
        update(&self.status, |s| {
            s.connected = true;
            s.user = Some(ready.user.name.clone());
            s.application_id = Some(ready.application.id.get());
            s.guilds = ready.guilds.len();
            s.last_error = None;
        });
        if let Err(err) = commands::register(&ctx, self.dev_guild_id).await {
            tracing::error!("registering discord commands failed: {err:#}");
            update(&self.status, |s| {
                s.last_error = Some(format!("registering commands failed: {err}"));
            });
        }
    }

    async fn shard_stage_update(&self, _ctx: Context, event: ShardStageUpdateEvent) {
        let connected = event.new == ConnectionStage::Connected;
        update(&self.status, |s| s.connected = connected);
    }

    async fn guild_create(&self, _ctx: Context, guild: serenity::all::Guild, _new: Option<bool>) {
        if let Ok(mut g) = self.guilds.lock() {
            g.insert(guild.id.get(), guild.name.clone());
        }
        self.count_guilds();
    }

    async fn guild_delete(
        &self,
        _ctx: Context,
        guild: UnavailableGuild,
        _full: Option<serenity::all::Guild>,
    ) {
        // `unavailable` means an outage, not that the bot was removed.
        if guild.unavailable {
            return;
        }
        let id = guild.id.get();
        if let Ok(mut g) = self.guilds.lock() {
            g.remove(&id);
        }
        self.count_guilds();
        let _ = self
            .change("forgetting a server", move |s| subs::forget_guild(s, id))
            .await;
    }

    async fn channel_delete(
        &self,
        _ctx: Context,
        channel: GuildChannel,
        _messages: Option<Vec<serenity::all::Message>>,
    ) {
        if channel.kind == ChannelType::Category {
            return;
        }
        let id = channel.id.get();
        let _ = self
            .change("forgetting a channel", move |s| subs::forget_channel(s, id))
            .await;
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        commands::handle(self, &ctx, interaction).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invite_url_asks_for_the_needed_permissions() {
        assert_eq!(BotStatus::default().invite_url(), None);
        let status = BotStatus {
            application_id: Some(42),
            ..BotStatus::default()
        };
        let url = status.invite_url().unwrap();
        assert!(
            url.starts_with("https://discord.com/oauth2/authorize?client_id=42&"),
            "{url}"
        );
        assert!(url.ends_with("&permissions=19472"), "{url}");
    }
}
