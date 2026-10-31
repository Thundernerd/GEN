//! Notifications: rule matching per channel and delivery to Discord, ntfy and email.

pub mod apprise;
pub mod discord;
pub mod email;
pub mod ntfy;

use chrono::{DateTime, Utc};

use crate::config::{AppConfig, ChannelConfig, ChannelTarget, NotifyRules, SourcesConfig};
use crate::fetch::Http;
use crate::model::{Item, ItemKind};
use crate::store::Store;

/// `meta` key set once the channels from gen.toml were imported.
const IMPORTED_KEY: &str = "channels_imported";

/// A configured channel with its last delivery result.
#[derive(Debug, Clone)]
pub struct Channel {
    pub config: ChannelConfig,
    pub last_ok: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub last_error_at: Option<DateTime<Utc>>,
}

/// The channels stored in the database. Unreadable rows are skipped with a warning.
pub fn load_channels(store: &Store) -> anyhow::Result<Vec<Channel>> {
    let mut out = Vec::new();
    for row in store.channels()? {
        match serde_json::from_str::<ChannelConfig>(&row.config) {
            Ok(config) => out.push(Channel {
                config,
                last_ok: row.last_ok,
                last_error: row.last_error,
                last_error_at: row.last_error_at,
            }),
            Err(err) => tracing::warn!(channel = %row.name, "skipping unreadable channel: {err}"),
        }
    }
    Ok(out)
}

/// Once per database: copies the channels from gen.toml into it. After that they're managed
/// on the website, and channels left in gen.toml are ignored.
pub fn import_once(app: &AppConfig, store: &Store) -> anyhow::Result<()> {
    let channels = &app.notify.channels;
    if store.meta(IMPORTED_KEY)?.is_some() {
        if !channels.is_empty() {
            tracing::info!(
                "notify channels in gen.toml are ignored; manage them on the Notifications page"
            );
        }
        return Ok(());
    }
    for channel in channels {
        let json = serde_json::to_string(channel)?;
        if store.import_channel(&channel.name, &json)? {
            tracing::info!(channel = %channel.name, "imported notify channel from gen.toml");
        } else {
            tracing::warn!(channel = %channel.name, "not imported: a channel with this name exists");
        }
    }
    store.set_meta(IMPORTED_KEY, &Utc::now().to_rfc3339())?;
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum NotifyError {
    #[error("http error: {0}")]
    Http(reqwest::Error),
    #[error("{0}")]
    Other(String),
}

/// Drops the URL from HTTP errors: webhook URLs and ntfy topics are secrets, and errors
/// are shown on the Notifications page.
impl From<reqwest::Error> for NotifyError {
    fn from(err: reqwest::Error) -> Self {
        NotifyError::Http(err.without_url())
    }
}

/// Context shared by notifiers for rendering messages.
pub struct NotifyContext<'a> {
    pub http: &'a Http,
    pub sources: &'a SourcesConfig,
    /// Public base URL of the GEN site, for "view on GEN" links.
    pub base_url: &'a str,
    /// Set when the Discord bot is configured, for bot channels.
    pub discord_bot: Option<&'a discord::BotApi>,
}

/// Whether `item` should be sent to a channel with these rules.
pub fn matches(rules: &NotifyRules, item: &Item) -> bool {
    if item.duplicate_of.is_some() || rules.exclude_kinds.contains(&item.kind) {
        return false;
    }
    if !rules.kinds.is_empty() && !rules.kinds.contains(&item.kind) {
        return false;
    }
    let engines_ok = if item.engines.is_empty() {
        rules.include_untagged
    } else {
        rules.engines.is_empty() || item.engines.iter().any(|e| rules.engines.contains(e))
    };
    if !engines_ok {
        return false;
    }
    if rules.keywords.is_empty() {
        return true;
    }
    let haystack = format!(
        "{} {}",
        item.title,
        item.summary.as_deref().unwrap_or_default()
    )
    .to_lowercase();
    rules
        .keywords
        .iter()
        .any(|k| haystack.contains(&k.to_lowercase()))
}

/// Whether `item` should be sent to `channel`: its rules match and, for a Discord bot
/// channel, one of its subscriptions does.
pub fn accepts(channel: &ChannelConfig, item: &Item) -> bool {
    if !matches(&channel.rules, item) {
        return false;
    }
    match &channel.target {
        ChannelTarget::DiscordBot { subscriptions, .. } => subscriptions
            .iter()
            .any(|s| item.engines.contains(&s.engine) && s.topic.kinds().contains(&item.kind)),
        _ => true,
    }
}

/// Sends `items` (already filtered by `accepts`) to one channel.
pub async fn send(
    ctx: &NotifyContext<'_>,
    channel: &ChannelConfig,
    items: &[Item],
) -> Result<(), NotifyError> {
    if items.is_empty() {
        return Ok(());
    }
    match &channel.target {
        ChannelTarget::Discord { webhook_url, style } => {
            discord::send(ctx, webhook_url, *style, items).await
        }
        ChannelTarget::DiscordBot {
            channel_id, style, ..
        } => discord::send_bot(ctx, *channel_id, *style, items).await,
        ChannelTarget::Ntfy {
            server,
            topic,
            token,
        } => ntfy::send(ctx, server, topic, token.as_deref(), items).await,
        ChannelTarget::Email { .. } => email::send(ctx, &channel.target, items).await,
        ChannelTarget::Apprise {
            server,
            key,
            urls,
            tag,
            username,
            password,
        } => {
            let target = apprise::Target {
                server,
                key: key.as_deref().filter(|k| !k.is_empty()),
                urls: urls.as_deref().filter(|u| !u.is_empty()),
                tag: tag.as_deref(),
                username: username.as_deref().filter(|u| !u.is_empty()),
                password: password.as_deref().filter(|p| !p.is_empty()),
            };
            apprise::send(ctx, &target, items).await
        }
    }
}

/// Human label for a kind, e.g. "Release".
pub fn kind_label(kind: ItemKind) -> &'static str {
    match kind {
        ItemKind::Release => "Release",
        ItemKind::Prerelease => "Prerelease",
        ItemKind::News => "News",
        ItemKind::Video => "Video",
        ItemKind::Community => "Community",
    }
}

/// Emoji shown in front of titles.
pub fn kind_emoji(kind: ItemKind) -> &'static str {
    match kind {
        ItemKind::Release => "🚀",
        ItemKind::Prerelease => "🧪",
        ItemKind::News => "📰",
        ItemKind::Video => "🎬",
        ItemKind::Community => "💬",
    }
}

/// Emoji shortcode for the kind (ntfy tags).
pub fn kind_shortcode(kind: ItemKind) -> &'static str {
    match kind {
        ItemKind::Release => "rocket",
        ItemKind::Prerelease => "test_tube",
        ItemKind::News => "newspaper",
        ItemKind::Video => "clapper",
        ItemKind::Community => "speech_balloon",
    }
}

/// Accent color as `0xRRGGBB`: green, amber, blue, red, purple.
pub fn kind_color(kind: ItemKind) -> u32 {
    match kind {
        ItemKind::Release => 0x2E_CC_71,
        ItemKind::Prerelease => 0xF5_A6_23,
        ItemKind::News => 0x34_98_DB,
        ItemKind::Video => 0xE7_4C_3C,
        ItemKind::Community => 0x9B_59_B6,
    }
}

/// Display names of the item's engines, falling back to the id for unknown engines.
pub fn engine_names(sources: &SourcesConfig, item: &Item) -> Vec<String> {
    item.engines
        .iter()
        .map(|id| {
            sources
                .engine(id)
                .map_or_else(|| id.clone(), |e| e.name.clone())
        })
        .collect()
}

/// `"[Godot] title"`, or just the title for untagged items.
pub fn prefixed_title(sources: &SourcesConfig, item: &Item) -> String {
    let names = engine_names(sources, item);
    if names.is_empty() {
        item.title.clone()
    } else {
        format!("[{}] {}", names.join(", "), item.title)
    }
}

/// An example notification, for trying out how a channel shows each kind of item.
pub struct Sample {
    pub key: &'static str,
    pub label: &'static str,
    pub item: Item,
}

/// Examples covering what changes how a message looks: the kind (emoji, colour, ntfy
/// priority), zero, one or several engines in the title prefix, with or without a summary
/// and a link, and text long enough to be cut short.
pub fn samples() -> Vec<Sample> {
    let now = Utc::now();
    let item = |key: &str,
                engines: &[&str],
                kind,
                title: &str,
                url: Option<&str>,
                summary: Option<&str>| Item {
        id: 0,
        source_id: format!("gen:test:{key}"),
        guid: format!("gen-notify-test-{key}"),
        engines: engines.iter().map(|e| e.to_string()).collect(),
        kind,
        title: title.into(),
        url: url.map(str::to_string),
        summary: summary.map(str::to_string),
        published: Some(now),
        first_seen: now,
        story_key: title.to_lowercase(),
        duplicate_of: None,
        image_url: None,
        image_checked: true,
    };
    let long_summary = "Two years in the making, this release reworks the renderer, adds a \
        new physics backend and brings C# hot reload to every platform. "
        .repeat(40);
    vec![
        Sample {
            key: "release",
            label: "Release, one engine",
            item: item(
                "release",
                &["godot"],
                ItemKind::Release,
                "Godot 4.8 released",
                Some("https://godotengine.org/releases/4.8/"),
                Some(
                    "Godot 4.8 is out, with improvements to C# support, rendering and the \
                      editor. This is a test notification from GEN.",
                ),
            ),
        },
        Sample {
            key: "prerelease",
            label: "Prerelease",
            item: item(
                "prerelease",
                &["unity"],
                ItemKind::Prerelease,
                "Unity 6000.4.0b2 (Beta)",
                Some("https://unity.com/releases/editor/beta"),
                Some("Unity beta release. This is a test notification from GEN."),
            ),
        },
        Sample {
            key: "news",
            label: "News post",
            item: item(
                "news",
                &["unreal"],
                ItemKind::News,
                "Unreal Engine 5.8 roadmap: what's coming next",
                Some("https://www.unrealengine.com/en-US/news"),
                Some(
                    "A look at the features planned for the next releases. This is a test \
                      notification from GEN.",
                ),
            ),
        },
        Sample {
            key: "outlet",
            label: "Outlet article, several engines",
            item: item(
                "outlet",
                &["godot", "unity"],
                ItemKind::News,
                "Godot vs. Unity for C# developers in 2026",
                Some("https://gamefromscratch.com/"),
                Some(
                    "An article from a news outlet that mentions two engines. This is a test \
                      notification from GEN.",
                ),
            ),
        },
        Sample {
            key: "untagged",
            label: "No engine, no summary, no link",
            item: item(
                "untagged",
                &[],
                ItemKind::News,
                "The state of open-source game engines (GEN test notification)",
                None,
                None,
            ),
        },
        Sample {
            key: "long",
            label: "Very long title and summary",
            item: item(
                "long",
                &["godot"],
                ItemKind::Release,
                &format!(
                    "Godot 5.0 released: {}",
                    "the biggest update yet, ".repeat(12)
                ),
                Some("https://godotengine.org/"),
                Some(&long_summary),
            ),
        },
    ]
}

/// The first example, as a base for test items.
#[cfg(test)]
pub fn sample_item() -> Item {
    samples().remove(0).item
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn item(engines: &[&str], kind: ItemKind, title: &str, summary: Option<&str>) -> Item {
        Item {
            engines: engines.iter().map(|e| e.to_string()).collect(),
            kind,
            title: title.into(),
            summary: summary.map(str::to_string),
            ..sample_item()
        }
    }

    pub fn sources() -> SourcesConfig {
        toml::from_str(
            r#"
            [[engine]]
            id = "godot"
            name = "Godot"
            tier = 1
            csharp = "first-class"
            [[engine]]
            id = "unity"
            name = "Unity"
            tier = 1
            csharp = "native"
            "#,
        )
        .unwrap()
    }

    fn rules(
        engines: &[&str],
        kinds: &[ItemKind],
        exclude: &[ItemKind],
        keywords: &[&str],
        untagged: bool,
    ) -> NotifyRules {
        NotifyRules {
            engines: engines.iter().map(|e| e.to_string()).collect(),
            kinds: kinds.to_vec(),
            exclude_kinds: exclude.to_vec(),
            keywords: keywords.iter().map(|k| k.to_string()).collect(),
            include_untagged: untagged,
        }
    }

    #[test]
    fn rule_table() {
        use ItemKind::*;
        let godot_rel = item(&["godot"], Release, "Godot 4.8", Some("C# goodies"));
        let unity_news = item(&["unity"], News, "Unity 6 roadmap", None);
        let untagged = item(&[], News, "Engine roundup", Some("Various engines"));
        let mut dup = godot_rel.clone();
        dup.duplicate_of = Some(1);

        let cases: Vec<(&str, NotifyRules, &Item, bool)> = vec![
            (
                "empty rules, tagged",
                rules(&[], &[], &[], &[], false),
                &godot_rel,
                true,
            ),
            (
                "empty rules, untagged",
                rules(&[], &[], &[], &[], false),
                &untagged,
                false,
            ),
            (
                "empty rules, untagged allowed",
                rules(&[], &[], &[], &[], true),
                &untagged,
                true,
            ),
            (
                "duplicate never",
                rules(&[], &[], &[], &[], true),
                &dup,
                false,
            ),
            (
                "engine listed",
                rules(&["godot"], &[], &[], &[], false),
                &godot_rel,
                true,
            ),
            (
                "engine not listed",
                rules(&["godot"], &[], &[], &[], false),
                &unity_news,
                false,
            ),
            (
                "engines + untagged",
                rules(&["godot"], &[], &[], &[], true),
                &untagged,
                true,
            ),
            (
                "engines, untagged denied",
                rules(&["godot"], &[], &[], &[], false),
                &untagged,
                false,
            ),
            (
                "kind listed",
                rules(&[], &[Release], &[], &[], false),
                &godot_rel,
                true,
            ),
            (
                "kind not listed",
                rules(&[], &[Release], &[], &[], false),
                &unity_news,
                false,
            ),
            (
                "kind excluded",
                rules(&[], &[], &[News], &[], true),
                &unity_news,
                false,
            ),
            (
                "excluded beats listed",
                rules(&[], &[News], &[News], &[], false),
                &unity_news,
                false,
            ),
            (
                "keyword in title",
                rules(&[], &[], &[], &["ROADMAP"], false),
                &unity_news,
                true,
            ),
            (
                "keyword in summary",
                rules(&[], &[], &[], &["c#"], false),
                &godot_rel,
                true,
            ),
            (
                "keyword missing",
                rules(&[], &[], &[], &["vulkan"], false),
                &godot_rel,
                false,
            ),
            (
                "any keyword",
                rules(&[], &[], &[], &["x", "4.8"], false),
                &godot_rel,
                true,
            ),
            (
                "all combined",
                rules(&["unity"], &[News], &[Video], &["unity"], false),
                &unity_news,
                true,
            ),
        ];
        for (name, rules, item, expected) in cases {
            assert_eq!(matches(&rules, item), expected, "{name}");
        }
    }

    #[test]
    fn bot_channels_get_their_subscriptions() {
        use crate::config::BotSubscription;
        use crate::model::Topic;
        use ItemKind::*;
        let sub = |engine: &str, topic| BotSubscription {
            engine: engine.into(),
            topic,
        };
        let channel = |subscriptions, rules| ChannelConfig {
            name: "discord-x".into(),
            enabled: true,
            rules,
            target: ChannelTarget::DiscordBot {
                guild_id: 1,
                channel_id: 2,
                guild_name: String::new(),
                channel_name: String::new(),
                subscriptions,
                created_by_bot: false,
                style: Default::default(),
            },
        };
        let unity_releases_godot_news = channel(
            vec![sub("unity", Topic::Releases), sub("godot", Topic::News)],
            NotifyRules::default(),
        );
        let cases = [
            (item(&["unity"], Release, "Unity 6.1", None), true),
            (item(&["unity"], Prerelease, "Unity 6.2b1", None), true),
            (item(&["unity"], News, "Unity roadmap", None), false),
            (item(&["godot"], News, "Godot showcase", None), true),
            (item(&["godot"], Release, "Godot 4.8", None), false),
            (item(&["godot", "unity"], Release, "Both", None), true),
            (item(&[], News, "Untagged", None), false),
            (item(&["bevy"], News, "Bevy 0.17", None), false),
        ];
        for (item, expected) in &cases {
            assert_eq!(
                accepts(&unity_releases_godot_news, item),
                *expected,
                "{}",
                item.title
            );
        }
        // The channel's rules still apply on top.
        let with_keywords = channel(
            vec![sub("unity", Topic::Releases)],
            rules(&[], &[], &[], &["lts"], false),
        );
        assert!(!accepts(&with_keywords, &cases[0].0));
        assert!(accepts(
            &with_keywords,
            &item(&["unity"], Release, "Unity 6 LTS", None)
        ));
        // Other channels go by their rules only.
        let webhook = ChannelConfig {
            target: ChannelTarget::Discord {
                webhook_url: "https://discord.com/api/webhooks/1/x".into(),
                style: Default::default(),
            },
            ..unity_releases_godot_news.clone()
        };
        assert!(accepts(&webhook, &cases[2].0));
    }

    #[tokio::test]
    async fn send_errors_leave_out_the_url() {
        let http = Http::new("gen-test", None).unwrap();
        let ctx = NotifyContext {
            http: &http,
            sources: &sources(),
            base_url: "https://gen.example.org/",
            discord_bot: None,
        };
        // Nothing listens on port 9 (discard) here, so the request fails.
        let channel: ChannelConfig = toml::from_str(
            r#"
            name = "phone"
            kind = "ntfy"
            server = "http://127.0.0.1:9"
            topic = "secret-topic"
            "#,
        )
        .unwrap();
        let err = send(&ctx, &channel, &[sample_item()])
            .await
            .unwrap_err()
            .to_string();
        assert!(!err.contains("secret-topic"), "{err}");
    }

    #[test]
    fn samples_cover_the_message_shapes() {
        let samples = samples();
        let keys: Vec<_> = samples.iter().map(|s| s.key).collect();
        assert_eq!(
            keys.len(),
            keys.iter().collect::<std::collections::HashSet<_>>().len()
        );
        let items: Vec<&Item> = samples.iter().map(|s| &s.item).collect();
        assert!(
            items
                .iter()
                .any(|i| i.engines.is_empty() && i.summary.is_none() && i.url.is_none())
        );
        assert!(items.iter().any(|i| i.engines.len() > 1));
        for kind in [ItemKind::Release, ItemKind::Prerelease, ItemKind::News] {
            assert!(items.iter().any(|i| i.kind == kind), "{kind:?}");
        }
        // The long one is cut short by every channel.
        let long = &samples.iter().find(|s| s.key == "long").unwrap().item;
        assert!(long.title.chars().count() > 256 && long.summary.as_ref().unwrap().len() > 4096);
        for style in crate::config::MessageStyle::ALL {
            let messages = discord::build_messages(
                &sources(),
                "https://gen.example.org/",
                style,
                std::slice::from_ref(long),
            );
            let payload = &messages[0].payload;
            let text = payload.to_string();
            assert!(text.chars().count() < 6000, "{style:?}");
            if let Some(title) = payload["embeds"][0]["title"].as_str() {
                assert!(title.chars().count() <= 256, "{style:?}");
            }
        }
    }

    #[test]
    fn imports_channels_from_gen_toml_once() {
        let mut app: AppConfig = toml::from_str(
            r#"
            [[notify.channel]]
            name = "phone"
            kind = "ntfy"
            topic = "${GEN_NTFY_TOPIC}"
            "#,
        )
        .unwrap();
        let store = Store::open_in_memory().unwrap();
        store.add_channel("chat", "{}").unwrap();
        import_once(&app, &store).unwrap();
        let channels = load_channels(&store).unwrap();
        // "chat" isn't valid channel JSON, so only the import is readable.
        assert_eq!(channels.len(), 1);
        assert_eq!(
            channels[0].config, app.notify.channels[0],
            "references kept"
        );

        // Later changes to gen.toml are ignored.
        app.notify.channels[0].name = "other".into();
        import_once(&app, &store).unwrap();
        assert_eq!(load_channels(&store).unwrap().len(), 1);
    }

    #[test]
    fn engine_names_fall_back_to_id() {
        let i = item(&["godot", "mystery"], ItemKind::News, "t", None);
        assert_eq!(engine_names(&sources(), &i), vec!["Godot", "mystery"]);
        assert_eq!(prefixed_title(&sources(), &i), "[Godot, mystery] t");
        let u = item(&[], ItemKind::News, "t", None);
        assert_eq!(prefixed_title(&sources(), &u), "t");
    }
}
