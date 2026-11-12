//! The built-in catalog of engines, outlets and sources (`catalog.toml`). A new database
//! starts with the [`SEEDED`] engines; the rest is offered as suggestions in the web UI.

use std::collections::HashSet;
use std::sync::LazyLock;

use crate::config::{GithubMode, Owner, ResolvedSource, SourceSpec, SourcesConfig, TagPrefixes};
use crate::registry::{NewOwner, Snapshot, SourceEdit, SourceRegistry};
use crate::store::Store;

/// Engines a new database starts with.
pub const SEEDED: &[&str] = &["unity", "unreal", "godot"];

/// `meta` key set once the defaults were seeded.
const SEEDED_KEY: &str = "seeded";
/// `meta` key set once the catalog's engine icons were copied into stored engines.
const ICONS_KEY: &str = "catalog_icons_backfilled";
/// `meta` key set once the catalog's languages were copied into stored engines.
const LANGUAGES_KEY: &str = "catalog_languages_backfilled";
/// `meta` key set once Flame's sources from before #70 were migrated.
const FLAME_SOURCES_KEY: &str = "flame_sources_migrated";
/// Flame's releases source, which kept its id across #70.
const FLAME_RELEASES: &str = "flame:github:flame-engine/flame";
/// Flame's tags source, which #70 dropped from the catalog.
const FLAME_TAGS: &str = "flame:github-tags:flame-engine/flame";

static CATALOG: LazyLock<SourcesConfig> = LazyLock::new(|| {
    let catalog: SourcesConfig =
        toml::from_str(include_str!("catalog.toml")).expect("catalog.toml parses");
    catalog.validate().expect("catalog.toml is valid");
    catalog
});

/// The catalog entry for an engine or outlet, with its sources.
pub fn owner(id: &str) -> Option<NewOwner> {
    let engine = CATALOG.engine(id).cloned().map(NewOwner::Engine);
    engine.or_else(|| {
        CATALOG
            .outlets
            .iter()
            .find(|o| o.id == id)
            .cloned()
            .map(NewOwner::Outlet)
    })
}

/// A catalog source by id.
pub fn source(id: &str) -> Option<ResolvedSource> {
    CATALOG.all_sources().into_iter().find(|s| s.id == id)
}

pub fn has_owner(id: &str) -> bool {
    owner(id).is_some()
}

pub fn has_source(id: &str) -> bool {
    source(id).is_some()
}

/// The languages of the catalog engines, each spelled the catalog's one way.
pub fn languages() -> impl Iterator<Item = &'static str> {
    CATALOG
        .engines
        .iter()
        .flat_map(|e| e.languages.iter().map(String::as_str))
}

/// What the catalog has that the current sources don't.
pub struct Suggestions {
    /// Engines, then outlets, in catalog order (tier 1 → 3 for engines).
    pub owners: Vec<NewOwner>,
    /// Catalog sources of engines and outlets that are present but lack them.
    pub sources: Vec<ResolvedSource>,
}

pub fn suggestions(snapshot: &Snapshot) -> Suggestions {
    let present: HashSet<String> = snapshot
        .config
        .all_sources()
        .into_iter()
        .map(|s| s.id)
        .collect();
    let owners = CATALOG
        .engines
        .iter()
        .filter(|e| !snapshot.has_owner(&e.id))
        .cloned()
        .map(NewOwner::Engine)
        .chain(
            CATALOG
                .outlets
                .iter()
                .filter(|o| !snapshot.has_owner(&o.id))
                .cloned()
                .map(NewOwner::Outlet),
        )
        .collect();
    let owner_present = |owner: &Owner| match owner {
        Owner::Engine(id) => snapshot.config.engine(id).is_some(),
        Owner::Outlet(id) => snapshot.config.outlets.iter().any(|o| o.id == *id),
    };
    let sources = CATALOG
        .all_sources()
        .into_iter()
        .filter(|s| owner_present(&s.owner) && !present.contains(&s.id))
        .collect();
    Suggestions { owners, sources }
}

/// Once per database: adds the [`SEEDED`] engines, then deletes the stored data of sources
/// that aren't configured (left over from before sources moved into the database).
pub fn seed_once(registry: &SourceRegistry, store: &mut Store) -> anyhow::Result<()> {
    if store.meta(SEEDED_KEY)?.is_some() {
        return Ok(());
    }
    for id in SEEDED {
        if registry.current().has_owner(id) {
            continue;
        }
        let owner = owner(id).expect("seeded engines are in the catalog");
        registry
            .apply(store, SourceEdit::AddOwner(owner))
            .map_err(|err| anyhow::anyhow!("seeding {id}: {err}"))?;
    }
    let known: HashSet<String> = registry
        .current()
        .config
        .all_sources()
        .into_iter()
        .map(|s| s.id)
        .collect();
    let orphans: Vec<String> = store
        .stored_source_ids()?
        .into_iter()
        .filter(|id| !known.contains(id))
        .collect();
    if !orphans.is_empty() {
        tracing::info!(
            sources = orphans.len(),
            "deleting data of unconfigured sources"
        );
        store.purge_sources(&orphans)?;
    }
    store.set_meta(SEEDED_KEY, &chrono::Utc::now().to_rfc3339())?;
    Ok(())
}

/// Once per database: gives engines added from the catalog before it had icons the
/// catalog's icon. Engines that already have one keep it.
pub fn backfill_icons_once(registry: &SourceRegistry, store: &mut Store) -> anyhow::Result<()> {
    if store.meta(ICONS_KEY)?.is_some() {
        return Ok(());
    }
    let snapshot = registry.current();
    for engine in snapshot.config.engines.iter().filter(|e| e.icon.is_none()) {
        let Some(icon) = CATALOG.engine(&engine.id).and_then(|e| e.icon.clone()) else {
            continue;
        };
        let edit = SourceEdit::SetEngineIcon {
            engine_id: engine.id.clone(),
            icon: Some(icon),
        };
        registry
            .apply(store, edit)
            .map_err(|err| anyhow::anyhow!("setting the icon of {}: {err}", engine.id))?;
    }
    store.set_meta(ICONS_KEY, &chrono::Utc::now().to_rfc3339())?;
    Ok(())
}

/// Once per database: gives engines stored before engines had `languages` (whose old C#
/// text was read as their `language_note`) the catalog's languages and note. Engines that
/// aren't in the catalog keep the old text as their note.
pub fn backfill_languages_once(registry: &SourceRegistry, store: &mut Store) -> anyhow::Result<()> {
    if store.meta(LANGUAGES_KEY)?.is_some() {
        return Ok(());
    }
    let snapshot = registry.current();
    for engine in snapshot
        .config
        .engines
        .iter()
        .filter(|e| e.languages.is_empty())
    {
        let Some(catalog_engine) = CATALOG.engine(&engine.id) else {
            continue;
        };
        let edit = SourceEdit::SetEngineLanguages {
            engine_id: engine.id.clone(),
            languages: catalog_engine.languages.clone(),
            language_note: catalog_engine.language_note.clone(),
        };
        registry
            .apply(store, edit)
            .map_err(|err| anyhow::anyhow!("setting the languages of {}: {err}", engine.id))?;
    }
    store.set_meta(LANGUAGES_KEY, &chrono::Utc::now().to_rfc3339())?;
    Ok(())
}

/// Once per database: moves Flame from its catalog sources before #70 (a releases source
/// with the `v` prefix and a tags source with `flame-v`, which reads only the 10 entries of
/// `tags.atom`) to the one releases source with both prefixes. The releases source keeps
/// its id, interval and enabled state; the tags source stops being polled but keeps its
/// items. A source the admin changed is left alone, and the tags source is only retired
/// once the releases source covers `flame-v` tags.
pub fn migrate_flame_sources_once(
    registry: &SourceRegistry,
    store: &mut Store,
) -> anyhow::Result<()> {
    if store.meta(FLAME_SOURCES_KEY)?.is_some() {
        return Ok(());
    }
    let flame = |mode, prefixes: &[&str]| SourceSpec::Github {
        repo: "flame-engine/flame".into(),
        mode,
        tag_prefixes: TagPrefixes::new(prefixes),
    };
    let new_releases = source(FLAME_RELEASES)
        .expect("Flame's releases source is in the catalog")
        .spec;
    let spec = |id: &str| {
        registry
            .current()
            .config
            .all_sources()
            .into_iter()
            .find(|s| s.id == id)
            .map(|s| s.spec)
    };
    if spec(FLAME_RELEASES) == Some(flame(GithubMode::Releases, &["v"])) {
        let edit = SourceEdit::ReplaceSourceSpec {
            source_id: FLAME_RELEASES.into(),
            spec: new_releases.clone(),
        };
        registry
            .apply(store, edit)
            .map_err(|err| anyhow::anyhow!("migrating {FLAME_RELEASES}: {err}"))?;
    }
    if spec(FLAME_RELEASES) == Some(new_releases)
        && spec(FLAME_TAGS) == Some(flame(GithubMode::Tags, &["flame-v"]))
    {
        registry
            .apply(store, SourceEdit::RetireSource(FLAME_TAGS.into()))
            .map_err(|err| anyhow::anyhow!("retiring {FLAME_TAGS}: {err}"))?;
    }
    store.set_meta(FLAME_SOURCES_KEY, &chrono::Utc::now().to_rfc3339())?;
    Ok(())
}

/// On every start: adds the catalog's exclude phrases that stored engines of the same id
/// lack, so catalog fixes reach existing databases. Never removes a phrase, and leaves
/// engines that aren't in the catalog alone. An engine the edit fails for is logged and
/// skipped.
pub fn merge_catalog_excludes(registry: &SourceRegistry, store: &mut Store) {
    let snapshot = registry.current();
    for engine in &snapshot.config.engines {
        let Some(catalog_engine) = CATALOG.engine(&engine.id) else {
            continue;
        };
        let missing: Vec<String> = catalog_engine
            .exclude
            .iter()
            .filter(|c| !engine.exclude.iter().any(|p| p.eq_ignore_ascii_case(c)))
            .cloned()
            .collect();
        if missing.is_empty() {
            continue;
        }
        let added = missing.len();
        let edit = SourceEdit::AddEngineExcludes {
            engine_id: engine.id.clone(),
            phrases: missing,
        };
        // This runs on every start, so a failure must not keep GEN from starting.
        match registry.apply(store, edit) {
            Ok(_) => tracing::info!(engine = %engine.id, added, "added catalog exclude phrases"),
            Err(err) => {
                tracing::warn!(engine = %engine.id, "not adding catalog exclude phrases: {err}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{SourceSpec, TagPrefixes, Tier};
    use std::collections::HashMap;

    #[test]
    fn catalog_is_valid_without_noisy_channels() {
        for id in SEEDED {
            assert!(CATALOG.engine(id).is_some(), "{id}");
        }
        for s in CATALOG.all_sources() {
            assert!(!matches!(s.spec, SourceSpec::Hn { .. }), "{}", s.id);
            for banned in ["youtube.com", "reddit.com", "/latest.rss"] {
                assert!(!s.id.contains(banned), "{}", s.id);
            }
        }
        assert!(CATALOG.engines.iter().all(|e| e.enabled));
        assert!(CATALOG.outlets.iter().all(|o| o.enabled));
    }

    #[test]
    fn every_catalog_engine_has_languages_spelled_one_way() {
        let mut spellings: HashMap<String, &str> = HashMap::new();
        for engine in &CATALOG.engines {
            assert!(
                !engine.languages.is_empty(),
                "{} has no languages",
                engine.id
            );
            for language in &engine.languages {
                let spelled = spellings.entry(language.to_lowercase()).or_insert(language);
                assert_eq!(spelled, language, "{} spells it differently", engine.id);
            }
        }
    }

    #[test]
    fn language_notes_add_what_the_languages_list_cannot() {
        let dotnet_version = regex::Regex::new(r"(?i)\.net \d").unwrap();
        for engine in &CATALOG.engines {
            let Some(note) = &engine.language_note else {
                continue;
            };
            let lower = note.to_lowercase();
            assert!(!dotnet_version.is_match(note), "{}: {note}", engine.id);
            for banned in ["hot reload", "written in c#", "plugin", "raylib-cs"] {
                assert!(!lower.contains(banned), "{}: {note}", engine.id);
            }
            // "C# framework" on a C#-only engine only repeats the list.
            if let [language] = &engine.languages[..] {
                assert!(
                    !lower.starts_with(&language.to_lowercase()),
                    "{}: {note}",
                    engine.id
                );
            }
        }
    }

    #[test]
    fn godot_tracks_prereleases_from_godot_builds() {
        // Godot's dev, beta and RC builds are only published in godot-builds.
        assert!(has_source("godot:github:godotengine/godot-builds"));
        assert!(has_source("godot:github:godotengine/godot"));
    }

    #[test]
    fn offers_gamingonlinux_as_an_engine_match_only_outlet() {
        let Some(NewOwner::Outlet(outlet)) = owner("gamingonlinux") else {
            panic!("gamingonlinux is not a catalog outlet");
        };
        assert_eq!(outlet.name, "GamingOnLinux");
        assert!(outlet.require_engine_match);
        let specs: Vec<_> = outlet.sources.iter().map(|s| &s.spec).collect();
        assert!(
            matches!(&specs[..], [SourceSpec::Feed { url, .. }]
                if url == "https://www.gamingonlinux.com/article_rss.php"),
            "{specs:?}"
        );
    }

    #[test]
    fn seeds_once_and_purges_unconfigured_sources() {
        let mut store = Store::open_in_memory().unwrap();
        let item = |source: &str| crate::model::NewItem {
            engines: Vec::new(),
            kind: crate::model::ItemKind::News,
            title: "t".into(),
            story_key: format!("{source} t"),
            ..crate::model::NewItem::test(source, "1")
        };
        let godot = "godot:github:godotengine/godot";
        store
            .insert_items(&[item(godot), item("hackernews:hn:godot")])
            .unwrap();
        let reg = SourceRegistry::load(&store).unwrap();
        seed_once(&reg, &mut store).unwrap();

        let snapshot = reg.current();
        let engines: Vec<_> = snapshot.config.engines.iter().map(|e| &e.id).collect();
        assert_eq!(engines, SEEDED);
        assert!(snapshot.config.outlets.is_empty());
        assert_eq!(store.stored_source_ids().unwrap(), [godot]);

        // Removing a seeded engine sticks: it is not seeded again.
        reg.apply(&mut store, SourceEdit::RemoveOwner("unity".into()))
            .unwrap();
        seed_once(&reg, &mut store).unwrap();
        assert!(reg.current().config.engine("unity").is_none());

        let suggested = suggestions(&reg.current());
        assert_eq!(suggested.owners[0].id(), "unity");
        assert!(suggested.owners.iter().any(|o| o.id() == "gamefromscratch"));
        assert!(suggested.sources.is_empty());
        reg.apply(&mut store, SourceEdit::RemoveSource(godot.into()))
            .unwrap();
        let suggested = suggestions(&reg.current());
        assert_eq!(suggested.sources.len(), 1);
        assert_eq!(suggested.sources[0].id, godot);
    }

    #[test]
    fn backfills_engine_icons_once() {
        let mut store = Store::open_in_memory().unwrap();
        let reg = SourceRegistry::load(&store).unwrap();
        // Godot as stored before the catalog had icons, plus an engine of the user's own.
        let mut godot = CATALOG.engine("godot").cloned().unwrap();
        godot.icon = None;
        godot.sources.clear();
        reg.apply(&mut store, SourceEdit::AddOwner(NewOwner::Engine(godot)))
            .unwrap();
        let mut own = CATALOG.engine("godot").cloned().unwrap();
        own.id = "mine".into();
        own.icon = None;
        own.sources.clear();
        reg.apply(&mut store, SourceEdit::AddOwner(NewOwner::Engine(own)))
            .unwrap();

        backfill_icons_once(&reg, &mut store).unwrap();
        let icon =
            |reg: &SourceRegistry, id: &str| reg.current().config.engine(id).unwrap().icon.clone();
        assert_eq!(icon(&reg, "godot"), CATALOG.engine("godot").unwrap().icon);
        assert!(icon(&reg, "godot").is_some());
        assert_eq!(icon(&reg, "mine"), None, "not in the catalog");
        // Stored, so it survives a restart.
        let reloaded = SourceRegistry::load(&store).unwrap();
        assert!(icon(&reloaded, "godot").is_some());

        // Only once: an icon removed later stays removed.
        reg.apply(
            &mut store,
            SourceEdit::SetEngineIcon {
                engine_id: "godot".into(),
                icon: None,
            },
        )
        .unwrap();
        backfill_icons_once(&reg, &mut store).unwrap();
        assert_eq!(icon(&reg, "godot"), None);
    }

    #[test]
    fn backfills_engine_languages_once() {
        let mut store = Store::open_in_memory().unwrap();
        store_old_engine(&mut store, "godot", "first-class (.NET build)");
        store_old_engine(&mut store, "mine", "none (Rust)");
        let reg = SourceRegistry::load(&store).unwrap();

        backfill_languages_once(&reg, &mut store).unwrap();
        let languages = |reg: &SourceRegistry, id: &str| {
            let snapshot = reg.current();
            let engine = snapshot.config.engine(id).unwrap();
            (engine.languages.clone(), engine.language_note.clone())
        };
        assert_eq!(
            languages(&reg, "godot"),
            (
                vec!["GDScript".into(), "C#".into(), "C++".into()],
                Some("C# needs the .NET build".into())
            )
        );
        assert_eq!(
            languages(&reg, "mine"),
            (Vec::new(), Some("none (Rust)".into())),
            "not in the catalog"
        );
        // Stored, so it survives a restart.
        let reloaded = SourceRegistry::load(&store).unwrap();
        assert_eq!(languages(&reloaded, "godot").0.len(), 3);

        // Only once: an engine stored in the old shape later is left alone.
        store_old_engine(&mut store, "unity", "native (C# scripting)");
        let reg = SourceRegistry::load(&store).unwrap();
        backfill_languages_once(&reg, &mut store).unwrap();
        assert!(languages(&reg, "unity").0.is_empty());
    }

    /// Stores engine `id` as JSON in the shape used before `languages` existed.
    fn store_old_engine(store: &mut Store, id: &str, csharp: &str) {
        let json = format!(
            r#"{{"id":"{id}","name":"{id}","tier":1,"csharp":"{csharp}","homepage":null,
                "enabled":true,"aliases":[],"exclude":[],"source":[]}}"#
        );
        store.add_owner(id, "engine", &json, &[]).unwrap();
    }

    /// Unity as stored before #13, with a phrase of the user's own, plus an engine that
    /// isn't in the catalog.
    fn store_with_old_unity() -> (Store, SourceRegistry) {
        let mut store = Store::open_in_memory().unwrap();
        let reg = SourceRegistry::load(&store).unwrap();
        let mut unity = CATALOG.engine("unity").cloned().unwrap();
        unity.exclude = ["ubuntu unity", "unity desktop", "unity shell", "unitypoint"]
            .map(String::from)
            .into();
        unity.exclude.push("unity tower".into());
        unity.sources.clear();
        reg.apply(&mut store, SourceEdit::AddOwner(NewOwner::Engine(unity)))
            .unwrap();
        let mut own = CATALOG.engine("unity").cloned().unwrap();
        own.id = "mine".into();
        own.exclude = vec!["ubuntu unity".into()];
        own.sources.clear();
        reg.apply(&mut store, SourceEdit::AddOwner(NewOwner::Engine(own)))
            .unwrap();
        (store, reg)
    }

    fn excludes(reg: &SourceRegistry, id: &str) -> Vec<String> {
        reg.current().config.engine(id).unwrap().exclude.clone()
    }

    #[test]
    fn merges_catalog_excludes_into_stored_engines() {
        let (mut store, reg) = store_with_old_unity();

        merge_catalog_excludes(&reg, &mut store);

        let unity = excludes(&reg, "unity");
        for phrase in &CATALOG.engine("unity").unwrap().exclude {
            assert!(unity.contains(phrase), "{phrase} missing from {unity:?}");
        }
        assert!(
            unity.contains(&"unity tower".to_string()),
            "user's phrase kept"
        );
        // Stored, so it survives a restart.
        let reloaded = SourceRegistry::load(&store).unwrap();
        assert_eq!(excludes(&reloaded, "unity"), unity);
    }

    #[test]
    fn merging_catalog_excludes_leaves_engines_outside_the_catalog_alone() {
        let (mut store, reg) = store_with_old_unity();

        merge_catalog_excludes(&reg, &mut store);

        assert_eq!(excludes(&reg, "mine"), ["ubuntu unity"]);
    }

    #[test]
    fn merging_catalog_excludes_twice_adds_nothing() {
        let (mut store, reg) = store_with_old_unity();
        merge_catalog_excludes(&reg, &mut store);
        let once = excludes(&reg, "unity");

        merge_catalog_excludes(&reg, &mut store);

        assert_eq!(excludes(&reg, "unity"), once);
        assert_eq!(
            excludes(&SourceRegistry::load(&store).unwrap(), "unity"),
            once
        );
    }

    #[test]
    fn merged_catalog_excludes_stop_the_classifier_tagging_game_titles() {
        let (mut store, reg) = store_with_old_unity();
        let title = "Assassin's Creed Unity gets a Linux port";
        assert!(
            reg.current()
                .classifier
                .match_engines(title)
                .contains(&"unity".into()),
            "the old excludes tag it"
        );

        merge_catalog_excludes(&reg, &mut store);

        let tags = reg.current().classifier.match_engines(title);
        assert!(!tags.contains(&"unity".into()), "{tags:?}");
    }

    /// Flame as added from the catalog before #70: a releases source with the `v` prefix and
    /// a tags source with the `flame-v` prefix. `releases_prefix` stands in for the first.
    fn store_with_old_flame(releases_prefix: &str) -> (Store, SourceRegistry) {
        let mut store = Store::open_in_memory().unwrap();
        let toml = format!(
            r#"
            [[engine]]
            id = "flame"
            name = "Flame"
            tier = 3
            languages = ["Dart"]
              [[engine.source]]
              kind = "github"
              repo = "flame-engine/flame"
              tag_prefix = "{releases_prefix}"
              category = "release"
              interval_minutes = 90
              [[engine.source]]
              kind = "github"
              repo = "flame-engine/flame"
              mode = "tags"
              tag_prefix = "flame-v"
              category = "release"
            "#
        );
        let reg = SourceRegistry::with_owners(&mut store, &toml);
        (store, reg)
    }

    /// The stored Flame sources: (id, tag prefixes, interval).
    fn flame_sources(reg: &SourceRegistry) -> Vec<(String, TagPrefixes, Option<u64>)> {
        reg.current()
            .config
            .all_sources()
            .into_iter()
            .map(|s| {
                let SourceSpec::Github { tag_prefixes, .. } = &s.spec else {
                    panic!("{} is not a GitHub source", s.id);
                };
                (s.id.clone(), tag_prefixes.clone(), s.interval_minutes)
            })
            .collect()
    }

    #[test]
    fn migrates_old_flame_sources_to_one_releases_source() {
        let (mut store, reg) = store_with_old_flame("v");
        let tag_item = crate::model::NewItem {
            engines: vec!["flame".into()],
            ..crate::model::NewItem::test(FLAME_TAGS, "flame-v2.0.0")
        };
        let tag_item = store.insert_items(&[tag_item]).unwrap().remove(0);

        migrate_flame_sources_once(&reg, &mut store).unwrap();

        let migrated = vec![(
            FLAME_RELEASES.to_string(),
            TagPrefixes::new(["v", "flame-v"]),
            Some(90),
        )];
        assert_eq!(flame_sources(&reg), migrated);
        // Stored, so it survives a restart.
        let reloaded = SourceRegistry::load(&store).unwrap();
        assert_eq!(flame_sources(&reloaded), migrated);
        // The tags source only stops being polled: its items stay.
        assert!(store.item(tag_item.id).unwrap().is_some());
    }

    #[test]
    fn removing_flame_after_the_migration_deletes_the_retired_sources_items() {
        let (mut store, reg) = store_with_old_flame("v");
        let tag_item = crate::model::NewItem {
            engines: vec!["flame".into()],
            ..crate::model::NewItem::test(FLAME_TAGS, "flame-v2.0.0")
        };
        let tag_item = store.insert_items(&[tag_item]).unwrap().remove(0);
        migrate_flame_sources_once(&reg, &mut store).unwrap();

        reg.apply(&mut store, SourceEdit::RemoveOwner("flame".into()))
            .unwrap();

        assert!(store.item(tag_item.id).unwrap().is_none());
        assert!(store.stored_source_ids().unwrap().is_empty());
    }

    #[test]
    fn migrating_flame_sources_twice_changes_nothing() {
        let (mut store, reg) = store_with_old_flame("v");
        migrate_flame_sources_once(&reg, &mut store).unwrap();
        // The admin adds the tags source back after the migration.
        let tags = source_config(
            r#"kind = "github"
            repo = "flame-engine/flame"
            mode = "tags"
            tag_prefix = "flame-v""#,
        );
        reg.apply(
            &mut store,
            SourceEdit::AddSource {
                owner_id: "flame".into(),
                config: tags,
            },
        )
        .unwrap();
        let before = flame_sources(&reg);

        let restarted = SourceRegistry::load(&store).unwrap();
        migrate_flame_sources_once(&restarted, &mut store).unwrap();

        assert_eq!(flame_sources(&restarted), before);
        assert!(before.iter().any(|(id, ..)| id == FLAME_TAGS));
    }

    #[test]
    fn migrating_flame_sources_leaves_an_admin_chosen_prefix_alone() {
        let (mut store, reg) = store_with_old_flame("v1.");
        let before = flame_sources(&reg);

        migrate_flame_sources_once(&reg, &mut store).unwrap();

        assert_eq!(flame_sources(&reg), before);
        assert_eq!(
            flame_sources(&SourceRegistry::load(&store).unwrap()),
            before
        );
        assert_eq!(before.len(), 2, "{before:?}");
    }

    fn source_config(toml: &str) -> crate::config::SourceConfig {
        toml::from_str(toml).unwrap()
    }

    /// Asserts which catalog engines each outlet title is tagged with.
    fn assert_tags(cases: &[(&str, &[&str])]) {
        let classifier = crate::classify::Classifier::new(&CATALOG);
        for (title, expected) in cases {
            assert_eq!(classifier.match_engines(title), *expected, "{title}");
        }
    }

    #[test]
    fn unity_networking_libraries_are_tagged_without_a_bare_mirror_alias() {
        assert_tags(&[
            ("Mirror Networking v96.11 released", &["mirror"]),
            ("FishNet 4.7.4 is out", &["fishnet"]),
            ("Fish-Networking adds client prediction", &["fishnet"]),
            ("Fish Networking 4.7 released", &["fishnet"]),
            ("Mirror reflections for water", &[]),
            ("Real-time mirror shaders", &[]),
        ]);
    }

    #[test]
    fn game_titles_containing_unity_do_not_tag_unity() {
        assert_tags(&[
            ("Assassin's Creed Unity gets a Linux port", &[]),
            ("Assassin’s Creed Unity runs well on Proton", &[]),
            ("Assassins Creed Unity is free this week", &[]),
            ("Unity of Command II DLC out now", &[]),
            ("Unity 6.3 LTS released", &["unity"]),
        ]);
    }

    #[test]
    fn love_is_tagged_by_its_name_not_by_the_word_love() {
        assert_tags(&[
            ("I love this game", &[]),
            ("I LOVE 2D platformers", &[]),
            ("LÖVE 12.0 released", &["love"]),
            ("Making a roguelike in Love2D", &["love"]),
        ]);
    }

    #[test]
    fn heaps_is_tagged_by_its_name_not_by_the_word_heaps() {
        assert_tags(&[
            ("Heaps of new games on Steam this week", &[]),
            ("Binary heaps for A* pathfinding", &[]),
            ("Heaps.io 2.1 released", &["heaps"]),
            ("Shiro Games' Heaps engine goes open", &["heaps"]),
        ]);
    }

    #[test]
    fn flame_is_tagged_by_its_name_not_by_the_word_flame() {
        assert_tags(&[
            ("Fire and flame effects with particles", &[]),
            ("Flame Keeper is out on Steam", &[]),
            ("Flame engine 1.38 adds a new camera", &["flame"]),
            ("Building a Flutter Flame game", &["flame"]),
        ]);
    }

    #[test]
    fn sdl_is_tagged_by_its_name_only() {
        assert_tags(&[
            ("SDLC best practices for game studios", &[]),
            ("Using the sdl tool in Windows SDK", &[]),
            ("SDL 3.4.20 released", &["sdl"]),
            ("Porting a game from SDL2 to SDL3", &["sdl"]),
            ("Simple DirectMedia Layer gets a GPU API", &["sdl"]),
        ]);
    }

    #[test]
    fn tiers_measure_prominence_and_kind_not_language() {
        let tier = |id: &str| CATALOG.engine(id).expect(id).tier;
        // 1 = major engines, of any language.
        for id in [
            "unity",
            "unreal",
            "godot",
            "bevy",
            "defold",
            "gamemaker",
            "babylonjs",
        ] {
            assert_eq!(tier(id), Tier::Major, "{id}");
        }
        // 2 = smaller engines, C# or not.
        for id in ["prowl", "lodzero", "evergine", "cryengine"] {
            assert_eq!(tier(id), Tier::Smaller, "{id}");
        }
        // 3 = frameworks and libraries, C# or not.
        for id in ["monogame", "silknet", "arch", "bepu", "raylib", "phaser"] {
            assert_eq!(tier(id), Tier::Frameworks, "{id}");
        }
    }

    #[test]
    fn unity_networking_libraries_are_tier_3_with_icons() {
        for id in ["mirror", "fishnet"] {
            let engine = CATALOG.engine(id).expect(id);
            assert_eq!(engine.tier, Tier::Frameworks, "{id}");
            assert!(engine.icon.is_some(), "{id}");
        }
    }

    #[test]
    fn unreal_tracks_unrealsharp_commits_disabled_by_default() {
        let s = source("unreal:github-commits:UnrealSharp/UnrealSharp").expect("in catalog");
        assert_eq!(s.owner, Owner::Engine("unreal".into()));
        assert!(!s.enabled, "commits are noisy and Unreal is seeded");
    }

    #[test]
    fn catalog_icons_are_web_images() {
        for e in &CATALOG.engines {
            if let Some(icon) = &e.icon {
                assert!(icon.starts_with("https://"), "{}: {icon}", e.id);
                assert!(
                    !icon.ends_with(".svg") && !icon.ends_with(".ico"),
                    "{}: {icon}",
                    e.id
                );
            }
        }
        for id in SEEDED {
            assert!(CATALOG.engine(id).unwrap().icon.is_some(), "{id}");
        }
    }
}
