//! What a Discord message shows about an item, gathered once for every style: the engine
//! line, title, summary lines, fields, images, source and links.

use chrono::{DateTime, Utc};

use crate::config::{GithubMode, Owner, SourceSpec, SourcesConfig};
use crate::fetch::truncate;
use crate::model::{Item, ItemKind};
use crate::notify::{engine_names, kind_color, kind_label};

/// Discord rejects link buttons whose URL is longer than this.
const MAX_BUTTON_URL: usize = 512;

#[derive(Debug, Clone)]
pub struct Parts {
    pub color: u32,
    /// "Godot · New release": the engines (or the outlet) and the kind.
    pub author: String,
    /// The first engine's icon.
    pub author_icon: Option<String>,
    pub title: String,
    pub url: Option<String>,
    /// The summary's paragraphs and list items ("• …"), as plain text.
    pub lines: Vec<String>,
    /// Name and value, plain text.
    pub fields: Vec<(String, String)>,
    /// The item's own preview image.
    pub image: Option<String>,
    /// A small picture: the item's image, else the engine's icon.
    pub thumbnail: Option<String>,
    /// Where it came from, e.g. "GitHub · godotengine/godot".
    pub source: String,
    pub timestamp: DateTime<Utc>,
    /// Link buttons: label and URL.
    pub buttons: Vec<(String, String)>,
}

impl Parts {
    pub fn from_item(sources: &SourcesConfig, base_url: &str, item: &Item) -> Parts {
        let source = sources
            .all_sources()
            .into_iter()
            .find(|s| s.id == item.source_id);
        let names = engine_names(sources, item);
        let who = if names.is_empty() {
            source
                .as_ref()
                .map_or_else(|| "GEN".to_string(), |s| s.owner_name.clone())
        } else {
            names.join(", ")
        };
        let first_engine = item.engines.first().and_then(|id| sources.engine(id));
        let author_icon = first_engine
            .and_then(|e| e.icon.clone())
            .filter(|u| is_http(u));
        let image = item.image_url.clone().filter(|u| is_http(u));
        let source_text = source
            .as_ref()
            .map_or_else(|| "GEN".to_string(), source_label);

        let mut fields = Vec::new();
        if let Some(version) = source.as_ref().and_then(|s| version(&s.spec, &item.guid)) {
            fields.push(("Version".to_string(), version));
        }
        match item.kind {
            ItemKind::Release => fields.push(("Type".into(), "Stable".into())),
            ItemKind::Prerelease => fields.push(("Type".into(), "Prerelease".into())),
            _ => {}
        }
        if !fields.is_empty() {
            fields.push(("Source".to_string(), short_source(&source_text)));
        }

        let url = item.url.clone().filter(|u| is_http(u));
        let mut buttons = Vec::new();
        if let Some(url) = url.as_ref().filter(|u| u.len() <= MAX_BUTTON_URL) {
            let label = match item.kind {
                ItemKind::Release | ItemKind::Prerelease => "Release notes",
                ItemKind::Video => "Watch video",
                ItemKind::Community => "Open discussion",
                ItemKind::News => "Read article",
            };
            buttons.push((label.to_string(), url.clone()));
        }
        if let Some(engine) = first_engine
            && public_site(base_url)
        {
            let page = format!("{}/engine/{}", base_url.trim_end_matches('/'), engine.id);
            buttons.push(("View on GEN".to_string(), page));
        }

        Parts {
            color: kind_color(item.kind),
            author: format!("{who} · {}", kind_words(item.kind)),
            author_icon: author_icon.clone(),
            title: item.title.trim().to_string(),
            url,
            lines: item
                .summary
                .as_deref()
                .unwrap_or_default()
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect(),
            fields,
            thumbnail: image.clone().or(author_icon),
            image,
            source: source_text,
            timestamp: item.date(),
            buttons,
        }
    }

    /// The summary as Markdown, one line per paragraph or list item, cut at about
    /// `budget` characters on a line break where possible.
    pub fn description(&self, budget: usize) -> String {
        let mut out = String::new();
        for line in &self.lines {
            let line = escape_line(line);
            let len = out.chars().count();
            if len > 0 && len + 1 + line.chars().count() > budget {
                if len < budget / 2 {
                    out.push('\n');
                    out.push_str(&truncate(&line, budget - len - 1));
                }
                break;
            }
            if len > 0 {
                out.push('\n');
            }
            out.push_str(&line);
        }
        truncate(&out, budget)
    }

    /// The summary as one short paragraph, for compact messages.
    pub fn one_paragraph(&self, budget: usize) -> String {
        let text = self
            .lines
            .iter()
            .map(|l| l.trim_start_matches("• "))
            .collect::<Vec<_>>()
            .join(" ");
        truncate(&escape(&text), budget)
    }
}

/// How the kind reads after the engine name.
pub fn kind_words(kind: ItemKind) -> &'static str {
    match kind {
        ItemKind::Release => "New release",
        other => kind_label(other),
    }
}

/// A source as people know it: "GitHub · owner/repo", "Unity release API", "Hacker News",
/// the outlet's name, or the site of an engine's feed.
pub fn source_label(source: &crate::config::ResolvedSource) -> String {
    match &source.spec {
        SourceSpec::Github { repo, .. } => format!("GitHub · {repo}"),
        SourceSpec::Unity { .. } => "Unity release API".to_string(),
        SourceSpec::Hn { .. } => "Hacker News".to_string(),
        SourceSpec::Feed { url, .. } | SourceSpec::Html { url, .. } => match source.owner {
            Owner::Outlet(_) => source.owner_name.clone(),
            Owner::Engine(_) => host(url).unwrap_or_else(|| source.owner_name.clone()),
        },
    }
}

/// "GitHub · godotengine/godot" → "GitHub", for the Source field.
fn short_source(label: &str) -> String {
    label.split(" · ").next().unwrap_or(label).to_string()
}

/// The version an item is about, for sources whose ids are versions.
fn version(spec: &SourceSpec, guid: &str) -> Option<String> {
    let version = match spec {
        SourceSpec::Github {
            mode: GithubMode::Releases | GithubMode::Tags,
            ..
        } => guid.rsplit('/').next().unwrap_or(guid),
        SourceSpec::Unity { .. } => guid,
        _ => return None,
    };
    Some(version.trim().to_string()).filter(|v| !v.is_empty() && v.len() <= 64)
}

fn host(url: &str) -> Option<String> {
    let host = url::Url::parse(url).ok()?.host_str()?.to_string();
    Some(host.strip_prefix("www.").unwrap_or(&host).to_string())
}

fn is_http(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://")
}

/// Whether links to the GEN site work for others: not a local address.
fn public_site(base_url: &str) -> bool {
    match url::Url::parse(base_url) {
        Ok(u) => !matches!(
            u.host_str(),
            None | Some("localhost" | "127.0.0.1" | "0.0.0.0" | "[::1]")
        ),
        Err(_) => false,
    }
}

/// Escapes Markdown so feed text shows as written.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(
            c,
            '\\' | '*' | '_' | '~' | '`' | '|' | '[' | ']' | '<' | '>'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Escapes a summary line, including Markdown that only works at the start of a line
/// (headings, quotes, lists), but keeps GEN's own "• " bullets.
fn escape_line(line: &str) -> String {
    let escaped = escape(line);
    match escaped.chars().next() {
        Some('#' | '-' | '+') => format!("\\{escaped}"),
        Some(c) if c.is_ascii_digit() && escaped.contains(". ") => {
            // "1. Fixes" would become a numbered list.
            escaped.replacen(". ", "\\. ", 1)
        }
        _ => escaped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::tests::item;

    fn sources() -> SourcesConfig {
        toml::from_str(
            r#"
            [[engine]]
            id = "godot"
            name = "Godot"
            tier = 1
            languages = ["GDScript", "C#", "C++"]
            icon = "https://godotengine.org/assets/favicon.png"
              [[engine.source]]
              kind = "github"
              repo = "godotengine/godot"
              [[engine.source]]
              kind = "feed"
              url = "https://www.godotengine.org/rss.xml"
            [[engine]]
            id = "unity"
            name = "Unity"
            tier = 1
            languages = ["C#"]
              [[engine.source]]
              kind = "unity"
            [[outlet]]
            id = "gfs"
            name = "GameFromScratch"
              [[outlet.source]]
              kind = "feed"
              url = "https://gamefromscratch.com/feed/"
              [[outlet.source]]
              kind = "hn"
              query = "godot"
            "#,
        )
        .unwrap()
    }

    fn from(source_id: &str, guid: &str, engines: &[&str], kind: ItemKind) -> Parts {
        let mut i = item(engines, kind, "Godot 4.8", Some("Big\n• One\n• Two"));
        i.source_id = source_id.into();
        i.guid = guid.into();
        Parts::from_item(&sources(), "https://gen.example.org/", &i)
    }

    #[test]
    fn gathers_the_parts_of_a_release() {
        let p = from(
            "godot:github:godotengine/godot",
            "4.8-stable",
            &["godot"],
            ItemKind::Release,
        );
        assert_eq!(p.author, "Godot · New release");
        assert_eq!(
            p.author_icon.as_deref(),
            Some("https://godotengine.org/assets/favicon.png")
        );
        assert_eq!(
            p.thumbnail, p.author_icon,
            "the icon stands in for an image"
        );
        assert_eq!(p.source, "GitHub · godotengine/godot");
        let fields: Vec<_> = p.fields.iter().map(|(n, v)| format!("{n}={v}")).collect();
        assert_eq!(
            fields,
            ["Version=4.8-stable", "Type=Stable", "Source=GitHub"]
        );
        let buttons: Vec<_> = p.buttons.iter().map(|(l, u)| format!("{l} {u}")).collect();
        assert_eq!(
            buttons,
            [
                "Release notes https://godotengine.org/releases/4.8/",
                "View on GEN https://gen.example.org/engine/godot"
            ]
        );
        assert_eq!(p.lines, ["Big", "• One", "• Two"]);
    }

    #[test]
    fn labels_sources_and_kinds() {
        let p = from(
            "godot:feed:https://www.godotengine.org/rss.xml",
            "x",
            &["godot"],
            ItemKind::News,
        );
        assert_eq!(
            (p.author.as_str(), p.source.as_str()),
            ("Godot · News", "godotengine.org")
        );
        assert!(p.fields.is_empty(), "no version for feed items");
        assert_eq!(p.buttons[0].0, "Read article");

        let p = from(
            "gfs:feed:https://gamefromscratch.com/feed/",
            "x",
            &[],
            ItemKind::News,
        );
        assert_eq!(
            (p.author.as_str(), p.source.as_str()),
            ("GameFromScratch · News", "GameFromScratch")
        );
        assert_eq!(p.buttons.len(), 1, "no engine page to link");

        let p = from(
            "gfs:hn:godot",
            "x",
            &["godot", "unity"],
            ItemKind::Community,
        );
        assert_eq!(p.author, "Godot, Unity · Community");
        assert_eq!(p.source, "Hacker News");

        let p = from(
            "unity:unity",
            "6000.4.0b2",
            &["unity"],
            ItemKind::Prerelease,
        );
        assert_eq!(p.source, "Unity release API");
        assert_eq!(p.fields[0], ("Version".into(), "6000.4.0b2".into()));
        assert_eq!(p.fields[1], ("Type".into(), "Prerelease".into()));
        assert_eq!(p.author_icon, None, "Unity has no icon in this config");

        let p = from("gen:test:x", "x", &[], ItemKind::Video);
        assert_eq!(
            (p.author.as_str(), p.source.as_str()),
            ("GEN · Video", "GEN")
        );
    }

    #[test]
    fn leaves_out_local_gen_links_and_bad_urls() {
        let mut i = item(&["godot"], ItemKind::Release, "x", None);
        i.url = Some(format!("https://example.org/{}", "a".repeat(600)));
        let p = Parts::from_item(&sources(), "http://localhost:8080", &i);
        assert!(p.buttons.is_empty(), "{:?}", p.buttons);
        i.url = Some("javascript:alert(1)".into());
        i.image_url = Some("ftp://x/y.png".into());
        let p = Parts::from_item(&sources(), "http://127.0.0.1:8080", &i);
        assert_eq!((p.url, p.image), (None, None));
    }

    #[test]
    fn escapes_markdown_and_cuts_at_lines() {
        assert_eq!(
            escape("a *b* _c_ [d](e) <@1>"),
            r"a \*b\* \_c\_ \[d\](e) \<@1\>"
        );
        assert_eq!(escape_line("# Title"), r"\# Title");
        assert_eq!(escape_line("- item"), r"\- item");
        assert_eq!(escape_line("1. First"), r"1\. First");
        assert_eq!(escape_line("• kept"), "• kept");

        let mut p = from("gen:test:x", "x", &[], ItemKind::News);
        p.lines = vec!["a".repeat(30), "b".repeat(30), "c".repeat(30)];
        let d = p.description(70);
        assert_eq!(d, format!("{}\n{}", "a".repeat(30), "b".repeat(30)));
        assert_eq!(
            p.one_paragraph(20).chars().count(),
            21,
            "cut with an ellipsis"
        );
        p.lines = vec!["x".repeat(200)];
        assert_eq!(p.description(50).chars().count(), 51);
    }
}
