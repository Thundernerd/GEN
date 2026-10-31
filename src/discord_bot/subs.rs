//! Bot subscriptions, stored as notification channels: one `notify_channels` row per
//! Discord channel, whose target lists that channel's engine topics.

use crate::config::{BotSubscription, ChannelConfig, ChannelTarget, NotifyRules};
use crate::notify::{self, Channel};
use crate::store::Store;

/// A Discord channel to subscribe.
#[derive(Debug, Clone)]
pub struct DiscordChannel {
    pub guild_id: u64,
    pub guild_name: String,
    pub channel_id: u64,
    pub channel_name: String,
    /// The bot just created it.
    pub created_by_bot: bool,
}

/// A bot channel row, with its Discord ids pulled out.
#[derive(Debug, Clone)]
pub struct BotChannel {
    pub name: String,
    pub guild_id: u64,
    pub channel_id: u64,
    pub subscriptions: Vec<BotSubscription>,
}

impl BotChannel {
    fn from_channel(c: &Channel) -> Option<BotChannel> {
        match &c.config.target {
            ChannelTarget::DiscordBot {
                guild_id,
                channel_id,
                subscriptions,
                ..
            } => Some(BotChannel {
                name: c.config.name.clone(),
                guild_id: *guild_id,
                channel_id: *channel_id,
                subscriptions: subscriptions.clone(),
            }),
            _ => None,
        }
    }
}

/// Every bot channel of a server.
pub fn in_guild(store: &Store, guild_id: u64) -> anyhow::Result<Vec<BotChannel>> {
    Ok(bot_channels(store)?
        .into_iter()
        .filter(|c| c.guild_id == guild_id)
        .collect())
}

fn bot_channels(store: &Store) -> anyhow::Result<Vec<BotChannel>> {
    Ok(notify::load_channels(store)?
        .iter()
        .filter_map(BotChannel::from_channel)
        .collect())
}

fn find_config(store: &Store, channel_id: u64) -> anyhow::Result<Option<ChannelConfig>> {
    Ok(notify::load_channels(store)?
        .into_iter()
        .map(|c| c.config)
        .find(|c| {
            matches!(c.target, ChannelTarget::DiscordBot { channel_id: id, .. } if id == channel_id)
        }))
}

/// Adds subscriptions to a Discord channel, creating its row if needed. Returns the ones
/// that are new.
pub fn subscribe(
    store: &Store,
    channel: &DiscordChannel,
    wanted: &[BotSubscription],
) -> anyhow::Result<Vec<BotSubscription>> {
    match find_config(store, channel.channel_id)? {
        Some(mut config) => {
            let ChannelTarget::DiscordBot { subscriptions, .. } = &mut config.target else {
                unreachable!("find_config only returns bot channels");
            };
            let mut added = Vec::new();
            for sub in wanted {
                if !subscriptions.contains(sub) {
                    subscriptions.push(sub.clone());
                    added.push(sub.clone());
                }
            }
            if !added.is_empty() {
                store.update_channel(&config.name, &serde_json::to_string(&config)?)?;
            }
            Ok(added)
        }
        None => {
            let mut subscriptions: Vec<BotSubscription> = Vec::new();
            for sub in wanted {
                if !subscriptions.contains(sub) {
                    subscriptions.push(sub.clone());
                }
            }
            if subscriptions.is_empty() {
                return Ok(Vec::new());
            }
            let taken: Vec<String> = notify::load_channels(store)?
                .into_iter()
                .map(|c| c.config.name)
                .collect();
            let config = ChannelConfig {
                name: row_name(&channel.channel_name, &taken),
                enabled: true,
                rules: NotifyRules::default(),
                target: ChannelTarget::DiscordBot {
                    guild_id: channel.guild_id,
                    channel_id: channel.channel_id,
                    guild_name: channel.guild_name.clone(),
                    channel_name: channel.channel_name.clone(),
                    subscriptions: subscriptions.clone(),
                    created_by_bot: channel.created_by_bot,
                },
            };
            store.add_channel(&config.name, &serde_json::to_string(&config)?)?;
            tracing::info!(channel = %config.name, "discord bot channel added");
            Ok(subscriptions)
        }
    }
}

/// What unsubscribing took away from one Discord channel.
#[derive(Debug, Clone, PartialEq)]
pub struct Removed {
    pub channel_id: u64,
    pub channel_name: String,
    pub subscriptions: Vec<BotSubscription>,
    /// Nothing is left, so the row was removed.
    pub emptied: bool,
    pub created_by_bot: bool,
}

/// Removes the subscriptions picked by `which` from the server's channels (or only from
/// `channel_id`). Channels left without subscriptions are removed.
pub fn unsubscribe(
    store: &mut Store,
    guild_id: u64,
    channel_id: Option<u64>,
    which: impl Fn(&BotSubscription) -> bool,
) -> anyhow::Result<Vec<Removed>> {
    let mut out = Vec::new();
    for channel in notify::load_channels(store)? {
        let mut config = channel.config;
        let ChannelTarget::DiscordBot {
            guild_id: g,
            channel_id: c,
            channel_name,
            subscriptions,
            created_by_bot,
            ..
        } = &mut config.target
        else {
            continue;
        };
        if *g != guild_id || channel_id.is_some_and(|id| id != *c) {
            continue;
        }
        let (gone, kept): (Vec<_>, Vec<_>) = subscriptions.drain(..).partition(|s| which(s));
        *subscriptions = kept;
        if gone.is_empty() {
            continue;
        }
        let removed = Removed {
            channel_id: *c,
            channel_name: channel_name.clone(),
            subscriptions: gone,
            emptied: subscriptions.is_empty(),
            created_by_bot: *created_by_bot,
        };
        if removed.emptied {
            store.remove_channel(&config.name)?;
            tracing::info!(channel = %config.name, "discord bot channel removed");
        } else {
            store.update_channel(&config.name, &serde_json::to_string(&config)?)?;
        }
        out.push(removed);
    }
    Ok(out)
}

/// Forgets a Discord channel (it was deleted). Returns whether it had a row.
pub fn forget_channel(store: &mut Store, channel_id: u64) -> anyhow::Result<bool> {
    forget(store, |c| c.channel_id == channel_id)
}

/// Forgets every channel of a server (the bot was removed from it).
pub fn forget_guild(store: &mut Store, guild_id: u64) -> anyhow::Result<bool> {
    forget(store, |c| c.guild_id == guild_id)
}

fn forget(store: &mut Store, which: impl Fn(&BotChannel) -> bool) -> anyhow::Result<bool> {
    let gone: Vec<_> = bot_channels(store)?.into_iter().filter(which).collect();
    for c in &gone {
        store.remove_channel(&c.name)?;
        tracing::info!(channel = %c.name, "discord bot channel forgotten");
    }
    Ok(!gone.is_empty())
}

/// A free row name for a Discord channel, e.g. `discord-unity-news`: lowercase letters,
/// digits and dashes, at most 40 long, with a number added when taken.
fn row_name(channel_name: &str, taken: &[String]) -> String {
    let mut slug = String::new();
    for c in channel_name.chars().flat_map(char::to_lowercase) {
        let c = if c.is_ascii_alphanumeric() { c } else { '-' };
        if c != '-' || !(slug.is_empty() || slug.ends_with('-')) {
            slug.push(c);
        }
    }
    let base = format!("discord-{}", slug.trim_end_matches('-'));
    let base = base.trim_end_matches('-');
    (1..)
        .map(|n| {
            let suffix = if n == 1 {
                String::new()
            } else {
                format!("-{n}")
            };
            let room = 40 - suffix.len();
            let stem: String = base.chars().take(room).collect();
            format!("{}{suffix}", stem.trim_end_matches('-'))
        })
        .find(|name| !taken.contains(name))
        .expect("some number is free")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::valid_owner_id;
    use crate::model::Topic;

    fn sub(engine: &str, topic: Topic) -> BotSubscription {
        BotSubscription {
            engine: engine.into(),
            topic,
        }
    }

    fn channel(guild_id: u64, channel_id: u64, name: &str) -> DiscordChannel {
        DiscordChannel {
            guild_id,
            guild_name: "Test server".into(),
            channel_id,
            channel_name: name.into(),
            created_by_bot: true,
        }
    }

    #[test]
    fn row_names_are_valid_and_unique() {
        assert_eq!(row_name("unity-news", &[]), "discord-unity-news");
        assert_eq!(row_name("Ünity  News!", &[]), "discord-nity-news");
        let taken = vec!["discord-unity-news".to_string()];
        assert_eq!(row_name("unity-news", &taken), "discord-unity-news-2");
        let long = row_name(&"x".repeat(80), &[]);
        assert_eq!(long.len(), 40);
        for name in [
            long,
            row_name("---", &[]),
            row_name(&"y".repeat(80), &[format!("discord-{}", "y".repeat(32))]),
        ] {
            assert!(valid_owner_id(&name), "{name}");
        }
    }

    #[test]
    fn subscribes_unsubscribes_and_forgets() {
        let mut store = Store::open_in_memory().unwrap();
        let news = channel(1, 10, "unity-news");
        let added = subscribe(&store, &news, &[sub("unity", Topic::News)]).unwrap();
        assert_eq!(added, [sub("unity", Topic::News)]);
        // Again, plus one more: only the new one is added, to the same row.
        let added = subscribe(
            &store,
            &news,
            &[sub("unity", Topic::News), sub("godot", Topic::News)],
        )
        .unwrap();
        assert_eq!(added, [sub("godot", Topic::News)]);
        subscribe(
            &store,
            &channel(1, 11, "unity-releases"),
            &[sub("unity", Topic::Releases)],
        )
        .unwrap();
        subscribe(
            &store,
            &channel(2, 20, "news"),
            &[sub("unity", Topic::News)],
        )
        .unwrap();

        let rows = in_guild(&store, 1).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "discord-unity-news");
        assert_eq!(rows[0].subscriptions.len(), 2);
        for c in notify::load_channels(&store).unwrap() {
            assert_eq!(c.config.check(), Ok(()), "{}", c.config.name);
        }

        // Removing Unity from server 1 empties #unity-releases only.
        let removed = unsubscribe(&mut store, 1, None, |s| s.engine == "unity").unwrap();
        assert_eq!(removed.len(), 2);
        assert!(!removed[0].emptied && removed[1].emptied);
        let rows = in_guild(&store, 1).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].subscriptions, [sub("godot", Topic::News)]);
        assert_eq!(
            in_guild(&store, 2).unwrap().len(),
            1,
            "other server untouched"
        );

        // Limited to one channel.
        let removed = unsubscribe(&mut store, 1, Some(99), |_| true).unwrap();
        assert!(removed.is_empty());

        assert!(forget_channel(&mut store, 10).unwrap());
        assert!(!forget_channel(&mut store, 10).unwrap());
        assert!(forget_guild(&mut store, 2).unwrap());
        assert!(notify::load_channels(&store).unwrap().is_empty());
    }
}
