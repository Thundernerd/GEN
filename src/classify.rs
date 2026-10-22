//! Turns `RawItem`s into `NewItem`s: decides the item kind, tags engines and computes the
//! cross-source `story_key`.
//!
//! # Engine alias matching
//!
//! Every enabled engine gets one compiled regex built from its `aliases` and one from its
//! `exclude` phrases. Matching is whole-word with explicit boundaries (a neighbouring char
//! must not be a letter, digit or `_`), so aliases containing symbols such as `s&box`, `c#`
//! or `.net` work, and whitespace inside an alias matches any run of whitespace.
//!
//! Case rule: an all-lowercase alias matches case-insensitively, while an alias containing
//! an uppercase letter matches case-sensitively. This is how the curator avoids false
//! positives on engine names that are ordinary words: writing `"Unity"` instead of `"unity"`
//! tags "Unity 6.3 LTS" but not "a call for unity", while lowercase aliases such as
//! `"unity3d"` or `"unity engine"` keep matching in any case. An `exclude` phrase found
//! anywhere in the text (always case-insensitive) vetoes the engine for that item.

use std::sync::LazyLock;

use regex::Regex;

use crate::config::{Owner, ResolvedSource, SourcesConfig};
use crate::model::{EngineId, ItemKind, NewItem, RawItem};

/// Left boundary: start of text or a char that is not a letter, digit or underscore.
const LEFT: &str = r"(?:^|[^\p{L}\p{N}_])";
/// Right boundary, the mirror of [`LEFT`].
const RIGHT: &str = r"(?:$|[^\p{L}\p{N}_])";

/// Precompiled matchers for one enabled engine.
struct EngineMatcher {
    id: EngineId,
    aliases: Option<Regex>,
    exclude: Option<Regex>,
}

pub struct Classifier {
    engines: Vec<EngineMatcher>,
}

impl Classifier {
    pub fn new(sources: &SourcesConfig) -> Self {
        let engines = sources
            .enabled_engines()
            .map(|e| EngineMatcher {
                id: e.id.clone(),
                aliases: phrase_regex(&e.aliases, true),
                exclude: phrase_regex(&e.exclude, false),
            })
            .collect();
        Classifier { engines }
    }

    /// Returns `None` when the item should be dropped (keyword filter or
    /// `require_engine_match` without a match).
    pub fn classify(&self, source: &ResolvedSource, raw: RawItem) -> Option<NewItem> {
        let text = match &raw.summary {
            Some(summary) => format!("{}\n{summary}", raw.title),
            None => raw.title.clone(),
        };

        if !source.keywords.is_empty() {
            let lower = text.to_lowercase();
            if !source
                .keywords
                .iter()
                .any(|k| lower.contains(&k.to_lowercase()))
            {
                return None;
            }
        }

        let matched = self.match_engines(&text);
        let engines = match &source.owner {
            Owner::Engine(id) => std::iter::once(id.clone())
                .chain(matched.into_iter().filter(|m| m != id))
                .collect(),
            Owner::Outlet(_) => matched,
        };
        if source.require_engine_match && engines.is_empty() {
            return None;
        }

        let kind = match source
            .category
            .unwrap_or_else(|| kind_from_url(raw.url.as_deref()))
        {
            ItemKind::Release => match raw.prerelease {
                Some(true) => ItemKind::Prerelease,
                Some(false) => ItemKind::Release,
                None if looks_prerelease(&raw.title)
                    || (!raw.guid.contains("://") && looks_prerelease(&raw.guid)) =>
                {
                    ItemKind::Prerelease
                }
                None => ItemKind::Release,
            },
            other => other,
        };

        Some(NewItem {
            source_id: source.id.clone(),
            story_key: story_key(&raw.title, &engines),
            guid: raw.guid,
            engines,
            kind,
            title: raw.title,
            url: raw.url.filter(|u| is_http_url(u)),
            summary: raw.summary,
            published: raw.published,
            notify: false,
        })
    }

    /// Enabled engines whose aliases occur in `text` and that no exclude phrase vetoes,
    /// ordered by first occurrence (ties keep config order).
    pub fn match_engines(&self, text: &str) -> Vec<EngineId> {
        let mut hits: Vec<(usize, &EngineId)> = self
            .engines
            .iter()
            .filter(|e| !e.exclude.as_ref().is_some_and(|re| re.is_match(text)))
            .filter_map(|e| Some((e.aliases.as_ref()?.find(text)?.start(), &e.id)))
            .collect();
        hits.sort_by_key(|(pos, _)| *pos);
        hits.into_iter().map(|(_, id)| id.clone()).collect()
    }
}

/// Builds one boundary-delimited regex matching any of `phrases`, or `None` when there are
/// none. See the module docs for the case rule applied when `case_by_alias` is set;
/// otherwise every phrase is case-insensitive.
fn phrase_regex(phrases: &[String], case_by_alias: bool) -> Option<Regex> {
    let mut phrases: Vec<&str> = phrases
        .iter()
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect();
    if phrases.is_empty() {
        return None;
    }
    // Longest first, so the leftmost match prefers the most specific phrase.
    phrases.sort_by_key(|p| std::cmp::Reverse(p.len()));
    let alternatives: Vec<String> = phrases
        .iter()
        .map(|p| {
            let body = p
                .split_whitespace()
                .map(regex::escape)
                .collect::<Vec<_>>()
                .join(r"\s+");
            if case_by_alias && p.chars().any(char::is_uppercase) {
                format!("(?:{body})")
            } else {
                format!("(?i:{body})")
            }
        })
        .collect();
    let pattern = format!("{LEFT}(?:{}){RIGHT}", alternatives.join("|"));
    // Every phrase is passed through `regex::escape`, so the pattern is always valid.
    Some(Regex::new(&pattern).expect("escaped alias regex is valid"))
}

/// Whether `url` is an absolute http(s) URL. Anything else (`javascript:`, `data:`,
/// relative links) from an untrusted source is dropped so it can't become a live link.
pub fn is_http_url(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|u| matches!(u.scheme(), "http" | "https"))
}

/// Default kind for a source without a category, guessed from the item URL.
fn kind_from_url(url: Option<&str>) -> ItemKind {
    let Some(parsed) = url.and_then(|u| url::Url::parse(u).ok()) else {
        return ItemKind::News;
    };
    let host = parsed.host_str().unwrap_or_default().to_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let is = |domain: &str| host == domain || host.ends_with(&format!(".{domain}"));
    if is("youtube.com") || is("youtu.be") {
        ItemKind::Video
    } else if is("reddit.com")
        || host == "news.ycombinator.com"
        || host.starts_with("forum.")
        || host.starts_with("forums.")
        || parsed.path().contains("/t/")
    {
        ItemKind::Community
    } else {
        ItemKind::News
    }
}

/// Prerelease markers as standalone tokens, optionally followed by a number
/// (`beta1`, `-rc.2`, `dev5`). The marker may follow a digit (`1.0rc1`) but not a letter,
/// and must not be followed by a letter, so `alphabet`, `source` or `developer` don't count.
static PRERELEASE_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:^|[^\p{L}])(?:alpha|beta|rc|preview|pre-?release|pre|dev|nightly|daily|snapshot|canary|insiders?)\d*(?:$|[^\p{L}])",
    )
    .expect("valid regex")
});

/// Unity-style versions with an `a`/`b` suffix: `6000.3.0b4`, `7000.0.0a12` (`f1` is final).
static UNITY_PRERELEASE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|[^\p{L}\p{N}])\d+\.\d+\.\d+[ab]\d+(?:$|[^\p{L}\p{N}])").expect("valid regex")
});

/// Whether a release title or tag looks like a prerelease (`4.8-beta1`, `v1.0-preview-5`,
/// `glue-daily`, `6000.3.0b4`, ...).
pub fn looks_prerelease(text: &str) -> bool {
    PRERELEASE_TOKEN.is_match(text) || UNITY_PRERELEASE.is_match(text)
}

/// C#/.NET ecosystem terms. "Mono" is case-sensitive so "mono audio" doesn't count.
static CSHARP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"{LEFT}(?:(?i:c#|f#|asp\.net|\.net|dotnet|nuget|coreclr|il2cpp)|Mono|MONO){RIGHT}"
    ))
    .expect("valid regex")
});

/// Whether `text` mentions C# or the .NET ecosystem (C#, F#, .NET, dotnet, NuGet, CoreCLR,
/// IL2CPP, Mono). Used for the "C#" badge on outlet items.
pub fn mentions_csharp(text: &str) -> bool {
    CSHARP.is_match(text)
}

/// Multi-word phrases that differ between outlets reporting the same story.
const NOISE_PHRASES: [&[&str]; 3] = [&["now", "available"], &["is", "out"], &["out", "now"]];
/// Single noise words, see [`NOISE_PHRASES`].
const NOISE_WORDS: [&str; 5] = [
    "released",
    "release",
    "announcing",
    "announced",
    "available",
];

/// Normalized title plus primary engine, so the same story from different sources collides.
///
/// The title is lowercased, reduced to alphanumeric words and stripped of a few noise words
/// ("released", "now available", ...). Titles with fewer than 3 remaining words yield an
/// empty key, which disables dedup for that item.
///
/// This is deliberately conservative because a false merge hides real news. For example
/// "Godot 4.8 Beta 1 Released" and "Dev snapshot: Godot 4.8 beta 1" do *not* merge: removing
/// outlet-specific prefixes such as "dev snapshot" would need a per-outlet list and makes it
/// easy to collapse distinct posts, while a missed merge only costs one duplicate entry.
pub fn story_key(title: &str, engines: &[String]) -> String {
    let lower = title.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();

    let mut kept = Vec::with_capacity(words.len());
    let mut i = 0;
    while i < words.len() {
        if let Some(phrase) = NOISE_PHRASES.iter().find(|p| words[i..].starts_with(p)) {
            i += phrase.len();
            continue;
        }
        if !NOISE_WORDS.contains(&words[i]) {
            kept.push(words[i]);
        }
        i += 1;
    }

    if kept.len() < 3 {
        return String::new();
    }
    let normalized = kept.join(" ");
    match engines.first() {
        Some(engine) => format!("{engine}|{normalized}"),
        None => normalized,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SourceSpec;

    const CONFIG: &str = r#"
        [[engine]]
        id = "unity"
        name = "Unity"
        tier = 1
        csharp = "native"
        aliases = ["Unity", "unity 6", "unity3d", "unity engine", "urp", "hdrp", "dots"]
        exclude = ["ubuntu unity", "unity desktop"]

        [[engine]]
        id = "unreal"
        name = "Unreal Engine"
        tier = 3
        csharp = "none"
        aliases = ["unreal", "unreal engine", "ue5", "ue6", "uefn", "nanite", "metahuman"]

        [[engine]]
        id = "godot"
        name = "Godot"
        tier = 1
        csharp = "first-class (.NET build)"
        aliases = ["godot", "gdscript"]

        [[engine]]
        id = "stride"
        name = "Stride"
        tier = 1
        csharp = "native"
        aliases = ["stride3d", "stride engine", "xenko"]

        [[engine]]
        id = "flax"
        name = "Flax Engine"
        tier = 1
        csharp = "native"
        aliases = ["flax engine", "flaxengine"]

        [[engine]]
        id = "sbox"
        name = "s&box"
        tier = 2
        csharp = "native"
        aliases = ["s&box", "sbox"]

        [[engine]]
        id = "monogame"
        name = "MonoGame"
        tier = 1
        csharp = "native"
        aliases = ["monogame", "xna", "fna"]

        [[engine]]
        id = "defold"
        name = "Defold"
        tier = 3
        csharp = "none"
        enabled = false
        aliases = ["defold"]
    "#;

    fn classifier() -> Classifier {
        let cfg: SourcesConfig = toml::from_str(CONFIG).unwrap();
        cfg.validate().unwrap();
        Classifier::new(&cfg)
    }

    fn source(owner: Owner, category: Option<ItemKind>) -> ResolvedSource {
        ResolvedSource {
            id: "test:feed:x".into(),
            owner,
            owner_name: "Test".into(),
            category,
            interval_minutes: None,
            keywords: Vec::new(),
            require_engine_match: false,
            spec: SourceSpec::Feed {
                url: "https://example.com/feed".into(),
                cache_bust: false,
            },
        }
    }

    fn outlet(require: bool) -> ResolvedSource {
        ResolvedSource {
            require_engine_match: require,
            ..source(Owner::Outlet("gfs".into()), Some(ItemKind::News))
        }
    }

    fn engine_release(id: &str) -> ResolvedSource {
        source(Owner::Engine(id.into()), Some(ItemKind::Release))
    }

    fn raw(title: &str) -> RawItem {
        RawItem {
            guid: title.into(),
            title: title.into(),
            ..Default::default()
        }
    }

    fn engines_of(c: &Classifier, src: &ResolvedSource, title: &str) -> Option<Vec<String>> {
        c.classify(src, raw(title)).map(|i| i.engines)
    }

    #[test]
    fn keeps_only_http_urls() {
        let c = classifier();
        let src = outlet(false);
        let with_url = |url: &str| RawItem {
            url: Some(url.into()),
            ..raw("Godot news")
        };
        let url_of = |u: &str| c.classify(&src, with_url(u)).unwrap().url;
        assert_eq!(
            url_of("https://godotengine.org/a").as_deref(),
            Some("https://godotengine.org/a")
        );
        assert_eq!(url_of("javascript:alert(1)"), None);
        assert_eq!(url_of("data:text/html,<script>x</script>"), None);
        assert_eq!(url_of("/relative/path"), None);
    }

    #[test]
    fn tags_outlet_items_by_alias() {
        let c = classifier();
        let src = outlet(false);
        let cases: &[(&str, &[&str])] = &[
            ("Godot 4.8 Beta 1 Released", &["godot"]),
            ("New GDScript features", &["godot"]),
            ("Unity 6.3 LTS is here", &["unity"]),
            ("UNITY3D tips", &["unity"]),
            ("HDRP vs URP in 2026", &["unity"]),
            ("Unreal Engine 5.7 released", &["unreal"]),
            ("UE5 Nanite deep dive", &["unreal"]),
            ("s&box goes free to play", &["sbox"]),
            ("Stride3D 4.3 is out", &["stride"]),
            ("Xenko is now Stride", &["stride"]),
            ("FlaxEngine 1.13", &["flax"]),
            ("Flax Engine 1.13 released", &["flax"]),
            ("MonoGame 3.8.4 ships", &["monogame"]),
            ("FNA and XNA preservation", &["monogame"]),
            ("Bevy 0.17 released", &[]),
        ];
        for (title, expected) in cases {
            assert_eq!(
                engines_of(&c, &src, title).unwrap(),
                expected.to_vec(),
                "{title}"
            );
        }
    }

    #[test]
    fn alias_matching_is_whole_word() {
        let c = classifier();
        let src = outlet(false);
        for title in [
            "Godotesque art styles",
            "Unrealistic expectations",
            "sboxes and crates",
            "Flax seeds for breakfast",
            "Polkadots everywhere",
        ] {
            let engines = engines_of(&c, &src, title).unwrap();
            assert!(engines.is_empty(), "{title} -> {engines:?}");
        }
        // Boundaries around symbol-bearing aliases.
        assert_eq!(
            engines_of(&c, &src, "(s&box) update").unwrap(),
            vec!["sbox"]
        );
        assert!(engines_of(&c, &src, "unreal5 news").unwrap().is_empty());
    }

    #[test]
    fn summary_is_matched_too() {
        let c = classifier();
        let item = RawItem {
            summary: Some("This week we look at Godot and MonoGame.".into()),
            ..raw("Weekly roundup")
        };
        let out = c.classify(&outlet(true), item).unwrap();
        assert_eq!(out.engines, vec!["godot", "monogame"]);
    }

    #[test]
    fn multi_engine_items_in_order_of_appearance() {
        let c = classifier();
        let src = outlet(false);
        assert_eq!(
            engines_of(&c, &src, "Unity vs Godot vs Unreal").unwrap(),
            vec!["unity", "godot", "unreal"]
        );
        assert_eq!(
            engines_of(&c, &src, "Unreal or Unity? A Godot user's view").unwrap(),
            vec!["unreal", "unity", "godot"]
        );
    }

    #[test]
    fn plain_word_unity_is_not_tagged() {
        let c = classifier();
        let src = outlet(false);
        assert!(
            engines_of(&c, &src, "A call for unity in the games industry")
                .unwrap()
                .is_empty()
        );
        // Lowercase multi-word aliases still match in any case.
        assert_eq!(
            engines_of(&c, &src, "porting to unity 6 was easy").unwrap(),
            vec!["unity"]
        );
        assert_eq!(
            engines_of(&c, &src, "the unity engine pricing saga").unwrap(),
            vec!["unity"]
        );
    }

    #[test]
    fn excludes_veto_the_engine() {
        let c = classifier();
        let src = outlet(false);
        assert!(
            engines_of(&c, &src, "Ubuntu Unity 24.04 remix released")
                .unwrap()
                .is_empty()
        );
        assert!(
            engines_of(&c, &src, "Unity Desktop gets a new theme")
                .unwrap()
                .is_empty()
        );
        // The veto only affects the excluded engine.
        assert_eq!(
            engines_of(&c, &src, "Ubuntu Unity vs Godot").unwrap(),
            vec!["godot"]
        );
    }

    #[test]
    fn disabled_engines_are_never_tagged() {
        let c = classifier();
        assert!(
            engines_of(&c, &outlet(false), "Defold 1.10 released")
                .unwrap()
                .is_empty()
        );
        assert!(
            c.classify(&outlet(true), raw("Defold 1.10 released"))
                .is_none()
        );
    }

    #[test]
    fn require_engine_match_drops_untagged_items() {
        let c = classifier();
        assert!(
            c.classify(&outlet(true), raw("Bevy 0.17 released"))
                .is_none()
        );
        assert!(
            c.classify(&outlet(false), raw("Bevy 0.17 released"))
                .is_some()
        );
        assert!(
            c.classify(&outlet(true), raw("Godot 4.8 released"))
                .is_some()
        );
    }

    #[test]
    fn engine_owner_comes_first() {
        let c = classifier();
        let src = source(Owner::Engine("godot".into()), Some(ItemKind::News));
        assert_eq!(
            engines_of(&c, &src, "Migrating from Unity to Godot").unwrap(),
            vec!["godot", "unity"]
        );
        assert_eq!(
            engines_of(&c, &src, "Dev snapshot: Godot 4.8 beta 1").unwrap(),
            vec!["godot"]
        );
        assert_eq!(
            engines_of(&c, &src, "Release notes").unwrap(),
            vec!["godot"]
        );
    }

    #[test]
    fn keyword_filter() {
        let c = classifier();
        let src = ResolvedSource {
            keywords: vec!["Evergine".into()],
            ..source(Owner::Engine("evergine".into()), Some(ItemKind::News))
        };
        assert!(c.classify(&src, raw("Plain Concepts at GDC")).is_none());
        let kept = c.classify(&src, raw("EVERGINE 2026.10 is here")).unwrap();
        assert_eq!(kept.engines, vec!["evergine"]);
        let in_summary = RawItem {
            summary: Some("New evergine release with .NET 10".into()),
            ..raw("Company news")
        };
        assert!(c.classify(&src, in_summary).is_some());
    }

    fn kind(c: &Classifier, src: &ResolvedSource, title: &str) -> ItemKind {
        c.classify(src, raw(title)).unwrap().kind
    }

    #[test]
    fn release_kinds_from_version_strings() {
        let c = classifier();
        let src = engine_release("godot");
        let releases = [
            "4.7.2-stable",
            "releases/4.4.1",
            "26.10.13",
            "v20.4.0",
            "Flax 1.13 released",
            "6000.0.23f1",
            "Source code update",
            "Support alphabet input",
            "Improved developer tools",
            "Betamax export",
        ];
        for t in releases {
            assert_eq!(kind(&c, &src, t), ItemKind::Release, "{t}");
        }
        let prereleases = [
            "4.8-beta1",
            "Godot 4.8 beta 1",
            "v1.0-preview-5",
            "v2026.10.0-alpha.6",
            "glue-daily",
            "1.14.1-alpha",
            "2.5.0-beta.33",
            "PreRelease",
            "v3.0.0-pre-release",
            "4.0-rc1",
            "4.0-rc.2",
            "Godot 4.5 RC 2",
            "1.0rc1",
            "4.4-dev5",
            "4.4-dev",
            "nightly-2026-10-21",
            "snapshot 25w41a",
            "v0.9-canary.3",
            "Insider build 12",
            "1.2-pre3",
            "6000.3.0b4",
            "7000.0.0a12",
        ];
        for t in prereleases {
            assert_eq!(kind(&c, &src, t), ItemKind::Prerelease, "{t}");
        }
    }

    #[test]
    fn explicit_prerelease_flag_wins() {
        let c = classifier();
        let src = engine_release("godot");
        let flagged = RawItem {
            prerelease: Some(true),
            ..raw("4.7.2-stable")
        };
        assert_eq!(
            c.classify(&src, flagged).unwrap().kind,
            ItemKind::Prerelease
        );
        let unflagged = RawItem {
            prerelease: Some(false),
            ..raw("4.8-beta1")
        };
        assert_eq!(c.classify(&src, unflagged).unwrap().kind, ItemKind::Release);
    }

    #[test]
    fn guid_is_checked_but_not_urls() {
        let c = classifier();
        let src = engine_release("godot");
        let by_guid = RawItem {
            guid: "v2.0.0-beta.1".into(),
            ..raw("Version 2.0.0")
        };
        assert_eq!(
            c.classify(&src, by_guid).unwrap().kind,
            ItemKind::Prerelease
        );
        let url_guid = RawItem {
            guid: "https://dev.to/some/post".into(),
            ..raw("Version 2.0.0")
        };
        assert_eq!(c.classify(&src, url_guid).unwrap().kind, ItemKind::Release);
    }

    #[test]
    fn non_release_categories_are_kept() {
        let c = classifier();
        for cat in [ItemKind::News, ItemKind::Video, ItemKind::Community] {
            let src = source(Owner::Engine("godot".into()), Some(cat));
            assert_eq!(kind(&c, &src, "Godot 4.8 beta 1"), cat);
        }
        let src = source(Owner::Engine("godot".into()), Some(ItemKind::Prerelease));
        assert_eq!(kind(&c, &src, "4.7.2-stable"), ItemKind::Prerelease);
    }

    #[test]
    fn kind_from_url_without_category() {
        let c = classifier();
        let src = source(Owner::Outlet("misc".into()), None);
        let cases = [
            ("https://www.youtube.com/watch?v=abc", ItemKind::Video),
            ("https://m.youtube.com/watch?v=abc", ItemKind::Video),
            ("https://youtu.be/abc", ItemKind::Video),
            (
                "https://www.reddit.com/r/godot/comments/x",
                ItemKind::Community,
            ),
            ("https://old.reddit.com/r/unity/x", ItemKind::Community),
            (
                "https://news.ycombinator.com/item?id=1",
                ItemKind::Community,
            ),
            (
                "https://forum.godotengine.org/t/hello/123",
                ItemKind::Community,
            ),
            ("https://forums.flaxengine.com/x", ItemKind::Community),
            ("https://discuss.example.org/t/topic/9", ItemKind::Community),
            ("https://gamefromscratch.com/godot-4-8", ItemKind::News),
            ("https://notyoutube.com/watch", ItemKind::News),
            ("not a url", ItemKind::News),
        ];
        for (url, expected) in cases {
            let item = RawItem {
                url: Some(url.into()),
                ..raw("Something happened")
            };
            assert_eq!(c.classify(&src, item).unwrap().kind, expected, "{url}");
        }
        assert_eq!(kind(&c, &src, "No url"), ItemKind::News);
    }

    #[test]
    fn story_key_normalizes_and_merges() {
        let flax = vec!["flax".to_string()];
        assert_eq!(story_key("Flax 1.13 Released", &flax), "flax|flax 1 13");
        assert_eq!(
            story_key("Flax 1.13 Released", &flax),
            story_key("Flax 1.13 released!", &flax)
        );
        let godot = vec!["godot".to_string(), "unity".to_string()];
        assert_eq!(
            story_key("Godot 4.8  is out — now available!", &godot),
            "godot|godot 4 8"
        );
        assert_eq!(
            story_key("Announcing Godot 4.8", &godot),
            story_key("Godot 4.8 Released", &godot)
        );
        assert_eq!(
            story_key("Unity vs Godot vs Unreal", &[]),
            "unity vs godot vs unreal"
        );
    }

    #[test]
    fn story_key_is_conservative() {
        let godot = vec!["godot".to_string()];
        // Intentionally not merged, see `story_key` docs.
        assert_ne!(
            story_key("Godot 4.8 Beta 1 Released", &godot),
            story_key("Dev snapshot: Godot 4.8 beta 1", &godot)
        );
        // Different versions and engines stay apart.
        assert_ne!(
            story_key("Godot 4.8 released", &godot),
            story_key("Godot 4.8.1 released", &godot)
        );
        assert_ne!(
            story_key("Version 2.0 released", &godot),
            story_key("Version 2.0 released", &["flax".to_string()])
        );
        // Too short after normalization: no dedup.
        assert_eq!(story_key("Released!", &godot), "");
        assert_eq!(story_key("Godot 4", &godot), "");
        assert_eq!(story_key("Now available: Flax", &godot), "");
    }

    #[test]
    fn classify_sets_story_key_from_primary_engine() {
        let c = classifier();
        let item = c
            .classify(&outlet(true), raw("Godot 4.8 Beta 1 Released"))
            .unwrap();
        assert_eq!(item.story_key, "godot|godot 4 8 beta 1");
    }

    #[test]
    fn csharp_mentions() {
        for t in [
            "Writing C# scripts",
            "C#-first engine",
            "Built on .NET 10",
            "ASP.NET backend",
            "dotnet new console",
            "Published on NuGet",
            "CoreCLR support lands",
            "IL2CPP build times",
            "Mono runtime removed",
            "F# bindings",
        ] {
            assert!(mentions_csharp(t), "{t}");
        }
        for t in [
            "gamedev.net weekly",
            "Monotone art",
            "mono audio output",
            "C++ only",
            "ABC#1 entry",
            "Godot 4.8 beta 1",
        ] {
            assert!(!mentions_csharp(t), "{t}");
        }
    }
}
