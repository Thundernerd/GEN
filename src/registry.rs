//! The live set of engines, outlets and sources: `sources.toml` plus the edits made in the
//! web UI (stored in the database). Each change publishes a new [`Snapshot`]; readers take
//! the current one and subscribers (the scheduler) are woken.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::watch;

use crate::classify::Classifier;
use crate::config::{Owner, ResolvedSource, SourceConfig, SourcesConfig, source_id};
use crate::model::Item;
use crate::store::{Removed, Store};

/// One consistent view of the sources, with everything derived from it.
pub struct Snapshot {
    /// `sources.toml` with the edits applied.
    pub config: SourcesConfig,
    pub classifier: Classifier,
    pub lookup: Lookup,
    pub edits: SourceEdits,
}

impl Snapshot {
    /// Applies `edits` to `baseline`. Edits that no longer fit (e.g. a source whose owner
    /// is gone, or one that sources.toml now defines too) are skipped with a warning, so a
    /// hand-edited sources.toml can't stop GEN from starting.
    fn build(baseline: &SourcesConfig, edits: SourceEdits) -> anyhow::Result<Self> {
        let mut config = baseline.clone();
        config
            .engines
            .retain(|e| !edits.removed_owners.contains(&e.id));
        config
            .outlets
            .retain(|o| !edits.removed_owners.contains(&o.id));
        for e in &mut config.engines {
            e.sources
                .retain(|s| !edits.removed_sources.contains(&source_id(&e.id, &s.spec)));
        }
        for o in &mut config.outlets {
            o.sources
                .retain(|s| !edits.removed_sources.contains(&source_id(&o.id, &s.spec)));
        }

        let mut ids: HashSet<String> = config.all_sources().into_iter().map(|s| s.id).collect();
        for custom in &edits.sources {
            let sources = config
                .engines
                .iter_mut()
                .find(|e| e.id == custom.owner_id)
                .map(|e| &mut e.sources)
                .or_else(|| {
                    config
                        .outlets
                        .iter_mut()
                        .find(|o| o.id == custom.owner_id)
                        .map(|o| &mut o.sources)
                });
            match sources {
                Some(sources) if ids.insert(custom.id.clone()) => {
                    sources.push(custom.config.clone());
                }
                Some(_) => {
                    tracing::warn!(source = %custom.id, "skipping added source: it exists already")
                }
                None => {
                    tracing::warn!(source = %custom.id, "skipping added source: its engine or outlet is gone")
                }
            }
        }

        config.validate()?;
        Ok(Snapshot {
            classifier: Classifier::new(&config),
            lookup: Lookup::new(&config),
            config,
            edits,
        })
    }

    /// Whether a source was added in the web UI (rather than coming from sources.toml).
    pub fn is_custom_source(&self, id: &str) -> bool {
        self.edits.sources.iter().any(|s| s.id == id)
    }
}

/// Changes made in the web UI on top of `sources.toml`.
#[derive(Debug, Clone, Default)]
pub struct SourceEdits {
    pub sources: Vec<CustomSource>,
    pub removed_owners: BTreeSet<String>,
    pub removed_sources: BTreeSet<String>,
}

/// A source added in the web UI.
#[derive(Debug, Clone)]
pub struct CustomSource {
    pub id: String,
    pub owner_id: String,
    pub config: SourceConfig,
}

impl SourceEdits {
    fn load(store: &Store) -> anyhow::Result<Self> {
        let mut sources = Vec::new();
        for (id, owner_id, json) in store.custom_sources()? {
            match serde_json::from_str(&json) {
                Ok(config) => sources.push(CustomSource {
                    id,
                    owner_id,
                    config,
                }),
                Err(err) => tracing::warn!(source = %id, "skipping unreadable added source: {err}"),
            }
        }
        Ok(SourceEdits {
            sources,
            removed_owners: store.removed(Removed::Owner)?.into_iter().collect(),
            removed_sources: store.removed(Removed::Source)?.into_iter().collect(),
        })
    }
}

/// A change requested in the web UI.
#[derive(Debug, Clone)]
pub enum SourceEdit {
    AddSource {
        owner_id: String,
        config: SourceConfig,
    },
    RemoveSource(String),
    RestoreSource(String),
}

#[derive(Debug, thiserror::Error)]
pub enum EditError {
    /// The change was refused; the message is for the user.
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

pub struct SourceRegistry {
    baseline: SourcesConfig,
    tx: watch::Sender<Arc<Snapshot>>,
}

impl SourceRegistry {
    /// Starts from `baseline` (sources.toml) with the edits stored in `store`.
    pub fn load(baseline: SourcesConfig, store: &Store) -> anyhow::Result<Self> {
        let snapshot = Snapshot::build(&baseline, SourceEdits::load(store)?)?;
        let (tx, _) = watch::channel(Arc::new(snapshot));
        Ok(SourceRegistry { baseline, tx })
    }

    /// The current snapshot. Cheap; hold it for one request or poll, not across changes.
    pub fn current(&self) -> Arc<Snapshot> {
        self.tx.borrow().clone()
    }

    /// A receiver that is marked changed whenever a new snapshot is published.
    pub fn subscribe(&self) -> watch::Receiver<Arc<Snapshot>> {
        self.tx.subscribe()
    }

    /// Sources from sources.toml that were removed in the web UI and can be restored.
    pub fn removed_sources(&self) -> Vec<ResolvedSource> {
        let current = self.current();
        let present: HashSet<String> = current
            .config
            .all_sources()
            .into_iter()
            .map(|s| s.id)
            .collect();
        self.baseline
            .all_sources()
            .into_iter()
            .filter(|s| current.edits.removed_sources.contains(&s.id) && !present.contains(&s.id))
            .filter(|s| !current.edits.removed_owners.contains(s.owner.id()))
            .collect()
    }

    /// Checks `edit`, stores it and publishes the new snapshot. Returns the ids of sources
    /// whose data was deleted. Callers must serialize calls (they run under the store lock).
    pub fn apply(&self, store: &mut Store, edit: SourceEdit) -> Result<Vec<String>, EditError> {
        let current = self.current();
        let mut edits = current.edits.clone();
        match edit {
            SourceEdit::AddSource { owner_id, config } => {
                config.check().map_err(EditError::Invalid)?;
                let owners = current.config.manageable_owners();
                let Some((_, owner_name)) = owners.iter().find(|(o, _)| o.id() == owner_id) else {
                    return Err(EditError::Invalid(format!(
                        "There is no engine or outlet “{owner_id}”."
                    )));
                };
                let id = source_id(&owner_id, &config.spec);
                if current.config.all_sources().iter().any(|s| s.id == id) {
                    return Err(EditError::Invalid(format!(
                        "{owner_name} already has this source."
                    )));
                }
                let json = serde_json::to_string(&config).map_err(anyhow::Error::from)?;
                edits.sources.push(CustomSource {
                    id: id.clone(),
                    owner_id: owner_id.clone(),
                    config,
                });
                self.commit(edits, || store.add_custom_source(&id, &owner_id, &json))?;
                tracing::info!(source = %id, "source added");
                Ok(Vec::new())
            }
            SourceEdit::RemoveSource(id) => {
                if !current.config.all_sources().iter().any(|s| s.id == id) {
                    return Err(EditError::Invalid(format!("There is no source “{id}”.")));
                }
                // Added here: delete it. From sources.toml: leave a tombstone.
                let before = edits.sources.len();
                edits.sources.retain(|s| s.id != id);
                let tombstone = edits.sources.len() == before;
                if tombstone {
                    edits.removed_sources.insert(id.clone());
                }
                self.commit(edits, || store.remove_source(&id, tombstone))?;
                tracing::info!(source = %id, "source removed");
                Ok(vec![id])
            }
            SourceEdit::RestoreSource(id) => {
                if !self.removed_sources().iter().any(|s| s.id == id) {
                    return Err(EditError::Invalid(format!(
                        "“{id}” is not a removed source that can be restored."
                    )));
                }
                edits.removed_sources.remove(&id);
                self.commit(edits, || store.restore(Removed::Source, &id))?;
                tracing::info!(source = %id, "source restored");
                Ok(Vec::new())
            }
        }
    }

    /// Builds the snapshot for `edits` (so nothing is stored if it is invalid), stores the
    /// change and publishes the snapshot.
    fn commit(
        &self,
        edits: SourceEdits,
        persist: impl FnOnce() -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let snapshot = Snapshot::build(&self.baseline, edits)?;
        persist()?;
        self.tx.send_replace(Arc::new(snapshot));
        Ok(())
    }
}

/// Name lookups derived from a snapshot of the sources.
pub struct Lookup {
    /// Source id -> owner display name, for every source (including disabled ones).
    pub source_names: HashMap<String, String>,
    /// Ids of sources that belong to general outlets rather than an engine.
    pub outlet_sources: HashSet<String>,
    /// Engine id -> display name.
    pub engine_names: HashMap<String, String>,
}

impl Lookup {
    pub fn new(sources: &SourcesConfig) -> Self {
        let all = sources.all_sources();
        Lookup {
            source_names: all
                .iter()
                .map(|s| (s.id.clone(), s.owner_name.clone()))
                .collect(),
            outlet_sources: all
                .iter()
                .filter(|s| matches!(s.owner, Owner::Outlet(_)))
                .map(|s| s.id.clone())
                .collect(),
            engine_names: sources
                .engines
                .iter()
                .map(|e| (e.id.clone(), e.name.clone()))
                .collect(),
        }
    }

    /// Display name of a source's owner, falling back to the raw id for unknown sources.
    pub fn source_name(&self, source_id: &str) -> String {
        self.source_names
            .get(source_id)
            .cloned()
            .unwrap_or_else(|| source_id.to_string())
    }

    pub fn engine_name(&self, engine_id: &str) -> String {
        self.engine_names
            .get(engine_id)
            .cloned()
            .unwrap_or_else(|| engine_id.to_string())
    }

    /// Distinct owner names of the given duplicate sources, minus the item's own owner.
    pub fn also_on(&self, item: &Item, dup_sources: Option<&Vec<String>>) -> Vec<String> {
        let own = self.source_name(&item.source_id);
        let mut names: Vec<String> = Vec::new();
        for id in dup_sources.into_iter().flatten() {
            let name = self.source_name(id);
            if name != own && !names.contains(&name) {
                names.push(name);
            }
        }
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{GithubMode, SourceSpec};

    const BASELINE: &str = r#"
        [[engine]]
        id = "godot"
        name = "Godot"
        tier = 1
        csharp = "first-class"
        aliases = ["godot"]
          [[engine.source]]
          kind = "github"
          repo = "godotengine/godot"

        [[outlet]]
        id = "gfs"
        name = "GameFromScratch"
          [[outlet.source]]
          kind = "feed"
          url = "https://gamefromscratch.com/feed/"
    "#;
    const GODOT_SRC: &str = "godot:github:godotengine/godot";

    fn registry(store: &Store) -> SourceRegistry {
        SourceRegistry::load(toml::from_str(BASELINE).unwrap(), store).unwrap()
    }

    fn ids(registry: &SourceRegistry) -> Vec<String> {
        registry
            .current()
            .config
            .manageable_sources()
            .into_iter()
            .map(|s| s.id)
            .collect()
    }

    fn hn(query: &str) -> SourceConfig {
        SourceConfig {
            category: None,
            enabled: true,
            interval_minutes: None,
            keywords: Vec::new(),
            spec: SourceSpec::Hn {
                query: query.into(),
            },
        }
    }

    fn add(owner_id: &str, config: SourceConfig) -> SourceEdit {
        SourceEdit::AddSource {
            owner_id: owner_id.into(),
            config,
        }
    }

    #[test]
    fn adds_and_removes_sources_and_survives_reload() {
        let mut store = Store::open_in_memory().unwrap();
        let reg = registry(&store);
        let changes = reg.subscribe();

        reg.apply(&mut store, add("godot", hn("godot engine")))
            .unwrap();
        assert!(changes.has_changed().unwrap());
        assert!(ids(&reg).contains(&"godot:hn:godot engine".to_string()));
        assert!(reg.current().is_custom_source("godot:hn:godot engine"));

        // A config source gets a tombstone and can be restored.
        let purged = reg
            .apply(&mut store, SourceEdit::RemoveSource(GODOT_SRC.into()))
            .unwrap();
        assert_eq!(purged, [GODOT_SRC]);
        assert!(!ids(&reg).contains(&GODOT_SRC.to_string()));
        assert_eq!(reg.removed_sources()[0].id, GODOT_SRC);

        // Everything is read back from the store.
        let reloaded = registry(&store);
        assert_eq!(ids(&reloaded), ids(&reg));

        reg.apply(&mut store, SourceEdit::RestoreSource(GODOT_SRC.into()))
            .unwrap();
        assert!(ids(&reg).contains(&GODOT_SRC.to_string()));
        assert!(reg.removed_sources().is_empty());

        // An added source is deleted without a tombstone.
        reg.apply(
            &mut store,
            SourceEdit::RemoveSource("godot:hn:godot engine".into()),
        )
        .unwrap();
        assert!(store.custom_sources().unwrap().is_empty());
        assert!(store.removed(Removed::Source).unwrap().is_empty());
    }

    #[test]
    fn refuses_invalid_edits() {
        let mut store = Store::open_in_memory().unwrap();
        let reg = registry(&store);
        let invalid = |result: Result<Vec<String>, EditError>| match result {
            Err(EditError::Invalid(msg)) => msg,
            other => panic!("expected Invalid, got {other:?}"),
        };

        let msg = invalid(reg.apply(&mut store, add("unknown", hn("x"))));
        assert!(msg.contains("no engine or outlet"), "{msg}");
        let dup = SourceConfig {
            spec: SourceSpec::Github {
                repo: "godotengine/godot".into(),
                mode: GithubMode::Releases,
                tag_prefix: None,
            },
            ..hn("")
        };
        let msg = invalid(reg.apply(&mut store, add("godot", dup)));
        assert!(msg.contains("already has"), "{msg}");
        invalid(reg.apply(&mut store, add("godot", hn(""))));
        invalid(reg.apply(&mut store, SourceEdit::RemoveSource("nope".into())));
        invalid(reg.apply(&mut store, SourceEdit::RestoreSource(GODOT_SRC.into())));
        assert!(store.custom_sources().unwrap().is_empty());
    }

    #[test]
    fn skips_edits_that_no_longer_fit() {
        let store = Store::open_in_memory().unwrap();
        let json = serde_json::to_string(&hn("x")).unwrap();
        store.add_custom_source("gone:hn:x", "gone", &json).unwrap();
        // sources.toml now defines the same source as one added earlier.
        let gh = r#"{"kind":"github","repo":"godotengine/godot"}"#;
        store.add_custom_source(GODOT_SRC, "godot", gh).unwrap();
        store
            .add_custom_source("godot:hn:bad", "godot", "not json")
            .unwrap();
        assert_eq!(
            ids(&registry(&store)),
            [GODOT_SRC, "gfs:feed:https://gamefromscratch.com/feed/"]
        );
    }
}
