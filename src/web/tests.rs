//! Handler tests against an in-memory store.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use chrono::{Duration, Utc};
use http_body_util::BodyExt;
use tower::ServiceExt;

use super::{AppState, router};
use crate::config::{AppConfig, SourcesConfig};
use crate::model::{ItemKind, NewItem};
use crate::store::Store;

const SOURCES: &str = r#"
[[engine]]
id = "godot"
name = "Godot"
tier = 1
csharp = "first-class (.NET build)"
homepage = "https://godotengine.org"
  [[engine.source]]
  kind = "github"
  repo = "godotengine/godot"
  category = "release"

[[engine]]
id = "bevy"
name = "Bevy"
tier = 3
csharp = "none"
  [[engine.source]]
  kind = "feed"
  url = "https://bevyengine.org/news/rss.xml"
  category = "news"

[[outlet]]
id = "gfs"
name = "GameFromScratch"
  [[outlet.source]]
  kind = "feed"
  url = "https://gamefromscratch.com/feed/"
  category = "news"
"#;

const GODOT_SRC: &str = "godot:github:godotengine/godot";
const BEVY_SRC: &str = "bevy:feed:https://bevyengine.org/news/rss.xml";
const GFS_SRC: &str = "gfs:feed:https://gamefromscratch.com/feed/";

fn item(source: &str, guid: &str, engines: &[&str], kind: ItemKind, title: &str) -> NewItem {
    NewItem {
        source_id: source.into(),
        guid: guid.into(),
        engines: engines.iter().map(|e| e.to_string()).collect(),
        kind,
        title: title.into(),
        url: Some(format!("https://example.com/{guid}")),
        summary: Some(format!("Summary of {title} <b>bold</b>")),
        published: Some(Utc::now() - Duration::hours(3)),
        story_key: guid.into(),
    }
}

fn state() -> AppState {
    let mut store = Store::open_in_memory().unwrap();
    store
        .insert_items(&[
            item(
                GODOT_SRC,
                "godot-4-8",
                &["godot"],
                ItemKind::Release,
                "Godot 4.8 released",
            ),
            item(
                BEVY_SRC,
                "bevy-0-17",
                &["bevy"],
                ItemKind::News,
                "Bevy 0.17 is out",
            ),
            item(
                GFS_SRC,
                "gfs-csharp",
                &[],
                ItemKind::News,
                "New C# engine appears",
            ),
        ])
        .unwrap();
    // Same story from the outlet: a duplicate of the Godot release.
    store
        .insert_items(&[item(
            GFS_SRC,
            "godot-4-8",
            &["godot"],
            ItemKind::News,
            "Godot 4.8",
        )])
        .unwrap();
    store.record_fetch_error(BEVY_SRC, "HTTP 503").unwrap();

    let sources: SourcesConfig = toml::from_str(SOURCES).unwrap();
    let mut config = AppConfig::default();
    config.server.templates_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/templates").into();
    config.server.static_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/static").into();
    config.server.base_url = "https://gen.example.org/".into();
    AppState::new(Arc::new(Mutex::new(store)), sources, config).unwrap()
}

async fn get(uri: &str) -> (StatusCode, Option<String>, String) {
    let resp = router(state())
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let ctype = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .map(|v| v.to_str().unwrap().to_string());
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, ctype, String::from_utf8(body.to_vec()).unwrap())
}

#[tokio::test]
async fn timeline_lists_items() {
    let (status, _, body) = get("/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Godot 4.8 released"));
    assert!(body.contains("Bevy 0.17 is out"));
    assert!(body.contains("also on: GameFromScratch"));
    assert!(body.contains("3 hours ago"));
    assert!(body.contains(r#"href="/engine/godot""#));
    // Auto-escaping.
    assert!(body.contains("&lt;b&gt;bold"));
}

#[tokio::test]
async fn timeline_filters() {
    let (status, _, body) = get("/?engine=godot").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Godot 4.8 released"));
    assert!(!body.contains("Bevy 0.17 is out"));

    let (_, _, body) = get("/?kind=news&engine=").await;
    assert!(!body.contains("Godot 4.8 released"));
    assert!(body.contains("Bevy 0.17 is out"));

    let (_, _, body) = get("/?csharp=1").await;
    assert!(body.contains("Godot 4.8 released"));
    assert!(body.contains("New C# engine appears"));
    assert!(!body.contains("Bevy 0.17 is out"));

    let (_, _, body) = get("/?tier=3").await;
    assert!(!body.contains("Godot 4.8 released"));
    assert!(body.contains("Bevy 0.17 is out"));
}

#[tokio::test]
async fn engine_pages() {
    let (status, _, body) = get("/engine/godot").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("first-class (.NET build)"));
    assert!(body.contains("Godot 4.8 released"));
    assert!(body.contains("godotengine&#x2f;godot"));

    let (status, _, body) = get("/engine/unknown").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("404"));

    let (status, _, _) = get("/no/such/page").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn engines_and_sources_pages() {
    let (status, _, body) = get("/engines").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Major C# engines"));
    assert!(body.contains("Bevy"));

    let (status, _, body) = get("/sources").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("HTTP 503"));
    assert!(body.contains("status-failing"));
}

#[tokio::test]
async fn atom_feed() {
    let (status, ctype, body) = get("/feed.xml?engine=godot").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        ctype.as_deref(),
        Some("application/atom+xml; charset=utf-8")
    );
    let feed = atom_syndication::Feed::read_from(body.as_bytes()).unwrap();
    assert_eq!(feed.entries().len(), 1);
    let entry = &feed.entries()[0];
    assert_eq!(entry.title().as_str(), "Godot 4.8 released");
    assert_eq!(entry.categories()[0].term(), "godot");
    for link in feed.links().iter().chain(entry.links()) {
        assert!(link.href().starts_with("https://"), "{}", link.href());
    }
    assert!(
        feed.links()
            .iter()
            .any(|l| l.href() == "https://gen.example.org/feed.xml?engine=godot")
    );

    let (_, _, body) = get("/feed.xml").await;
    let feed = atom_syndication::Feed::read_from(body.as_bytes()).unwrap();
    // The duplicate is left out.
    assert_eq!(feed.entries().len(), 3);
}

#[tokio::test]
async fn json_api() {
    let (status, ctype, body) = get("/api/items?engine=godot&limit=500").await;
    assert_eq!(status, StatusCode::OK);
    assert!(ctype.unwrap().starts_with("application/json"));
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["limit"], 200);
    let items = json["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["title"], "Godot 4.8 released");
    assert_eq!(items[0]["kind"], "release");
    assert_eq!(items[0]["also_on"], serde_json::json!(["GameFromScratch"]));
}

#[tokio::test]
async fn health_and_static() {
    let (status, _, body) = get("/health").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "ok");

    let (status, ctype, _) = get("/static/style.css").await;
    assert_eq!(status, StatusCode::OK);
    assert!(ctype.unwrap().starts_with("text/css"));
}
