//! The built-in catalog of engines, outlets and sources (`catalog.toml`). A new database
//! starts with the [`SEEDED`] engines; the rest is offered as suggestions in the web UI.

use std::collections::HashSet;
use std::sync::LazyLock;

use crate::config::{Owner, ResolvedSource, SourcesConfig};
use crate::registry::{NewOwner, Snapshot, SourceEdit, SourceRegistry};
use crate::store::Store;

/// Engines a new database starts with.
pub const SEEDED: &[&str] = &["unity", "unreal", "godot"];

/// `meta` key set once the defaults were seeded.
const SEEDED_KEY: &str = "seeded";
/// `meta` key set once the catalog's engine icons were copied into stored engines.
const ICONS_KEY: &str = "catalog_icons_backfilled";

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
    use crate::config::SourceSpec;

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
            source_id: source.into(),
            guid: "1".into(),
            engines: Vec::new(),
            kind: crate::model::ItemKind::News,
            title: "t".into(),
            url: None,
            summary: None,
            published: None,
            story_key: format!("{source} t"),
            notify: false,
            image_url: None,
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

    #[test]
    fn unity_networking_libraries_are_tagged_without_a_bare_mirror_alias() {
        let classifier = crate::classify::Classifier::new(&CATALOG);
        let cases: &[(&str, &[&str])] = &[
            ("Mirror Networking v96.11 released", &["mirror"]),
            ("FishNet 4.7.4 is out", &["fishnet"]),
            ("Fish-Networking adds client prediction", &["fishnet"]),
            ("Fish Networking 4.7 released", &["fishnet"]),
            ("Mirror reflections for water", &[]),
            ("Real-time mirror shaders", &[]),
        ];
        for (title, expected) in cases {
            assert_eq!(classifier.match_engines(title), *expected, "{title}");
        }
    }

    #[test]
    fn game_titles_containing_unity_do_not_tag_unity() {
        let classifier = crate::classify::Classifier::new(&CATALOG);
        let cases: &[(&str, &[&str])] = &[
            ("Assassin's Creed Unity gets a Linux port", &[]),
            ("Assassin’s Creed Unity runs well on Proton", &[]),
            ("Assassins Creed Unity is free this week", &[]),
            ("Unity of Command II DLC out now", &[]),
            ("Unity 6.3 LTS released", &["unity"]),
        ];
        for (title, expected) in cases {
            assert_eq!(classifier.match_engines(title), *expected, "{title}");
        }
    }

    #[test]
    fn unity_networking_libraries_are_tier_2_with_icons() {
        for id in ["mirror", "fishnet"] {
            let engine = CATALOG.engine(id).expect(id);
            assert_eq!(engine.tier, 2, "{id}");
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
