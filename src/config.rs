//! Configuration: `sources.toml` (engines, outlets, their sources) and `gen.toml` (app settings).

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

use crate::model::{EngineId, ItemKind};

// ---------------------------------------------------------------------------
// sources.toml
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
#[serde(deny_unknown_fields)]
pub struct EngineConfig {
    pub id: EngineId,
    pub name: String,
    /// 1 = major C#, 2 = niche C#, 3 = notable without C#.
    pub tier: u8,
    /// Human description of C# support, e.g. "native", "first-class (.NET build)", "none".
    pub csharp: String,
    pub homepage: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Whole-word phrases that tag outlet items with this engine. All-lowercase aliases match
    /// in any case; an alias with an uppercase letter (e.g. "Unity") matches case-sensitively.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Phrases that veto an alias match (e.g. "ubuntu unity").
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default, rename = "source")]
    pub sources: Vec<SourceConfig>,
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
        /// Only keep tags starting with this prefix (e.g. Stride's `releases/`).
        tag_prefix: Option<String>,
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
const MAX_INTERVAL_MINUTES: u64 = 7 * 24 * 60;

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
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let cfg: SourcesConfig =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

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
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        DatabaseConfig {
            path: default_db_path(),
        }
    }
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

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct NotifyConfig {
    #[serde(default, rename = "channel")]
    pub channels: Vec<ChannelConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChannelConfig {
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub rules: NotifyRules,
    #[serde(flatten)]
    pub target: ChannelTarget,
}

/// Which items a channel is notified about. Empty lists mean "no restriction".
#[derive(Debug, Clone, Deserialize, Default)]
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
    /// Also notify about items from general outlets that matched no engine.
    #[serde(default)]
    pub include_untagged: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ChannelTarget {
    Discord {
        webhook_url: String,
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
}

impl AppConfig {
    /// Loads `gen.toml`, expanding `${VAR}` references from the environment.
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

/// Applies `expand_env` to every string in a parsed TOML document (so comments are ignored).
fn expand_env_in_value(value: &mut toml::Value) -> anyhow::Result<()> {
    match value {
        toml::Value::String(s) => *s = expand_env(s)?,
        toml::Value::Array(items) => items.iter_mut().try_for_each(expand_env_in_value)?,
        toml::Value::Table(table) => table
            .iter_mut()
            .map(|(_, v)| v)
            .try_for_each(expand_env_in_value)?,
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
    fn parses_engine_and_outlet_sources() {
        let cfg: SourcesConfig = toml::from_str(
            r#"
            [[engine]]
            id = "godot"
            name = "Godot"
            tier = 1
            csharp = "first-class"
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

    #[test]
    fn source_configs_round_trip_through_json() {
        let cfg: SourcesConfig = toml::from_str(
            r#"
            [[engine]]
            id = "x"
            name = "X"
            tier = 1
            csharp = "none"
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
            tag_prefix: None,
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
}
