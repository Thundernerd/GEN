//! Notifications: rule matching per channel and delivery to Discord, ntfy and email.

pub mod discord;
pub mod email;
pub mod ntfy;

use chrono::Utc;

use crate::config::{ChannelConfig, ChannelTarget, NotifyRules, SourcesConfig};
use crate::fetch::Http;
use crate::model::{Item, ItemKind};

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

/// Sends `items` (already filtered by `matches`) to one channel.
pub async fn send(
    ctx: &NotifyContext<'_>,
    channel: &ChannelConfig,
    items: &[Item],
) -> Result<(), NotifyError> {
    if items.is_empty() {
        return Ok(());
    }
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

/// A realistic item for `gen notify-test`.
pub fn sample_item() -> Item {
    let now = Utc::now();
    Item {
        id: 0,
        source_id: "godot:github:godotengine/godot".into(),
        guid: "gen-notify-test".into(),
        engines: vec!["godot".into()],
        kind: ItemKind::Release,
        title: "Godot 4.8 released".into(),
        url: Some("https://godotengine.org/releases/4.8/".into()),
        summary: Some(
            "Godot 4.8 is out, with improvements to C# support, rendering and the editor. \
             This is a test notification from GEN."
                .into(),
        ),
        published: Some(now),
        first_seen: now,
        story_key: "godot 4.8 released".into(),
        duplicate_of: None,
    }
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
    fn engine_names_fall_back_to_id() {
        let i = item(&["godot", "mystery"], ItemKind::News, "t", None);
        assert_eq!(engine_names(&sources(), &i), vec!["Godot", "mystery"]);
        assert_eq!(prefixed_title(&sources(), &i), "[Godot, mystery] t");
        let u = item(&[], ItemKind::News, "t", None);
        assert_eq!(prefixed_title(&sources(), &u), "t");
    }
}
