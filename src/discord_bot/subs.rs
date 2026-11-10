//! Bot subscriptions, stored as notification channels: one `notify_channels` row per
//! Discord channel, whose target lists that channel's engine topics.

use crate::config::{BotSubscription, ChannelConfig, ChannelTarget, MessageStyle, NotifyRules};
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
    pub style: MessageStyle,
}

impl BotChannel {
    fn from_channel(c: &Channel) -> Option<BotChannel> {
        match &c.config.target {
            ChannelTarget::DiscordBot {
                guild_id,
                channel_id,
                subscriptions,
                style,
                ..
            } => Some(BotChannel {
                name: c.config.name.clone(),
                guild_id: *guild_id,
                channel_id: *channel_id,
                subscriptions: subscriptions.clone(),
                style: *style,
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

/// Adds subscriptions to a Discord channel, creating its row if needed, and sets its
/// style when given (a new row otherwise gets the server's default). Returns the
/// subscriptions that are new.
pub fn subscribe(
    store: &Store,
    channel: &DiscordChannel,
    wanted: &[BotSubscription],
    style: Option<MessageStyle>,
) -> anyhow::Result<Vec<BotSubscription>> {
    match find_config(store, channel.channel_id)? {
        Some(mut config) => {
            let ChannelTarget::DiscordBot {
                subscriptions,
                style: current,
                ..
            } = &mut config.target
            else {
                unreachable!("find_config only returns bot channels");
            };
            let mut added = Vec::new();
            for sub in wanted {
                if !subscriptions.contains(sub) {
                    subscriptions.push(sub.clone());
                    added.push(sub.clone());
                }
            }
            let restyled = style.is_some_and(|s| s != *current);
            if let Some(style) = style {
                *current = style;
            }
            if !added.is_empty() || restyled {
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
                health_alerts: false,
                target: ChannelTarget::DiscordBot {
                    guild_id: channel.guild_id,
                    channel_id: channel.channel_id,
                    guild_name: channel.guild_name.clone(),
                    channel_name: channel.channel_name.clone(),
                    subscriptions: subscriptions.clone(),
                    created_by_bot: channel.created_by_bot,
                    style: match style {
                        Some(style) => style,
                        None => default_style(store, channel.guild_id)?,
                    },
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

/// Sets the style of the server's bot channels picked by `which`. Returns how many there
/// are.
pub fn set_style(
    store: &Store,
    guild_id: u64,
    which: impl Fn(&BotChannel) -> bool,
    style: MessageStyle,
) -> anyhow::Result<usize> {
    let mut count = 0;
    for channel in notify::load_channels(store)? {
        let picked =
            BotChannel::from_channel(&channel).is_some_and(|c| c.guild_id == guild_id && which(&c));
        if !picked {
            continue;
        }
        let mut config = channel.config;
        if let ChannelTarget::DiscordBot { style: current, .. } = &mut config.target
            && *current != style
        {
            *current = style;
            store.update_channel(&config.name, &serde_json::to_string(&config)?)?;
        }
        count += 1;
    }
    Ok(count)
}

fn style_key(guild_id: u64) -> String {
    format!("discord_style:{guild_id}")
}

/// The style new channels on a server get.
pub fn default_style(store: &Store, guild_id: u64) -> anyhow::Result<MessageStyle> {
    Ok(store
        .meta(&style_key(guild_id))?
        .and_then(|s| MessageStyle::parse(&s))
        .unwrap_or_default())
}

pub fn set_default_style(store: &Store, guild_id: u64, style: MessageStyle) -> anyhow::Result<()> {
    store.set_meta(&style_key(guild_id), style.key())
}

/// Forgets a Discord channel (it was deleted). Returns whether it had a row.
pub fn forget_channel(store: &mut Store, channel_id: u64) -> anyhow::Result<bool> {
    forget(store, |c| c.channel_id == channel_id)
}

/// Forgets every channel of a server (the bot was removed from it), and its default style.
pub fn forget_guild(store: &mut Store, guild_id: u64) -> anyhow::Result<bool> {
    store.delete_meta(&style_key(guild_id))?;
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
        let added = subscribe(&store, &news, &[sub("unity", Topic::News)], None).unwrap();
        assert_eq!(added, [sub("unity", Topic::News)]);
        // Again, plus one more: only the new one is added, to the same row.
        let added = subscribe(
            &store,
            &news,
            &[sub("unity", Topic::News), sub("godot", Topic::News)],
            None,
        )
        .unwrap();
        assert_eq!(added, [sub("godot", Topic::News)]);
        subscribe(
            &store,
            &channel(1, 11, "unity-releases"),
            &[sub("unity", Topic::Releases)],
            None,
        )
        .unwrap();
        subscribe(
            &store,
            &channel(2, 20, "news"),
            &[sub("unity", Topic::News)],
            None,
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

    #[test]
    fn sets_styles_and_server_defaults() {
        let mut store = Store::open_in_memory().unwrap();
        let style = |store: &Store, guild| -> Vec<MessageStyle> {
            in_guild(store, guild)
                .unwrap()
                .iter()
                .map(|c| c.style)
                .collect()
        };
        subscribe(
            &store,
            &channel(1, 10, "a"),
            &[sub("unity", Topic::News)],
            None,
        )
        .unwrap();
        subscribe(
            &store,
            &channel(1, 11, "b"),
            &[sub("godot", Topic::News)],
            None,
        )
        .unwrap();
        subscribe(
            &store,
            &channel(2, 20, "c"),
            &[sub("unity", Topic::News)],
            None,
        )
        .unwrap();
        assert_eq!(style(&store, 1), [MessageStyle::Card, MessageStyle::Card]);

        // One engine's channels, then the whole server; other servers stay as they are.
        let n = set_style(
            &store,
            1,
            |c| c.subscriptions.iter().any(|s| s.engine == "unity"),
            MessageStyle::Compact,
        )
        .unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            style(&store, 1),
            [MessageStyle::Compact, MessageStyle::Card]
        );
        assert_eq!(
            set_style(&store, 1, |_| true, MessageStyle::Image).unwrap(),
            2
        );
        assert_eq!(style(&store, 1), [MessageStyle::Image, MessageStyle::Image]);
        assert_eq!(style(&store, 2), [MessageStyle::Card]);

        // New channels get the server default; a style given when subscribing wins.
        set_default_style(&store, 1, MessageStyle::Polished).unwrap();
        assert_eq!(default_style(&store, 1).unwrap(), MessageStyle::Polished);
        assert_eq!(default_style(&store, 2).unwrap(), MessageStyle::Card);
        subscribe(
            &store,
            &channel(1, 12, "d"),
            &[sub("unity", Topic::Videos)],
            None,
        )
        .unwrap();
        subscribe(&store, &channel(1, 10, "a"), &[], Some(MessageStyle::Card)).unwrap();
        assert_eq!(
            style(&store, 1),
            [
                MessageStyle::Card,
                MessageStyle::Image,
                MessageStyle::Polished
            ]
        );

        forget_guild(&mut store, 1).unwrap();
        assert_eq!(default_style(&store, 1).unwrap(), MessageStyle::Card);
    }
}
