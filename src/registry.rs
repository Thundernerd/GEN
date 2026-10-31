//! The live set of engines, outlets and sources, stored in the database and edited in the
//! web UI. Each change publishes a new [`Snapshot`]; readers take the current one and
//! subscribers (the scheduler) are woken.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::watch;

use crate::classify::Classifier;
use crate::config::{
    EngineConfig, OutletConfig, Owner, SourceConfig, SourcesConfig, check_url, source_id,
    valid_owner_id,
};
use crate::model::Item;
use crate::store::Store;

/// One consistent view of the sources, with everything derived from it.
pub struct Snapshot {
    /// The stored engines and outlets with their sources.
    pub config: SourcesConfig,
    pub classifier: Classifier,
    pub lookup: Lookup,
    stored: StoredSources,
}

impl Snapshot {
    /// Builds the config from the stored rows. Rows that don't fit (a source whose owner
    /// is gone, or a duplicate) are skipped with a warning, so they can't stop GEN from
    /// starting.
    fn build(stored: StoredSources) -> anyhow::Result<Self> {
        let mut config = SourcesConfig {
            engines: Vec::new(),
            outlets: Vec::new(),
        };
        for owner in &stored.owners {
            match owner {
                NewOwner::Engine(e) => config.engines.push(e.clone()),
                NewOwner::Outlet(o) => config.outlets.push(o.clone()),
            }
        }

        let mut ids = HashSet::new();
        for source in &stored.sources {
            let sources = config
                .engines
                .iter_mut()
                .find(|e| e.id == source.owner_id)
                .map(|e| &mut e.sources)
                .or_else(|| {
                    config
                        .outlets
                        .iter_mut()
                        .find(|o| o.id == source.owner_id)
                        .map(|o| &mut o.sources)
                });
            match sources {
                Some(sources) if ids.insert(source_id(&source.owner_id, &source.config.spec)) => {
                    sources.push(source.config.clone());
                }
                Some(_) => {
                    tracing::warn!(source = %source.id, "skipping stored source: it exists already")
                }
                None => {
                    tracing::warn!(source = %source.id, "skipping stored source: its engine or outlet is gone")
                }
            }
        }

        config.validate()?;
        Ok(Snapshot {
            classifier: Classifier::new(&config),
            lookup: Lookup::new(&config),
            config,
            stored,
        })
    }

    pub fn has_owner(&self, id: &str) -> bool {
        self.config.engines.iter().any(|e| e.id == id)
            || self.config.outlets.iter().any(|o| o.id == id)
    }
}

/// The engines, outlets and sources as stored in the database.
#[derive(Debug, Clone, Default)]
struct StoredSources {
    owners: Vec<NewOwner>,
    sources: Vec<StoredSource>,
}

/// An engine or outlet to add, possibly with sources. Once stored, its sources are kept
/// in [`StoredSources::sources`], so `sources` there is always empty.
#[derive(Debug, Clone)]
pub enum NewOwner {
    Engine(EngineConfig),
    Outlet(OutletConfig),
}

impl NewOwner {
    pub fn id(&self) -> &str {
        match self {
            NewOwner::Engine(e) => &e.id,
            NewOwner::Outlet(o) => &o.id,
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            NewOwner::Engine(_) => "engine",
            NewOwner::Outlet(_) => "outlet",
        }
    }

    /// Splits off the sources, leaving the owner without them.
    fn take_sources(&mut self) -> Vec<SourceConfig> {
        match self {
            NewOwner::Engine(e) => std::mem::take(&mut e.sources),
            NewOwner::Outlet(o) => std::mem::take(&mut o.sources),
        }
    }

    fn to_json(&self) -> serde_json::Result<String> {
        match self {
            NewOwner::Engine(e) => serde_json::to_string(e),
            NewOwner::Outlet(o) => serde_json::to_string(o),
        }
    }

    fn from_json(kind: &str, json: &str) -> anyhow::Result<Self> {
        Ok(match kind {
            "engine" => NewOwner::Engine(serde_json::from_str(json)?),
            "outlet" => NewOwner::Outlet(serde_json::from_str(json)?),
            other => anyhow::bail!("unknown kind `{other}`"),
        })
    }

    /// Checks an engine/outlet before it is added. The error is a message for the user.
    fn check(&self) -> Result<(), String> {
        let id = self.id();
        if !valid_owner_id(id) {
            return Err(format!(
                "“{id}” can't be used as an id: use up to 40 lowercase letters, digits and dashes."
            ));
        }
        let (name, homepage) = match self {
            NewOwner::Engine(e) => (&e.name, &e.homepage),
            NewOwner::Outlet(o) => (&o.name, &o.homepage),
        };
        if name.trim().is_empty() {
            return Err("Enter a name.".into());
        }
        if let Some(url) = homepage {
            check_url(url)?;
        }
        if let NewOwner::Engine(e) = self {
            if !(1..=3).contains(&e.tier) {
                return Err("The tier must be 1, 2 or 3.".into());
            }
            if let Some(icon) = &e.icon {
                check_url(icon)?;
            }
        }
        Ok(())
    }
}

/// A stored source.
#[derive(Debug, Clone)]
struct StoredSource {
    id: String,
    owner_id: String,
    config: SourceConfig,
}

impl StoredSources {
    fn load(store: &Store) -> anyhow::Result<Self> {
        let mut owners = Vec::new();
        for (id, kind, json) in store.owners()? {
            match NewOwner::from_json(&kind, &json) {
                Ok(owner) => owners.push(owner),
                Err(err) => {
                    tracing::warn!(owner = %id, "skipping unreadable stored {kind}: {err:#}")
                }
            }
        }
        let mut sources = Vec::new();
        for (id, owner_id, json) in store.sources()? {
            match serde_json::from_str(&json) {
                Ok(config) => sources.push(StoredSource {
                    id,
                    owner_id,
                    config,
                }),
                Err(err) => {
                    tracing::warn!(source = %id, "skipping unreadable stored source: {err}")
                }
            }
        }
        Ok(StoredSources { owners, sources })
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
    /// Sets how often a source is polled; `None` uses the default interval.
    SetInterval {
        source_id: String,
        minutes: Option<u64>,
    },
    /// Adds an engine or outlet with the sources it carries.
    AddOwner(NewOwner),
    /// Sets an engine's icon URL.
    SetEngineIcon {
        engine_id: String,
        icon: Option<String>,
    },
    RemoveOwner(String),
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
    tx: watch::Sender<Arc<Snapshot>>,
}

impl SourceRegistry {
    /// Loads the engines, outlets and sources stored in `store`.
    pub fn load(store: &Store) -> anyhow::Result<Self> {
        let snapshot = Snapshot::build(StoredSources::load(store)?)?;
        let (tx, _) = watch::channel(Arc::new(snapshot));
        Ok(SourceRegistry { tx })
    }

    /// The current snapshot. Cheap; hold it for one request or poll, not across changes.
    pub fn current(&self) -> Arc<Snapshot> {
        self.tx.borrow().clone()
    }

    /// A receiver that is marked changed whenever a new snapshot is published.
    pub fn subscribe(&self) -> watch::Receiver<Arc<Snapshot>> {
        self.tx.subscribe()
    }

    /// Checks `edit`, stores it and publishes the new snapshot. Returns the ids of sources
    /// whose data was deleted. Callers must serialize calls (they run under the store lock).
    pub fn apply(&self, store: &mut Store, edit: SourceEdit) -> Result<Vec<String>, EditError> {
        let current = self.current();
        let mut stored = current.stored.clone();
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
                stored.sources.push(StoredSource {
                    id: id.clone(),
                    owner_id: owner_id.clone(),
                    config,
                });
                self.commit(stored, || store.add_source(&id, &owner_id, &json))?;
                tracing::info!(source = %id, "source added");
                Ok(Vec::new())
            }
            SourceEdit::RemoveSource(id) => {
                if !current.config.all_sources().iter().any(|s| s.id == id) {
                    return Err(EditError::Invalid(format!("There is no source “{id}”.")));
                }
                stored.sources.retain(|s| s.id != id);
                self.commit(stored, || store.remove_source(&id))?;
                tracing::info!(source = %id, "source removed");
                Ok(vec![id])
            }
            SourceEdit::SetInterval { source_id, minutes } => {
                let Some(stored_source) = stored.sources.iter_mut().find(|s| s.id == source_id)
                else {
                    return Err(EditError::Invalid(format!(
                        "There is no source “{source_id}”."
                    )));
                };
                stored_source.config.interval_minutes = minutes;
                stored_source.config.check().map_err(EditError::Invalid)?;
                let json =
                    serde_json::to_string(&stored_source.config).map_err(anyhow::Error::from)?;
                self.commit(stored, || {
                    store.update_source(&source_id, &json).map(|_| ())
                })?;
                tracing::info!(source = %source_id, ?minutes, "polling interval changed");
                Ok(Vec::new())
            }
            SourceEdit::AddOwner(mut owner) => {
                owner.check().map_err(EditError::Invalid)?;
                let id = owner.id().to_string();
                if current.has_owner(&id) {
                    return Err(EditError::Invalid(format!("“{id}” is already used.")));
                }
                let mut rows = Vec::new();
                for config in owner.take_sources() {
                    config.check().map_err(EditError::Invalid)?;
                    let source_id = source_id(&id, &config.spec);
                    if rows.iter().any(|(s, _)| *s == source_id) {
                        return Err(EditError::Invalid(format!(
                            "“{source_id}” is listed twice."
                        )));
                    }
                    let json = serde_json::to_string(&config).map_err(anyhow::Error::from)?;
                    rows.push((source_id.clone(), json));
                    stored.sources.push(StoredSource {
                        id: source_id,
                        owner_id: id.clone(),
                        config,
                    });
                }
                let json = owner.to_json().map_err(anyhow::Error::from)?;
                let kind = owner.kind();
                stored.owners.push(owner);
                self.commit(stored, || store.add_owner(&id, kind, &json, &rows))?;
                tracing::info!(owner = %id, sources = rows.len(), "{kind} added");
                Ok(Vec::new())
            }
            SourceEdit::SetEngineIcon { engine_id, icon } => {
                let Some(NewOwner::Engine(engine)) =
                    stored.owners.iter_mut().find(|o| o.id() == engine_id)
                else {
                    return Err(EditError::Invalid(format!(
                        "There is no engine “{engine_id}”."
                    )));
                };
                engine.icon = icon;
                let owner = NewOwner::Engine(engine.clone());
                owner.check().map_err(EditError::Invalid)?;
                let json = owner.to_json().map_err(anyhow::Error::from)?;
                self.commit(stored, || store.update_owner(&engine_id, &json))?;
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
                stored.owners.retain(|o| o.id() != id);
                stored.sources.retain(|s| s.owner_id != id);
                self.commit(stored, || store.remove_owner(&id, is_engine, &source_ids))?;
                tracing::info!(owner = %id, sources = source_ids.len(), "engine/outlet removed");
                Ok(source_ids)
            }
        }
    }

    /// Builds the snapshot for `stored` (so nothing is stored if it is invalid), stores the
    /// change and publishes the snapshot.
    fn commit(
        &self,
        stored: StoredSources,
        persist: impl FnOnce() -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let snapshot = Snapshot::build(stored)?;
        persist()?;
        self.tx.send_replace(Arc::new(snapshot));
        Ok(())
    }
}

#[cfg(test)]
impl SourceRegistry {
    /// A registry over `store` with the engines and outlets of `toml` (sources.toml format)
    /// added.
    pub fn with_owners(store: &mut Store, toml: &str) -> Self {
        let registry = SourceRegistry::load(store).unwrap();
        let config: SourcesConfig = toml::from_str(toml).unwrap();
        let engines = config.engines.into_iter().map(NewOwner::Engine);
        for owner in engines.chain(config.outlets.into_iter().map(NewOwner::Outlet)) {
            registry.apply(store, SourceEdit::AddOwner(owner)).unwrap();
        }
        registry
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

    const FIXTURE: &str = r#"
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

    /// A registry holding the fixture's engine and outlet.
    fn registry(store: &mut Store) -> SourceRegistry {
        SourceRegistry::with_owners(store, FIXTURE)
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
        let reg = registry(&mut store);
        assert_eq!(
            ids(&reg),
            [GODOT_SRC, "gfs:feed:https://gamefromscratch.com/feed/"]
        );
        let changes = reg.subscribe();

        reg.apply(&mut store, add("godot", hn("godot engine")))
            .unwrap();
        assert!(changes.has_changed().unwrap());
        assert!(ids(&reg).contains(&"godot:hn:godot engine".to_string()));

        let purged = reg
            .apply(&mut store, SourceEdit::RemoveSource(GODOT_SRC.into()))
            .unwrap();
        assert_eq!(purged, [GODOT_SRC]);
        assert!(!ids(&reg).contains(&GODOT_SRC.to_string()));

        // Everything is read back from the store.
        let reloaded = SourceRegistry::load(&store).unwrap();
        assert_eq!(ids(&reloaded), ids(&reg));
        assert_eq!(store.sources().unwrap().len(), 2);
    }

    #[test]
    fn sets_polling_intervals() {
        let mut store = Store::open_in_memory().unwrap();
        let reg = registry(&mut store);
        let set = |reg: &SourceRegistry, store: &mut Store, minutes| {
            reg.apply(
                store,
                SourceEdit::SetInterval {
                    source_id: GODOT_SRC.into(),
                    minutes,
                },
            )
        };
        set(&reg, &mut store, Some(15)).unwrap();
        let interval = |reg: &SourceRegistry| {
            reg.current()
                .config
                .all_sources()
                .into_iter()
                .find(|s| s.id == GODOT_SRC)
                .unwrap()
                .interval_minutes
        };
        assert_eq!(interval(&reg), Some(15));
        assert_eq!(
            interval(&SourceRegistry::load(&store).unwrap()),
            Some(15),
            "stored"
        );
        set(&reg, &mut store, None).unwrap();
        assert_eq!(interval(&reg), None);
        assert!(matches!(
            set(&reg, &mut store, Some(0)),
            Err(EditError::Invalid(_))
        ));
        let missing = reg.apply(
            &mut store,
            SourceEdit::SetInterval {
                source_id: "nope".into(),
                minutes: None,
            },
        );
        assert!(matches!(missing, Err(EditError::Invalid(_))));
    }

    #[test]
    fn refuses_invalid_edits() {
        let mut store = Store::open_in_memory().unwrap();
        let reg = registry(&mut store);
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
        assert_eq!(store.sources().unwrap().len(), 2);
    }

    fn engine(id: &str) -> NewOwner {
        NewOwner::Engine(EngineConfig {
            id: id.into(),
            name: "Stride".into(),
            tier: 1,
            csharp: "native".into(),
            homepage: None,
            icon: None,
            enabled: true,
            aliases: vec!["stride".into()],
            exclude: Vec::new(),
            sources: Vec::new(),
        })
    }

    #[test]
    fn adds_and_removes_engines() {
        let mut store = Store::open_in_memory().unwrap();
        let reg = registry(&mut store);
        let NewOwner::Engine(mut stride) = engine("stride") else {
            unreachable!()
        };
        stride.sources = vec![hn("stride")];
        reg.apply(&mut store, SourceEdit::AddOwner(NewOwner::Engine(stride)))
            .unwrap();
        reg.apply(&mut store, add("stride", hn("xenko"))).unwrap();
        let snapshot = reg.current();
        assert_eq!(snapshot.config.engine("stride").unwrap().sources.len(), 2);
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
        let reloaded = SourceRegistry::load(&store).unwrap().current();
        assert_eq!(reloaded.config.engine("stride").unwrap().sources.len(), 2);

        let purged = reg
            .apply(&mut store, SourceEdit::RemoveOwner("stride".into()))
            .unwrap();
        assert_eq!(purged, ["stride:hn:stride", "stride:hn:xenko"]);
        assert!(reg.current().config.engine("stride").is_none());
        assert_eq!(store.owners().unwrap().len(), 2);
        assert_eq!(store.sources().unwrap().len(), 2);
    }

    #[test]
    fn refuses_invalid_owners() {
        let mut store = Store::open_in_memory().unwrap();
        let reg = registry(&mut store);
        let invalid = |reg: &SourceRegistry, store: &mut Store, owner: NewOwner| match reg
            .apply(store, SourceEdit::AddOwner(owner))
        {
            Err(EditError::Invalid(msg)) => msg,
            other => panic!("expected Invalid, got {other:?}"),
        };
        assert!(invalid(&reg, &mut store, engine("Bad Id")).contains("can't be used"));
        assert!(invalid(&reg, &mut store, engine("a:b")).contains("can't be used"));
        assert!(invalid(&reg, &mut store, engine("gfs")).contains("already used"));
        let NewOwner::Engine(mut tier7) = engine("x") else {
            unreachable!()
        };
        tier7.tier = 7;
        assert!(invalid(&reg, &mut store, NewOwner::Engine(tier7)).contains("tier"));
        let NewOwner::Engine(mut twice) = engine("x") else {
            unreachable!()
        };
        twice.sources = vec![hn("x"), hn("x")];
        assert!(invalid(&reg, &mut store, NewOwner::Engine(twice)).contains("twice"));
        let NewOwner::Engine(mut bad_source) = engine("x") else {
            unreachable!()
        };
        bad_source.sources = vec![hn("")];
        invalid(&reg, &mut store, NewOwner::Engine(bad_source));
        assert_eq!(store.owners().unwrap().len(), 2);
    }

    #[test]
    fn skips_rows_that_no_longer_fit() {
        let mut store = Store::open_in_memory().unwrap();
        registry(&mut store);
        let json = serde_json::to_string(&hn("x")).unwrap();
        store.add_source("gone:hn:x", "gone", &json).unwrap();
        let gh = r#"{"kind":"github","repo":"godotengine/godot"}"#;
        store.add_source("godot:dup", "godot", gh).unwrap();
        store
            .add_source("godot:hn:bad", "godot", "not json")
            .unwrap();
        assert_eq!(
            ids(&SourceRegistry::load(&store).unwrap()),
            [GODOT_SRC, "gfs:feed:https://gamefromscratch.com/feed/"]
        );
    }
}
