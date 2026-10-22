//! The live set of engines, outlets and sources: `sources.toml` plus the edits made in the
//! web UI (stored in the database). Each change publishes a new [`Snapshot`]; readers take
//! the current one and subscribers (the scheduler) are woken.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::watch;

use crate::classify::Classifier;
use crate::config::{
    EngineConfig, OutletConfig, Owner, ResolvedSource, SourceConfig, SourcesConfig, check_url,
    source_id, valid_owner_id,
};
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

        for owner in &edits.owners {
            let id = owner.id();
            if config.engines.iter().any(|e| e.id == id)
                || config.outlets.iter().any(|o| o.id == id)
            {
                tracing::warn!(owner = %id, "skipping added engine/outlet: sources.toml defines it too");
                continue;
            }
            match owner {
                CustomOwner::Engine(e) => config.engines.push(e.clone()),
                CustomOwner::Outlet(o) => config.outlets.push(o.clone()),
            }
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

    /// Whether an engine or outlet was added in the web UI.
    pub fn is_custom_owner(&self, id: &str) -> bool {
        self.edits.owners.iter().any(|o| o.id() == id)
    }

    fn has_owner(&self, id: &str) -> bool {
        self.config.engines.iter().any(|e| e.id == id)
            || self.config.outlets.iter().any(|o| o.id == id)
    }
}

/// Changes made in the web UI on top of `sources.toml`.
#[derive(Debug, Clone, Default)]
pub struct SourceEdits {
    pub owners: Vec<CustomOwner>,
    pub sources: Vec<CustomSource>,
    pub removed_owners: BTreeSet<String>,
    pub removed_sources: BTreeSet<String>,
}

/// An engine or outlet added in the web UI. Its sources are kept in
/// [`SourceEdits::sources`], so `sources` here is always empty.
#[derive(Debug, Clone)]
pub enum CustomOwner {
    Engine(EngineConfig),
    Outlet(OutletConfig),
}

impl CustomOwner {
    pub fn id(&self) -> &str {
        match self {
            CustomOwner::Engine(e) => &e.id,
            CustomOwner::Outlet(o) => &o.id,
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            CustomOwner::Engine(_) => "engine",
            CustomOwner::Outlet(_) => "outlet",
        }
    }

    fn to_json(&self) -> serde_json::Result<String> {
        match self {
            CustomOwner::Engine(e) => serde_json::to_string(e),
            CustomOwner::Outlet(o) => serde_json::to_string(o),
        }
    }

    fn from_json(kind: &str, json: &str) -> anyhow::Result<Self> {
        Ok(match kind {
            "engine" => CustomOwner::Engine(serde_json::from_str(json)?),
            "outlet" => CustomOwner::Outlet(serde_json::from_str(json)?),
            other => anyhow::bail!("unknown kind `{other}`"),
        })
    }

    /// Checks an engine/outlet entered in the web UI. The error is a message for the user.
    fn check(&self) -> Result<(), String> {
        let id = self.id();
        if !valid_owner_id(id) {
            return Err(format!(
                "“{id}” can't be used as an id: use up to 40 lowercase letters, digits and dashes."
            ));
        }
        let (name, homepage) = match self {
            CustomOwner::Engine(e) => (&e.name, &e.homepage),
            CustomOwner::Outlet(o) => (&o.name, &o.homepage),
        };
        if name.trim().is_empty() {
            return Err("Enter a name.".into());
        }
        if let Some(url) = homepage {
            check_url(url)?;
        }
        if let CustomOwner::Engine(e) = self
            && !(1..=3).contains(&e.tier)
        {
            return Err("The tier must be 1, 2 or 3.".into());
        }
        Ok(())
    }
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
        let mut owners = Vec::new();
        for (id, kind, json) in store.custom_owners()? {
            match CustomOwner::from_json(&kind, &json) {
                Ok(owner) => owners.push(owner),
                Err(err) => {
                    tracing::warn!(owner = %id, "skipping unreadable added {kind}: {err:#}")
                }
            }
        }
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
            owners,
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
    AddOwner(CustomOwner),
    RemoveOwner(String),
    RestoreOwner(String),
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

    /// Engines and outlets from sources.toml that were removed in the web UI and can be
    /// restored.
    pub fn removed_owners(&self) -> Vec<(Owner, String)> {
        let current = self.current();
        let removed =
            |id: &str| current.edits.removed_owners.contains(id) && !current.has_owner(id);
        let engines = self
            .baseline
            .engines
            .iter()
            .filter(|e| removed(&e.id))
            .map(|e| (Owner::Engine(e.id.clone()), e.name.clone()));
        let outlets = self
            .baseline
            .outlets
            .iter()
            .filter(|o| removed(&o.id))
            .map(|o| (Owner::Outlet(o.id.clone()), o.name.clone()));
        engines.chain(outlets).collect()
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
            SourceEdit::AddOwner(owner) => {
                owner.check().map_err(EditError::Invalid)?;
                let id = owner.id().to_string();
                if self.baseline.engines.iter().any(|e| e.id == id)
                    || self.baseline.outlets.iter().any(|o| o.id == id)
                {
                    let enabled = current.config.engine(&id).map(|e| e.enabled).or_else(|| {
                        current
                            .config
                            .outlets
                            .iter()
                            .find(|o| o.id == id)
                            .map(|o| o.enabled)
                    });
                    let hint = match enabled {
                        None => " Restore it instead.",
                        Some(false) => " It is switched off there; set `enabled = true` to use it.",
                        Some(true) => "",
                    };
                    return Err(EditError::Invalid(format!(
                        "“{id}” is defined in sources.toml.{hint}"
                    )));
                }
                if current.has_owner(&id) {
                    return Err(EditError::Invalid(format!("“{id}” is already used.")));
                }
                let json = owner.to_json().map_err(anyhow::Error::from)?;
                let kind = owner.kind();
                edits.owners.push(owner);
                self.commit(edits, || store.add_custom_owner(&id, kind, &json))?;
                tracing::info!(owner = %id, "{kind} added");
                Ok(Vec::new())
            }
            SourceEdit::RemoveOwner(id) => {
                let is_engine = current.config.engine(&id).is_some();
                if !current.has_owner(&id) {
                    return Err(EditError::Invalid(format!(
                        "There is no engine or outlet “{id}”."
                    )));
                }
                let source_ids: Vec<String> = current
                    .config
                    .all_sources()
                    .into_iter()
                    .filter(|s| s.owner.id() == id)
                    .map(|s| s.id)
                    .collect();
                // Added here: delete it. From sources.toml: leave a tombstone. Either way
                // the sources added to it are deleted.
                let before = edits.owners.len();
                edits.owners.retain(|o| o.id() != id);
                let tombstone = edits.owners.len() == before;
                if tombstone {
                    edits.removed_owners.insert(id.clone());
                }
                edits.sources.retain(|s| s.owner_id != id);
                self.commit(edits, || {
                    store.remove_owner(&id, is_engine, tombstone, &source_ids)
                })?;
                tracing::info!(owner = %id, sources = source_ids.len(), "engine/outlet removed");
                Ok(source_ids)
            }
            SourceEdit::RestoreOwner(id) => {
                if !self.removed_owners().iter().any(|(o, _)| o.id() == id) {
                    return Err(EditError::Invalid(format!(
                        "“{id}” is not a removed engine or outlet that can be restored."
                    )));
                }
                edits.removed_owners.remove(&id);
                self.commit(edits, || store.restore(Removed::Owner, &id))?;
                tracing::info!(owner = %id, "engine/outlet restored");
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

    fn engine(id: &str) -> CustomOwner {
        CustomOwner::Engine(EngineConfig {
            id: id.into(),
            name: "Stride".into(),
            tier: 1,
            csharp: "native".into(),
            homepage: None,
            enabled: true,
            aliases: vec!["stride".into()],
            exclude: Vec::new(),
            sources: Vec::new(),
        })
    }

    #[test]
    fn adds_and_removes_engines() {
        let mut store = Store::open_in_memory().unwrap();
        let reg = registry(&store);
        reg.apply(&mut store, SourceEdit::AddOwner(engine("stride")))
            .unwrap();
        reg.apply(&mut store, add("stride", hn("stride"))).unwrap();
        let snapshot = reg.current();
        assert!(snapshot.is_custom_owner("stride"));
        assert_eq!(snapshot.config.engine("stride").unwrap().sources.len(), 1);
        // The classifier knows the new engine's aliases.
        let gfs = snapshot
            .config
            .manageable_sources()
            .into_iter()
            .find(|s| s.owner.id() == "gfs")
            .unwrap();
        let raw = crate::model::RawItem {
            guid: "1".into(),
            title: "Stride 4.3 is out".into(),
            ..Default::default()
        };
        let item = snapshot.classifier.classify(&gfs, raw).unwrap();
        assert_eq!(item.engines, ["stride"]);
        // Survives a reload.
        assert!(registry(&store).current().config.engine("stride").is_some());

        let purged = reg
            .apply(&mut store, SourceEdit::RemoveOwner("stride".into()))
            .unwrap();
        assert_eq!(purged, ["stride:hn:stride"]);
        assert!(reg.current().config.engine("stride").is_none());
        assert!(store.custom_owners().unwrap().is_empty());
        assert!(store.custom_sources().unwrap().is_empty());
        assert!(
            reg.removed_owners().is_empty(),
            "added here: nothing to restore"
        );

        // An engine from sources.toml is tombstoned with its sources and can be restored.
        let purged = reg
            .apply(&mut store, SourceEdit::RemoveOwner("godot".into()))
            .unwrap();
        assert_eq!(purged, [GODOT_SRC]);
        assert!(!ids(&reg).contains(&GODOT_SRC.to_string()));
        assert_eq!(reg.removed_owners()[0].1, "Godot");
        assert!(registry(&store).current().config.engine("godot").is_none());
        reg.apply(&mut store, SourceEdit::RestoreOwner("godot".into()))
            .unwrap();
        assert!(ids(&reg).contains(&GODOT_SRC.to_string()));
    }

    #[test]
    fn refuses_invalid_owners() {
        let mut store = Store::open_in_memory().unwrap();
        let reg = registry(&store);
        let invalid = |reg: &SourceRegistry, store: &mut Store, owner: CustomOwner| match reg
            .apply(store, SourceEdit::AddOwner(owner))
        {
            Err(EditError::Invalid(msg)) => msg,
            other => panic!("expected Invalid, got {other:?}"),
        };
        assert!(invalid(&reg, &mut store, engine("Bad Id")).contains("can't be used"));
        assert!(invalid(&reg, &mut store, engine("a:b")).contains("can't be used"));
        assert!(invalid(&reg, &mut store, engine("gfs")).contains("defined in sources.toml."));
        let CustomOwner::Engine(mut tier7) = engine("x") else {
            unreachable!()
        };
        tier7.tier = 7;
        assert!(invalid(&reg, &mut store, CustomOwner::Engine(tier7)).contains("tier"));

        reg.apply(&mut store, SourceEdit::RemoveOwner("godot".into()))
            .unwrap();
        assert!(invalid(&reg, &mut store, engine("godot")).contains("Restore it instead"));
        reg.apply(&mut store, SourceEdit::AddOwner(engine("stride")))
            .unwrap();
        assert!(invalid(&reg, &mut store, engine("stride")).contains("already used"));
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
