//! Configuration: engines, outlets and their sources (stored in the database, seeded from
//! the built-in catalog) and `gen.toml` (app settings).

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

use crate::model::{EngineId, ItemKind, Topic};

// ---------------------------------------------------------------------------
// Engines, outlets and sources
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourcesConfig {
    #[serde(default, rename = "engine")]
    pub engines: Vec<EngineConfig>,
    #[serde(default, rename = "outlet")]
    pub outlets: Vec<OutletConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(try_from = "StoredEngineConfig")]
pub struct EngineConfig {
    pub id: EngineId,
    pub name: String,
    pub tier: Tier,
    /// The languages games are written in with this engine, primary first, e.g.
    /// `["GDScript", "C#", "C++"]`. Empty only for engines stored before this field existed
    /// that aren't in the catalog.
    pub languages: Vec<String>,
    /// Nuance the list can't carry, e.g. "C# via the UnrealSharp plugin".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language_note: Option<String>,
    pub homepage: Option<String>,
    /// Logo shown next to the engine's name in Discord messages: a PNG, JPEG or WebP URL
    /// (Discord shows no SVG or ICO).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    pub enabled: bool,
    /// Whole-word phrases that tag outlet items with this engine. All-lowercase aliases match
    /// in any case; an alias with an uppercase letter (e.g. "Unity") matches case-sensitively.
    pub aliases: Vec<String>,
    /// Phrases that veto an alias match (e.g. "ubuntu unity").
    pub exclude: Vec<String>,
    #[serde(rename = "source")]
    pub sources: Vec<SourceConfig>,
}

/// How prominent and active an engine is and whether it's a full engine, not which language
/// it uses. Stored and written in `catalog.toml` as its number, 1, 2 or 3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "u8", into = "u8")]
pub enum Tier {
    /// Widely used or very active full engines.
    Major = 1,
    /// Niche, young or less active full engines.
    Smaller = 2,
    /// Frameworks, bindings and libraries for physics, ECS, networking, UI and more.
    Frameworks = 3,
}

const TIER_OUT_OF_RANGE: &str = "The tier must be 1, 2 or 3.";

impl Tier {
    /// Every tier, in order.
    pub const ALL: [Tier; 3] = [Tier::Major, Tier::Smaller, Tier::Frameworks];

    /// The tier's heading, e.g. on the Engines page.
    pub fn label(self) -> &'static str {
        match self {
            Tier::Major => "Major engines",
            Tier::Smaller => "Smaller engines",
            Tier::Frameworks => "Frameworks and libraries",
        }
    }
}

impl From<Tier> for u8 {
    fn from(tier: Tier) -> u8 {
        tier as u8
    }
}

impl TryFrom<u8> for Tier {
    type Error = String;

    fn try_from(n: u8) -> Result<Self, Self::Error> {
        Tier::ALL
            .into_iter()
            .find(|&t| u8::from(t) == n)
            .ok_or_else(|| TIER_OUT_OF_RANGE.into())
    }
}

/// How [`EngineConfig`] is read: also accepts engines stored before `languages` existed,
/// whose free-text `csharp` field becomes the `language_note`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredEngineConfig {
    id: EngineId,
    name: String,
    tier: Tier,
    #[serde(default)]
    languages: Vec<String>,
    #[serde(default)]
    language_note: Option<String>,
    #[serde(default)]
    csharp: Option<String>,
    homepage: Option<String>,
    #[serde(default)]
    icon: Option<String>,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default, rename = "source")]
    sources: Vec<SourceConfig>,
}

impl TryFrom<StoredEngineConfig> for EngineConfig {
    type Error = String;

    fn try_from(e: StoredEngineConfig) -> Result<Self, Self::Error> {
        let language_note = match e.csharp {
            None => e.language_note,
            Some(_) if !e.languages.is_empty() || e.language_note.is_some() => {
                return Err(format!(
                    "Engine “{}”: `csharp` is replaced by `languages` and `language_note`; \
                     use only those.",
                    e.id
                ));
            }
            Some(csharp) => Some(csharp),
        };
        Ok(EngineConfig {
            id: e.id,
            name: e.name,
            tier: e.tier,
            languages: e.languages,
            language_note,
            homepage: e.homepage,
            icon: e.icon,
            enabled: e.enabled,
            aliases: e.aliases,
            exclude: e.exclude,
            sources: e.sources,
        })
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OutletConfig {
    pub id: String,
    pub name: String,
    #[allow(dead_code)] // accepted in config for documentation; not shown in the UI yet
    pub homepage: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Drop items that match no engine alias.
    #[serde(default)]
    pub require_engine_match: bool,
    #[serde(default, rename = "source")]
    pub sources: Vec<SourceConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct SourceConfig {
    /// Default kind for items from this source; the classifier may refine it
    /// (e.g. `release` -> `prerelease`).
    pub category: Option<ItemKind>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Overrides `[polling] default_interval_minutes`.
    pub interval_minutes: Option<u64>,
    /// Only keep items whose title/summary contains one of these (case-insensitive).
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(flatten)]
    pub spec: SourceSpec,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum SourceSpec {
    /// RSS / Atom / JSON Feed.
    Feed {
        url: String,
        /// Append a cache-busting query param and send `Cache-Control: no-cache`.
        #[serde(default)]
        cache_bust: bool,
    },
    /// GitHub repository releases/tags/commits.
    Github {
        /// `owner/repo`
        repo: String,
        #[serde(default)]
        mode: GithubMode,
        /// Only keep tags starting with one of these prefixes (e.g. Stride's `releases/`).
        /// Written as one string or a list; empty entries are dropped, and no prefixes
        /// keep every tag.
        #[serde(default, with = "tag_prefixes")]
        tag_prefix: Vec<String>,
    },
    /// Unity editor releases JSON API.
    Unity {
        #[serde(default = "default_unity_limit")]
        limit: u32,
    },
    /// Hacker News search via the Algolia API.
    Hn { query: String },
    /// Scraped HTML page.
    Html {
        url: String,
        /// CSS selector matching one element per item.
        item: String,
        /// Selectors relative to `item`.
        title: String,
        link: String,
        date: Option<String>,
    },
}

/// `tag_prefix` as one string or a list. It is written back as before it took a list:
/// `null` for none and a plain string for one, so stored sources keep their JSON.
mod tag_prefixes {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
        let prefixes = match Option::<OneOrMany>::deserialize(d)? {
            None => Vec::new(),
            Some(OneOrMany::One(p)) => vec![p],
            Some(OneOrMany::Many(ps)) => ps,
        };
        Ok(prefixes.into_iter().filter(|p| !p.is_empty()).collect())
    }

    pub fn serialize<S: Serializer>(prefixes: &[String], s: S) -> Result<S::Ok, S::Error> {
        match prefixes {
            [] => s.serialize_none(),
            [one] => one.serialize(s),
            many => many.serialize(s),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum GithubMode {
    #[default]
    Releases,
    Tags,
    Commits,
}

/// Whether a source belongs to an engine or a general outlet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    Engine(EngineId),
    Outlet(String),
}

impl Owner {
    pub fn id(&self) -> &str {
        match self {
            Owner::Engine(id) | Owner::Outlet(id) => id,
        }
    }
}

/// Stable id of a source, derived from its owner and spec, e.g. `godot:github:godotengine/godot`.
pub fn source_id(owner_id: &str, spec: &SourceSpec) -> String {
    format!("{owner_id}:{}", spec.key())
}

/// A source flattened out of its owner, with a stable id.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedSource {
    /// Stable id derived from owner + spec, e.g. `godot:github:godotengine/godot`.
    pub id: String,
    pub owner: Owner,
    /// Display name of the owner.
    pub owner_name: String,
    /// The source's own `enabled` flag from the config (runtime overrides may differ).
    pub enabled: bool,
    pub category: Option<ItemKind>,
    pub interval_minutes: Option<u64>,
    pub keywords: Vec<String>,
    /// Only meaningful for outlets.
    pub require_engine_match: bool,
    pub spec: SourceSpec,
}

impl SourceSpec {
    fn key(&self) -> String {
        match self {
            SourceSpec::Feed { url, .. } => format!("feed:{url}"),
            SourceSpec::Github { repo, mode, .. } => match mode {
                GithubMode::Releases => format!("github:{repo}"),
                GithubMode::Tags => format!("github-tags:{repo}"),
                GithubMode::Commits => format!("github-commits:{repo}"),
            },
            SourceSpec::Unity { .. } => "unity".to_string(),
            SourceSpec::Hn { query } => format!("hn:{query}"),
            SourceSpec::Html { url, .. } => format!("html:{url}"),
        }
    }

    /// Checks a spec entered in the web UI. The error is a message for the user.
    pub fn check(&self) -> Result<(), String> {
        match self {
            SourceSpec::Feed { url, .. } => check_url(url),
            SourceSpec::Github { repo, .. } => {
                let re =
                    regex::Regex::new(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$").expect("valid regex");
                if re.is_match(repo) {
                    Ok(())
                } else {
                    Err(format!(
                        "“{repo}” is not a GitHub repository; use owner/repo."
                    ))
                }
            }
            SourceSpec::Unity { limit } => {
                if (1..=100).contains(limit) {
                    Ok(())
                } else {
                    Err("The Unity limit must be between 1 and 100.".into())
                }
            }
            SourceSpec::Hn { query } => {
                if query.trim().is_empty() {
                    Err("Enter a Hacker News search query.".into())
                } else {
                    Ok(())
                }
            }
            SourceSpec::Html {
                url,
                item,
                title,
                link,
                date,
            } => {
                check_url(url)?;
                for (css, what) in [(item, "item"), (title, "title"), (link, "link")] {
                    crate::fetch::html::check_selector(css, what)?;
                }
                if let Some(date) = date {
                    crate::fetch::html::check_selector(date, "date")?;
                }
                Ok(())
            }
        }
    }
}

/// Longest polling interval accepted from the web UI: one week.
pub const MAX_INTERVAL_MINUTES: u64 = 7 * 24 * 60;

impl SourceConfig {
    /// Checks a source entered in the web UI. The error is a message for the user.
    pub fn check(&self) -> Result<(), String> {
        if let Some(minutes) = self.interval_minutes
            && !(1..=MAX_INTERVAL_MINUTES).contains(&minutes)
        {
            return Err(format!(
                "The interval must be between 1 and {MAX_INTERVAL_MINUTES} minutes."
            ));
        }
        self.spec.check()
    }
}

/// Whether `id` can name an engine or outlet added in the web UI: a short lowercase slug,
/// so it fits in `/engine/{id}` and in source ids (`{owner}:{key}`).
pub fn valid_owner_id(id: &str) -> bool {
    let re = regex::Regex::new(r"^[a-z0-9][a-z0-9-]{0,39}$").expect("valid regex");
    re.is_match(id)
}

pub fn check_url(url: &str) -> Result<(), String> {
    match url::Url::parse(url) {
        Ok(u) if matches!(u.scheme(), "http" | "https") && u.host().is_some() => Ok(()),
        _ => Err(format!("“{url}” is not an http(s) URL.")),
    }
}

impl SourcesConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        let mut ids = std::collections::HashSet::new();
        for id in self
            .engines
            .iter()
            .map(|e| &e.id)
            .chain(self.outlets.iter().map(|o| &o.id))
        {
            if !ids.insert(id) {
                bail!("duplicate engine/outlet id `{id}`");
            }
        }
        let mut source_ids = std::collections::HashSet::new();
        for s in self.all_sources() {
            if !source_ids.insert(s.id.clone()) {
                bail!("duplicate source `{}`", s.id);
            }
        }
        Ok(())
    }

    pub fn engine(&self, id: &str) -> Option<&EngineConfig> {
        self.engines.iter().find(|e| e.id == id)
    }

    /// Enabled engines, in config order.
    pub fn enabled_engines(&self) -> impl Iterator<Item = &EngineConfig> {
        self.engines.iter().filter(|e| e.enabled)
    }

    /// Every source of every engine and outlet, including disabled ones.
    pub fn all_sources(&self) -> Vec<ResolvedSource> {
        self.resolve().into_iter().map(|(s, _)| s).collect()
    }

    /// Sources whose owner and the source itself are enabled in the config.
    pub fn enabled_sources(&self) -> Vec<ResolvedSource> {
        self.resolve()
            .into_iter()
            .filter(|(s, owner_enabled)| *owner_enabled && s.enabled)
            .map(|(s, _)| s)
            .collect()
    }

    /// Sources of enabled engines and outlets, including sources disabled in the config.
    /// These are the ones that can be switched on or off at runtime.
    pub fn manageable_sources(&self) -> Vec<ResolvedSource> {
        self.resolve()
            .into_iter()
            .filter(|(_, owner_enabled)| *owner_enabled)
            .map(|(s, _)| s)
            .collect()
    }

    /// Enabled engines, then enabled outlets, in config order: the owners whose sources can
    /// be managed at runtime.
    pub fn manageable_owners(&self) -> Vec<(Owner, &str)> {
        self.enabled_engines()
            .map(|e| (Owner::Engine(e.id.clone()), e.name.as_str()))
            .chain(
                self.outlets
                    .iter()
                    .filter(|o| o.enabled)
                    .map(|o| (Owner::Outlet(o.id.clone()), o.name.as_str())),
            )
            .collect()
    }

    /// Every source, paired with whether its owner is enabled.
    fn resolve(&self) -> Vec<(ResolvedSource, bool)> {
        let mut out = Vec::new();
        for e in &self.engines {
            for s in &e.sources {
                let source = ResolvedSource {
                    id: source_id(&e.id, &s.spec),
                    owner: Owner::Engine(e.id.clone()),
                    owner_name: e.name.clone(),
                    enabled: s.enabled,
                    category: s.category,
                    interval_minutes: s.interval_minutes,
                    keywords: s.keywords.clone(),
                    require_engine_match: false,
                    spec: s.spec.clone(),
                };
                out.push((source, e.enabled));
            }
        }
        for o in &self.outlets {
            for s in &o.sources {
                let source = ResolvedSource {
                    id: source_id(&o.id, &s.spec),
                    owner: Owner::Outlet(o.id.clone()),
                    owner_name: o.name.clone(),
                    enabled: s.enabled,
                    category: s.category,
                    interval_minutes: s.interval_minutes,
                    keywords: s.keywords.clone(),
                    require_engine_match: o.require_engine_match,
                    spec: s.spec.clone(),
                };
                out.push((source, o.enabled));
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// gen.toml
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub database: DatabaseConfig,
    #[serde(default)]
    pub polling: PollingConfig,
    #[serde(default)]
    pub notify: NotifyConfig,
    /// The Discord bot. Without this section the bot doesn't run.
    pub discord: Option<DiscordConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscordConfig {
    /// Bot token from the Discord Developer Portal, e.g. `${GEN_DISCORD_TOKEN}`.
    pub token: String,
    /// Registers the slash commands on this server only, where they update instantly
    /// (handy while developing). Unset: commands are registered for every server.
    pub dev_guild_id: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(default = "default_bind")]
    pub bind: String,
    /// Public base URL, used for links in `/feed.xml` and notifications.
    #[serde(default = "default_base_url")]
    pub base_url: String,
    #[serde(default = "default_templates_dir")]
    pub templates_dir: PathBuf,
    #[serde(default = "default_static_dir")]
    pub static_dir: PathBuf,
    /// Enables the admin login (e.g. for switching sources on and off). Unset or empty
    /// means admin features are off. Use a long random value.
    pub admin_token: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            bind: default_bind(),
            base_url: default_base_url(),
            templates_dir: default_templates_dir(),
            static_dir: default_static_dir(),
            admin_token: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseConfig {
    #[serde(default = "default_db_path")]
    pub path: PathBuf,
    /// Items older than this many days are deleted once a day. Unset keeps every item.
    #[serde(default, deserialize_with = "deserialize_retention_days")]
    pub retention_days: Option<u32>,
}

impl DatabaseConfig {
    /// How long items are kept, if they're pruned at all.
    pub fn retention(&self) -> Option<chrono::TimeDelta> {
        self.retention_days
            .map(|days| chrono::TimeDelta::days(days.into()))
    }
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        DatabaseConfig {
            path: default_db_path(),
            retention_days: None,
        }
    }
}

/// Retention must cover the duplicate window: a story pruned sooner could be stored again
/// from another source as a new item.
fn deserialize_retention_days<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<u32>, D::Error> {
    let days = u32::deserialize(d)?;
    let min = crate::store::DUPLICATE_WINDOW_DAYS;
    if i64::from(days) < min {
        return Err(serde::de::Error::custom(format!(
            "retention_days must be at least {min}"
        )));
    }
    Ok(Some(days))
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PollingConfig {
    #[serde(default = "default_interval")]
    pub default_interval_minutes: u64,
    /// Maximum number of sources fetched at the same time.
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
    #[serde(default = "default_user_agent")]
    pub user_agent: String,
    /// Read from config or the `GITHUB_TOKEN` env var.
    pub github_token: Option<String>,
}

impl Default for PollingConfig {
    fn default() -> Self {
        PollingConfig {
            default_interval_minutes: default_interval(),
            concurrency: default_concurrency(),
            user_agent: default_user_agent(),
            github_token: None,
        }
    }
}

/// Channels from `gen.toml`. They're imported into the database once
/// ([`crate::notify::import_once`]) and managed on the website after that.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct NotifyConfig {
    #[serde(default, rename = "channel")]
    pub channels: Vec<ChannelConfig>,
}

/// A notification channel. Its strings may hold `${VAR}` references, which are expanded
/// when sending ([`ChannelConfig::resolved`]), so secrets can stay in the environment.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct ChannelConfig {
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub rules: NotifyRules,
    /// Also send a message when a source starts failing.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub health_alerts: bool,
    #[serde(flatten)]
    pub target: ChannelTarget,
}

/// How a Discord channel's messages look.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MessageStyle {
    /// A card built from Discord's layout components.
    #[default]
    Card,
    /// A full embed: engine, summary, fields and a thumbnail.
    Polished,
    /// A short embed for busy channels.
    Compact,
    /// An embed led by a large preview image.
    Image,
}

impl MessageStyle {
    pub const ALL: [MessageStyle; 4] = [
        MessageStyle::Card,
        MessageStyle::Polished,
        MessageStyle::Compact,
        MessageStyle::Image,
    ];

    pub fn key(self) -> &'static str {
        match self {
            MessageStyle::Card => "card",
            MessageStyle::Polished => "polished",
            MessageStyle::Compact => "compact",
            MessageStyle::Image => "image",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            MessageStyle::Card => "Card",
            MessageStyle::Polished => "Polished embed",
            MessageStyle::Compact => "Compact embed",
            MessageStyle::Image => "Big image",
        }
    }

    pub fn parse(key: &str) -> Option<MessageStyle> {
        MessageStyle::ALL.into_iter().find(|s| s.key() == key)
    }
}

/// One engine's topic, as subscribed to by a Discord bot channel.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct BotSubscription {
    pub engine: EngineId,
    pub topic: Topic,
}

/// Which items a channel is notified about. Empty lists mean "no restriction".
#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NotifyRules {
    #[serde(default)]
    pub engines: Vec<EngineId>,
    #[serde(default)]
    pub kinds: Vec<ItemKind>,
    #[serde(default)]
    pub exclude_kinds: Vec<ItemKind>,
    /// Case-insensitive substrings; at least one must appear in title or summary.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// A search with the same meaning as the site's search box: every word must match
    /// the start of a word in title or summary, ignoring case and accents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<String>,
    /// Also notify about items from general outlets that matched no engine.
    #[serde(default)]
    pub include_untagged: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ChannelTarget {
    Discord {
        webhook_url: String,
        #[serde(default)]
        style: MessageStyle,
    },
    /// A Discord channel the bot posts to, set up with its slash commands. It is sent the
    /// items of its subscriptions (and the channel's rules still apply on top).
    DiscordBot {
        guild_id: u64,
        channel_id: u64,
        /// Server and channel names when subscribed, for display.
        #[serde(default)]
        guild_name: String,
        #[serde(default)]
        channel_name: String,
        #[serde(default)]
        subscriptions: Vec<BotSubscription>,
        /// The bot created the channel, so `/gen remove` may delete it again.
        #[serde(default)]
        created_by_bot: bool,
        #[serde(default)]
        style: MessageStyle,
    },
    Ntfy {
        #[serde(default = "default_ntfy_server")]
        server: String,
        topic: String,
        token: Option<String>,
    },
    Email {
        smtp_host: String,
        #[serde(default = "default_smtp_port")]
        smtp_port: u16,
        username: Option<String>,
        password: Option<String>,
        from: String,
        to: Vec<String>,
        /// Send one digest mail per poll cycle instead of one mail per item.
        #[serde(default)]
        digest: bool,
    },
    /// An Apprise API server, which forwards to the services it's configured for.
    Apprise {
        /// Base URL of the Apprise API, e.g. `http://apprise:8000`.
        server: String,
        /// Key of a configuration saved on the server (`POST /notify/{key}`).
        key: Option<String>,
        /// Apprise URLs to notify directly (`POST /notify`), instead of a saved key.
        urls: Option<String>,
        /// With a key: only notify the saved URLs with this tag (Apprise tag syntax).
        tag: Option<String>,
        /// HTTP Basic auth, when the server requires it.
        username: Option<String>,
        password: Option<String>,
    },
}

impl ChannelTarget {
    pub fn kind(&self) -> &'static str {
        match self {
            ChannelTarget::Discord { .. } => "discord",
            ChannelTarget::DiscordBot { .. } => "discordbot",
            ChannelTarget::Ntfy { .. } => "ntfy",
            ChannelTarget::Email { .. } => "email",
            ChannelTarget::Apprise { .. } => "apprise",
        }
    }

    /// Every string of the target, mutably, for `${VAR}` expansion.
    fn strings_mut(&mut self) -> Vec<&mut String> {
        match self {
            ChannelTarget::Discord { webhook_url, .. } => vec![webhook_url],
            ChannelTarget::DiscordBot { .. } => Vec::new(),
            ChannelTarget::Ntfy {
                server,
                topic,
                token,
            } => [server, topic].into_iter().chain(token.as_mut()).collect(),
            ChannelTarget::Email {
                smtp_host,
                username,
                password,
                from,
                to,
                ..
            } => [smtp_host, from]
                .into_iter()
                .chain(username.as_mut())
                .chain(password.as_mut())
                .chain(to.iter_mut())
                .collect(),
            ChannelTarget::Apprise {
                server,
                key,
                urls,
                tag,
                username,
                password,
            } => std::iter::once(server)
                .chain([key, urls, tag, username, password].into_iter().flatten())
                .collect(),
        }
    }
}

impl ChannelConfig {
    /// A copy with `${VAR}` references expanded from the environment, ready to send with.
    pub fn resolved(&self) -> anyhow::Result<ChannelConfig> {
        let mut channel = self.clone();
        for s in channel.target.strings_mut() {
            *s = expand_env(s)?;
        }
        Ok(channel)
    }

    /// Checks a channel entered in the web UI. The error is a message for the user.
    pub fn check(&self) -> Result<(), String> {
        if !valid_owner_id(&self.name) {
            return Err(format!(
                "“{}” can't be used as a name: use up to 40 lowercase letters, digits and dashes.",
                self.name
            ));
        }
        // A `${VAR}` reference is checked when sending, once it's expanded.
        let literal = |s: &str| !s.contains("${");
        match &self.target {
            ChannelTarget::Discord { webhook_url, .. } => {
                if literal(webhook_url)
                    && !webhook_url.starts_with("https://discord.com/api/webhooks/")
                    && !webhook_url.starts_with("https://discordapp.com/api/webhooks/")
                {
                    return Err(
                        "The webhook URL must start with https://discord.com/api/webhooks/ \
                                (or be a ${VARIABLE})."
                            .into(),
                    );
                }
            }
            ChannelTarget::DiscordBot {
                channel_id,
                subscriptions,
                ..
            } => {
                if *channel_id == 0 {
                    return Err("The Discord channel is missing.".into());
                }
                if subscriptions.is_empty() {
                    return Err("The channel isn't subscribed to anything.".into());
                }
            }
            ChannelTarget::Ntfy { server, topic, .. } => {
                if literal(server) {
                    check_url(server)?;
                }
                if topic.trim().is_empty() {
                    return Err("Enter the ntfy topic.".into());
                }
            }
            ChannelTarget::Email {
                smtp_host,
                smtp_port,
                from,
                to,
                ..
            } => {
                if smtp_host.trim().is_empty() {
                    return Err("Enter the SMTP host.".into());
                }
                if *smtp_port == 0 {
                    return Err("Enter the SMTP port.".into());
                }
                if from.trim().is_empty() {
                    return Err("Enter the From address.".into());
                }
                if to.is_empty() {
                    return Err("Enter at least one recipient.".into());
                }
            }
            ChannelTarget::Apprise {
                server, key, urls, ..
            } => {
                if literal(server) {
                    check_url(server)?;
                }
                let set = |v: &Option<String>| v.as_deref().is_some_and(|v| !v.trim().is_empty());
                match (set(key), set(urls)) {
                    (false, false) => {
                        return Err("Enter a configuration key or Apprise URLs.".into());
                    }
                    (true, true) => {
                        return Err(
                            "Use either a configuration key or Apprise URLs, not both.".into()
                        );
                    }
                    _ => {}
                }
                if let Some(key) = key.as_deref().filter(|k| literal(k)) {
                    let re = regex::Regex::new(r"^[A-Za-z0-9_-]{1,128}$").expect("valid regex");
                    if !re.is_match(key) {
                        return Err(
                            "The configuration key may only hold letters, digits, - and _.".into(),
                        );
                    }
                }
            }
        }
        Ok(())
    }
}

impl AppConfig {
    /// Loads `gen.toml`, expanding `${VAR}` references from the environment. The `notify`
    /// section is left as written: channels expand their references when sending.
    /// A missing file yields the defaults.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let mut cfg: AppConfig = if path.exists() {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            let mut value: toml::Value =
                toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
            expand_env_in_value(&mut value)?;
            value
                .try_into()
                .with_context(|| format!("parsing {}", path.display()))?
        } else {
            AppConfig::default()
        };
        if cfg
            .polling
            .github_token
            .as_deref()
            .is_none_or(str::is_empty)
        {
            cfg.polling.github_token = std::env::var("GITHUB_TOKEN").ok().filter(|t| !t.is_empty());
        }
        Ok(cfg)
    }
}

/// Applies `expand_env` to every string in a parsed `gen.toml` (so comments are ignored),
/// except under the top-level `notify` table.
fn expand_env_in_value(value: &mut toml::Value) -> anyhow::Result<()> {
    match value {
        toml::Value::Table(table) => table
            .iter_mut()
            .filter(|(k, _)| *k != "notify")
            .try_for_each(|(_, v)| expand_env_in_strings(v)),
        other => expand_env_in_strings(other),
    }
}

fn expand_env_in_strings(value: &mut toml::Value) -> anyhow::Result<()> {
    match value {
        toml::Value::String(s) => *s = expand_env(s)?,
        toml::Value::Array(items) => items.iter_mut().try_for_each(expand_env_in_strings)?,
        toml::Value::Table(table) => table
            .iter_mut()
            .map(|(_, v)| v)
            .try_for_each(expand_env_in_strings)?,
        _ => {}
    }
    Ok(())
}

/// Replaces `${NAME}` with the value of env var `NAME`. Unset vars are an error.
pub fn expand_env(text: &str) -> anyhow::Result<String> {
    let re = regex::Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}").expect("valid regex");
    let mut missing = Vec::new();
    let out = re.replace_all(text, |caps: &regex::Captures| {
        std::env::var(&caps[1]).unwrap_or_else(|_| {
            missing.push(caps[1].to_string());
            String::new()
        })
    });
    if !missing.is_empty() {
        bail!("unset environment variables: {}", missing.join(", "));
    }
    Ok(out.into_owned())
}

fn default_true() -> bool {
    true
}
pub fn default_unity_limit() -> u32 {
    25
}
fn default_bind() -> String {
    "0.0.0.0:8080".to_string()
}
fn default_base_url() -> String {
    "http://localhost:8080".to_string()
}
fn default_templates_dir() -> PathBuf {
    PathBuf::from("templates")
}
fn default_static_dir() -> PathBuf {
    PathBuf::from("static")
}
fn default_db_path() -> PathBuf {
    PathBuf::from("data/gen.db")
}
fn default_interval() -> u64 {
    60
}
fn default_concurrency() -> usize {
    8
}
fn default_user_agent() -> String {
    format!(
        "GEN/{} (Game Engine News aggregator)",
        env!("CARGO_PKG_VERSION")
    )
}
fn default_ntfy_server() -> String {
    "https://ntfy.sh".to_string()
}
fn default_smtp_port() -> u16 {
    587
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tier_round_trips_through_json_as_a_number() {
        for (json, tier) in [
            ("1", Tier::Major),
            ("2", Tier::Smaller),
            ("3", Tier::Frameworks),
        ] {
            assert_eq!(serde_json::from_str::<Tier>(json).unwrap(), tier);
            assert_eq!(serde_json::to_string(&tier).unwrap(), json);
        }
    }

    #[test]
    fn a_tier_outside_1_to_3_is_rejected() {
        for json in ["0", "4"] {
            let err = serde_json::from_str::<Tier>(json).unwrap_err();
            assert!(
                err.to_string().contains("The tier must be 1, 2 or 3."),
                "{json}: {err}"
            );
        }
    }

    #[test]
    fn reads_an_engine_stored_with_the_old_csharp_field() {
        let json = r#"{"id":"mine","name":"Mine","tier":3,"csharp":"none (Rust)",
            "homepage":null,"enabled":true,"aliases":["mine"],"exclude":[],"source":[]}"#;
        let engine: EngineConfig = serde_json::from_str(json).unwrap();
        assert!(engine.languages.is_empty());
        assert_eq!(engine.language_note.as_deref(), Some("none (Rust)"));
        assert_eq!(engine.aliases, ["mine"]);
        // Stored again, it is in the new shape.
        let json = serde_json::to_string(&engine).unwrap();
        assert!(!json.contains("csharp"), "{json}");
    }

    #[test]
    fn engine_languages_round_trip_through_json() {
        let json = r#"{"id":"flax","name":"Flax","tier":1,"languages":["C#","C++"],
            "language_note":"engine written in C++","homepage":null,"source":[]}"#;
        let engine: EngineConfig = serde_json::from_str(json).unwrap();
        assert_eq!(engine.languages, ["C#", "C++"]);
        assert_eq!(
            engine.language_note.as_deref(),
            Some("engine written in C++")
        );
        let again: EngineConfig =
            serde_json::from_str(&serde_json::to_string(&engine).unwrap()).unwrap();
        assert_eq!(again.languages, engine.languages);
        assert_eq!(again.language_note, engine.language_note);
    }

    #[test]
    fn refuses_an_engine_with_both_csharp_and_languages() {
        let json = r#"{"id":"x","name":"X","tier":1,"csharp":"native","languages":["C#"],
            "homepage":null}"#;
        let err = serde_json::from_str::<EngineConfig>(json).unwrap_err();
        assert!(err.to_string().contains("csharp"), "{err}");
    }

    #[test]
    fn parses_engine_and_outlet_sources() {
        let cfg: SourcesConfig = toml::from_str(
            r#"
            [[engine]]
            id = "godot"
            name = "Godot"
            tier = 1
            languages = ["GDScript", "C#", "C++"]
            aliases = ["godot"]
              [[engine.source]]
              kind = "feed"
              url = "https://godotengine.org/rss.xml"
              category = "news"
              [[engine.source]]
              kind = "github"
              repo = "godotengine/godot"
              category = "release"

            [[outlet]]
            id = "gfs"
            name = "GameFromScratch"
            require_engine_match = true
              [[outlet.source]]
              kind = "feed"
              url = "https://gamefromscratch.com/feed/"
            "#,
        )
        .unwrap();
        cfg.validate().unwrap();
        let sources = cfg.enabled_sources();
        assert_eq!(sources.len(), 3);
        assert_eq!(sources[1].id, "godot:github:godotengine/godot");
        assert_eq!(sources[1].category, Some(ItemKind::Release));
        assert!(sources[2].require_engine_match);
    }

    /// `tag_prefix` takes one prefix or several (a monorepo whose engine changed its tag
    /// scheme); empty entries are dropped, and the prefix never changes the source id.
    #[test]
    fn tag_prefix_takes_one_prefix_or_several() {
        let spec = |prefix: &str| -> SourceSpec {
            toml::from_str(&format!(
                "kind = \"github\"\nrepo = \"flame-engine/flame\"\n{prefix}"
            ))
            .unwrap()
        };
        let prefixes = |spec: &SourceSpec| match spec {
            SourceSpec::Github { tag_prefix, .. } => tag_prefix.clone(),
            other => panic!("not github: {other:?}"),
        };
        let none = spec("");
        let one = spec(r#"tag_prefix = "v""#);
        let several = spec(r#"tag_prefix = ["v", "", "flame-v"]"#);
        assert_eq!(prefixes(&none), Vec::<String>::new());
        assert_eq!(prefixes(&one), vec!["v"]);
        assert_eq!(prefixes(&several), vec!["v", "flame-v"]);
        assert_eq!(prefixes(&spec(r#"tag_prefix = """#)), Vec::<String>::new());
        for s in [&none, &one, &several] {
            assert_eq!(source_id("flame", s), "flame:github:flame-engine/flame");
        }
    }

    /// Sources stored before `tag_prefix` took a list still load, and a single prefix is
    /// stored as before.
    #[test]
    fn stored_tag_prefixes_keep_their_json_form() {
        let one = r#"{"kind":"github","repo":"a/b","mode":"releases","tag_prefix":"v"}"#;
        let none = r#"{"kind":"github","repo":"a/b","mode":"releases","tag_prefix":null}"#;
        for json in [one, none] {
            let spec: SourceSpec = serde_json::from_str(json).unwrap();
            assert_eq!(serde_json::to_string(&spec).unwrap(), json);
        }
        let several =
            r#"{"kind":"github","repo":"a/b","mode":"releases","tag_prefix":["v","flame-v"]}"#;
        let spec: SourceSpec = serde_json::from_str(several).unwrap();
        assert_eq!(serde_json::to_string(&spec).unwrap(), several);
    }

    #[test]
    fn source_configs_round_trip_through_json() {
        let cfg: SourcesConfig = toml::from_str(
            r#"
            [[engine]]
            id = "x"
            name = "X"
            tier = 1
            languages = ["C++"]
              [[engine.source]]
              kind = "feed"
              url = "https://x.org/feed"
              [[engine.source]]
              kind = "github"
              repo = "x/x"
              mode = "tags"
              tag_prefix = "v"
              category = "release"
              interval_minutes = 30
              keywords = ["engine"]
              [[engine.source]]
              kind = "unity"
              [[engine.source]]
              kind = "hn"
              query = "x engine"
              enabled = false
              [[engine.source]]
              kind = "html"
              url = "https://x.org/news"
              item = "article"
              title = "h2"
              link = "a"
            "#,
        )
        .unwrap();
        for source in &cfg.engines[0].sources {
            let json = serde_json::to_string(source).unwrap();
            let back: SourceConfig = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, source, "{json}");
        }
    }

    #[test]
    fn checks_specs_from_the_web_ui() {
        let feed = |url: &str| SourceSpec::Feed {
            url: url.into(),
            cache_bust: false,
        };
        assert!(feed("https://x.org/feed").check().is_ok());
        assert!(feed("ftp://x.org/feed").check().is_err());
        assert!(feed("not a url").check().is_err());
        let github = |repo: &str| SourceSpec::Github {
            repo: repo.into(),
            mode: GithubMode::Releases,
            tag_prefix: Vec::new(),
        };
        assert!(github("godotengine/godot").check().is_ok());
        assert!(
            github("https://github.com/godotengine/godot")
                .check()
                .is_err()
        );
        assert!(SourceSpec::Unity { limit: 0 }.check().is_err());
        assert!(SourceSpec::Hn { query: " ".into() }.check().is_err());
        let html = |item: &str| SourceSpec::Html {
            url: "https://x.org".into(),
            item: item.into(),
            title: "h2".into(),
            link: "a".into(),
            date: None,
        };
        assert!(html("article.post").check().is_ok());
        assert!(html("article[").check().is_err());
    }

    #[test]
    fn expands_env_vars() {
        // SAFETY: test-only, single-threaded access to this unique variable.
        unsafe { std::env::set_var("GEN_TEST_HOOK", "abc") };
        assert_eq!(expand_env("x=${GEN_TEST_HOOK}").unwrap(), "x=abc");
        assert!(expand_env("${GEN_TEST_DEFINITELY_UNSET}").is_err());
    }

    fn channel(toml: &str) -> ChannelConfig {
        toml::from_str(toml).unwrap()
    }

    #[test]
    fn leaves_notify_references_for_send_time() {
        let mut value: toml::Value = toml::from_str(
            r#"
            [server]
            base_url = "${GEN_TEST_DEFINITELY_UNSET_TOO}"
            "#,
        )
        .unwrap();
        assert!(
            expand_env_in_value(&mut value).is_err(),
            "other sections expand"
        );

        let mut value: toml::Value = toml::from_str(
            r#"
            [[notify.channel]]
            name = "phone"
            kind = "ntfy"
            topic = "${GEN_TEST_DEFINITELY_UNSET}"
            "#,
        )
        .unwrap();
        expand_env_in_value(&mut value).unwrap();
        let cfg: AppConfig = value.try_into().unwrap();
        let phone = &cfg.notify.channels[0];
        assert!(
            matches!(&phone.target, ChannelTarget::Ntfy { topic, .. } if topic == "${GEN_TEST_DEFINITELY_UNSET}")
        );
        assert!(phone.resolved().is_err(), "unset when sending");

        // SAFETY: test-only, single-threaded access to this unique variable.
        unsafe { std::env::set_var("GEN_TEST_NTFY_TOPIC", "gen-alerts") };
        let resolved = channel(
            r#"
            name = "phone"
            kind = "ntfy"
            topic = "${GEN_TEST_NTFY_TOPIC}"
            "#,
        )
        .resolved()
        .unwrap();
        assert!(
            matches!(resolved.target, ChannelTarget::Ntfy { topic, .. } if topic == "gen-alerts")
        );
    }

    #[test]
    fn checks_channels_from_the_web_ui() {
        let ok = |toml: &str| channel(toml).check();
        assert!(ok("name = \"chat\"\nkind = \"discord\"\nwebhook_url = \"https://discord.com/api/webhooks/1/x\"").is_ok());
        assert!(ok("name = \"chat\"\nkind = \"discord\"\nwebhook_url = \"${GEN_HOOK}\"").is_ok());
        let err =
            ok("name = \"chat\"\nkind = \"discord\"\nwebhook_url = \"https://example.com/hook\"")
                .unwrap_err();
        assert!(err.contains("discord.com"), "{err}");
        let err = ok("name = \"Chat Room\"\nkind = \"ntfy\"\ntopic = \"t\"").unwrap_err();
        assert!(err.contains("can't be used as a name"), "{err}");
        assert!(ok("name = \"p\"\nkind = \"ntfy\"\ntopic = \" \"").is_err());
        let err =
            ok("name = \"m\"\nkind = \"email\"\nsmtp_host = \"smtp.x\"\nfrom = \"a@x\"\nto = []")
                .unwrap_err();
        assert!(err.contains("recipient"), "{err}");
        let apprise = |rest: &str| {
            ok(&format!(
                "name = \"a\"\nkind = \"apprise\"\nserver = \"http://apprise:8000\"\n{rest}"
            ))
        };
        assert!(apprise("key = \"gen\"").is_ok());
        assert!(apprise("urls = \"${GEN_APPRISE_URLS}\"").is_ok());
        assert!(apprise("").unwrap_err().contains("key or Apprise URLs"));
        assert!(
            apprise("key = \"gen\"\nurls = \"tgram://x\"")
                .unwrap_err()
                .contains("not both")
        );
        assert!(apprise("key = \"a/b\"").unwrap_err().contains("letters"));
    }

    #[test]
    fn stores_discord_bot_channels() {
        let json = r#"{"name":"discord-unity-news","kind":"discordbot","guild_id":1,
            "channel_id":12345678901234567890,"channel_name":"unity-news",
            "subscriptions":[{"engine":"unity","topic":"news"},{"engine":"unity","topic":"releases"}]}"#;
        let c: ChannelConfig = serde_json::from_str(json).unwrap();
        let ChannelTarget::DiscordBot {
            channel_id,
            subscriptions,
            created_by_bot,
            ..
        } = &c.target
        else {
            panic!("{c:?}");
        };
        assert_eq!(*channel_id, 12345678901234567890);
        assert_eq!(subscriptions[1].topic, Topic::Releases);
        assert!(!created_by_bot);
        assert_eq!(c.target.kind(), "discordbot");
        assert!(
            matches!(
                c.target,
                ChannelTarget::DiscordBot {
                    style: MessageStyle::Card,
                    ..
                }
            ),
            "rows stored before styles existed get the card"
        );
        let hook: ChannelConfig = serde_json::from_str(
            r#"{"name":"chat","kind":"discord","webhook_url":"https://discord.com/api/webhooks/1/x",
                "style":"compact"}"#,
        )
        .unwrap();
        assert!(matches!(
            hook.target,
            ChannelTarget::Discord {
                style: MessageStyle::Compact,
                ..
            }
        ));
        assert_eq!(MessageStyle::parse("image"), Some(MessageStyle::Image));
        assert_eq!(c.check(), Ok(()));
        let again: ChannelConfig =
            serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!(again, c);
        assert_eq!(c.resolved().unwrap(), c, "nothing to expand");

        let mut empty = c.clone();
        if let ChannelTarget::DiscordBot { subscriptions, .. } = &mut empty.target {
            subscriptions.clear();
        }
        assert!(empty.check().unwrap_err().contains("isn't subscribed"));
    }

    #[test]
    fn discord_section_is_optional() {
        let cfg: AppConfig = toml::from_str("").unwrap();
        assert!(cfg.discord.is_none());
        let cfg: AppConfig =
            toml::from_str("[discord]\ntoken = \"abc\"\ndev_guild_id = 42").unwrap();
        let discord = cfg.discord.unwrap();
        assert_eq!(
            (discord.token.as_str(), discord.dev_guild_id),
            ("abc", Some(42))
        );
    }

    #[test]
    fn item_retention_is_off_by_default_and_keeps_the_duplicate_window() {
        let cfg: AppConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.database.retention(), None);
        let cfg: AppConfig = toml::from_str("[database]\nretention_days = 365").unwrap();
        assert_eq!(cfg.database.retention(), Some(chrono::TimeDelta::days(365)));
        // Shorter than the duplicate window would let a pruned story come back as new.
        let err = toml::from_str::<AppConfig>("[database]\nretention_days = 3").unwrap_err();
        assert!(err.to_string().contains("at least 7"), "{err}");
    }
}
