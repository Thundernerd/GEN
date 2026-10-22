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
use crate::registry::SourceRegistry;
use crate::store::Store;
use crate::toggles::SourceToggles;

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
        notify: false,
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
    config.server.admin_token = Some(ADMIN_TOKEN.into());
    let toggles = Arc::new(SourceToggles::default());
    let registry = Arc::new(SourceRegistry::load(sources, &store).unwrap());
    AppState::new(Arc::new(Mutex::new(store)), toggles, registry, config).unwrap()
}

const ADMIN_TOKEN: &str = "test-admin-token";

/// Sends a request to a router sharing `st`, returning status, headers and body.
async fn send(st: &AppState, req: Request<Body>) -> (StatusCode, axum::http::HeaderMap, String) {
    let resp = router(st.clone()).oneshot(req).await.unwrap();
    let (status, headers) = (resp.status(), resp.headers().clone());
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8(body.to_vec()).unwrap())
}

fn post_form(uri: &str, body: &str, cookie: Option<&str>) -> Request<Body> {
    let mut req =
        Request::post(uri).header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    if let Some(c) = cookie {
        req = req.header(header::COOKIE, c);
    }
    req.body(Body::from(body.to_string())).unwrap()
}

/// Logs in and returns the `name=value` admin cookie.
async fn login(st: &AppState) -> String {
    let (status, headers, _) = send(
        st,
        post_form(
            "/login",
            &format!("token={ADMIN_TOKEN}&next=%2Fsources"),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers[header::LOCATION], "/sources");
    let set_cookie = headers[header::SET_COOKIE].to_str().unwrap();
    assert!(set_cookie.contains("HttpOnly") && set_cookie.contains("SameSite=Strict"));
    assert!(set_cookie.contains("Secure"), "base_url is https");
    set_cookie.split(';').next().unwrap().to_string()
}

#[tokio::test]
async fn sources_grouped_by_owner() {
    let pos = |body: &str, owner: &str| body.find(&format!(">{owner}<")).unwrap();
    let active =
        |body: &str, label: &str| body.contains(&format!(r#"aria-current="true">{label}<"#));

    // Default: groups in config order (Godot, Bevy, then the outlet), one header each.
    let (_, _, body) = get("/sources?sort=bogus").await;
    assert!(pos(&body, "Godot") < pos(&body, "Bevy"));
    assert!(pos(&body, "Bevy") < pos(&body, "GameFromScratch"));
    assert_eq!(body.matches(r#"class="group-head""#).count(), 3);
    assert!(body.contains("news outlet"));
    assert!(body.contains("1 of 1 enabled"));
    assert!(active(&body, "Config"));

    let (_, _, body) = get("/sources?sort=owner").await;
    assert!(pos(&body, "Bevy") < pos(&body, "GameFromScratch"));
    assert!(pos(&body, "GameFromScratch") < pos(&body, "Godot"));
    assert!(active(&body, "A–Z"));

    let (_, _, body) = get("/sources?sort=owner&dir=desc").await;
    assert!(pos(&body, "Godot") < pos(&body, "GameFromScratch"));
    assert!(pos(&body, "GameFromScratch") < pos(&body, "Bevy"));
    assert!(active(&body, "Z–A"));

    // Engine pages list their sources without group headers.
    let (_, _, body) = get("/engine/godot").await;
    assert!(!body.contains(r#"class="group-head""#));
    assert!(body.contains(r#"class="source-kind""#));
}

#[tokio::test]
async fn toggling_sources_requires_admin() {
    let st = state();
    let toggle = format!("source_id={}&enabled=0&next=%2Fsources", urlenc(GODOT_SRC));

    let (status, _, _) = send(&st, post_form("/sources/toggle", &toggle, None)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = send(
        &st,
        post_form("/sources/toggle", &toggle, Some("gen_admin=deadbeef")),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _, body) = send(&st, post_form("/login", "token=wrong", None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body.contains("not correct"));

    // Not logged in: no toggle buttons, but a way to log in.
    let (_, _, body) = send(&st, Request::get("/sources").body(Body::empty()).unwrap()).await;
    assert!(!body.contains("/sources/toggle"));
    assert!(body.contains("/login?next=/sources") || body.contains("/login?next=&#x2f;sources"));
}

#[tokio::test]
async fn admin_can_disable_and_enable_sources() {
    let st = state();
    let cookie = login(&st).await;
    let source = st
        .registry
        .current()
        .config
        .manageable_sources()
        .into_iter()
        .find(|s| s.id == GODOT_SRC)
        .unwrap();

    let (_, _, body) = send(
        &st,
        Request::get("/sources")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(body.contains("Disable"));

    let disable = format!(
        "source_id={}&enabled=0&next=%2Fengine%2Fgodot",
        urlenc(GODOT_SRC)
    );
    let (status, headers, _) =
        send(&st, post_form("/sources/toggle", &disable, Some(&cookie))).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers[header::LOCATION], "/engine/godot");
    assert!(!st.toggles.is_enabled(&source));
    let stored = st.store.lock().unwrap().source_overrides().unwrap();
    assert_eq!(stored.get(GODOT_SRC), Some(&false));

    let (_, _, body) = send(
        &st,
        Request::get("/sources")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(body.contains("status-disabled\">disabled"));
    assert!(body.contains("Enable"));

    let enable = format!(
        "source_id={}&enabled=1&next=https%3A%2F%2Fevil.example",
        urlenc(GODOT_SRC)
    );
    let (status, headers, _) =
        send(&st, post_form("/sources/toggle", &enable, Some(&cookie))).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers[header::LOCATION], "/sources", "no open redirect");
    assert!(st.toggles.is_enabled(&source));

    let unknown = "source_id=nope&enabled=0";
    let (status, _, _) = send(&st, post_form("/sources/toggle", unknown, Some(&cookie))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, headers, _) = send(&st, post_form("/logout", "", Some(&cookie))).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(
        headers[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .contains("Max-Age=0")
    );
}

fn urlenc(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
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
async fn huge_page_numbers_dont_overflow() {
    for uri in [
        "/?page=4294967295",
        "/engine/godot?page=4294967295",
        "/api/items?page=4294967295&limit=200",
    ] {
        let (status, _, _) = get(uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
    }
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

/// GET with the admin cookie.
async fn get_as(st: &AppState, uri: &str, cookie: &str) -> (StatusCode, String) {
    let (status, _, body) = send(
        st,
        Request::get(uri)
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    (status, body)
}

#[tokio::test]
async fn managing_sources_requires_admin() {
    let st = state();
    for (uri, body) in [
        ("/sources/add", "owner_id=bevy&kind=hn&query=bevy"),
        (
            "/sources/remove",
            &*format!("source_id={}", urlenc(GODOT_SRC)),
        ),
        (
            "/sources/restore",
            &*format!("source_id={}", urlenc(GODOT_SRC)),
        ),
        (
            "/owners/add",
            "owner_kind=engine&id=stride&name=Stride&tier=1",
        ),
        ("/owners/remove", "owner_id=godot"),
        ("/owners/restore", "owner_id=godot"),
    ] {
        let (status, _, _) = send(&st, post_form(uri, body, None)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
    }
    assert_eq!(st.registry.current().config.all_sources().len(), 3);

    let (_, _, body) = send(&st, Request::get("/sources").body(Body::empty()).unwrap()).await;
    assert!(!body.contains("/sources/add"));
    assert!(!body.contains("/sources/remove"));
}

#[tokio::test]
async fn admin_adds_and_removes_a_source() {
    let st = state();
    let cookie = login(&st).await;
    let (_, body) = get_as(&st, "/sources?owner=bevy", &cookie).await;
    assert!(body.contains(r#"id="add-source" class="panel" open"#));
    assert!(body.contains(r#"<option value="bevy" selected>"#));

    let form = "owner_id=bevy&kind=github&repo=bevyengine%2Fbevy&mode=releases\
                &feed_url=https%3A%2F%2Fignored.example&category=release&interval_minutes=30";
    let (status, headers, _) = send(&st, post_form("/sources/add", form, Some(&cookie))).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers[header::LOCATION], "/sources");
    let id = "bevy:github:bevyengine/bevy";
    let added = st
        .registry
        .current()
        .config
        .manageable_sources()
        .into_iter()
        .find(|s| s.id == id)
        .unwrap();
    assert_eq!(added.interval_minutes, Some(30));
    assert_eq!(added.category, Some(ItemKind::Release));
    let (_, body) = get_as(&st, "/sources", &cookie).await;
    assert!(body.contains("bevyengine&#x2f;bevy (releases)"));
    assert!(body.contains("added here"));

    let remove = format!("source_id={}&next=%2Fengine%2Fbevy", urlenc(id));
    let (status, headers, _) =
        send(&st, post_form("/sources/remove", &remove, Some(&cookie))).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers[header::LOCATION], "/engine/bevy");
    assert!(
        st.store
            .lock()
            .unwrap()
            .custom_sources()
            .unwrap()
            .is_empty()
    );
    let (_, body) = get_as(&st, "/sources", &cookie).await;
    assert!(!body.contains("bevyengine&#x2f;bevy"));
    assert!(
        !body.contains("Removed from sources.toml"),
        "nothing to restore"
    );
}

#[tokio::test]
async fn invalid_sources_rerender_the_form() {
    let st = state();
    let cookie = login(&st).await;
    for (form, message) in [
        (
            "owner_id=bevy&kind=feed&feed_url=ftp%3A%2F%2Fx.org",
            "not an http(s) URL",
        ),
        (
            "owner_id=bevy&kind=github&repo=",
            "Enter the GitHub repository",
        ),
        (
            "owner_id=bevy&kind=html&html_url=https%3A%2F%2Fx.org&item=a%5B&title=h2&link=a",
            "invalid item selector",
        ),
        (
            "owner_id=bevy&kind=hn&query=bevy&interval_minutes=soon",
            "whole number of minutes",
        ),
        ("owner_id=nope&kind=hn&query=bevy", "no engine or outlet"),
        ("kind=hn&query=bevy", "Choose the engine or outlet"),
        (
            "owner_id=godot&kind=github&repo=godotengine%2Fgodot",
            "Godot already has this source",
        ),
    ] {
        let (status, _, body) = send(&st, post_form("/sources/add", form, Some(&cookie))).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{form}");
        assert!(body.contains(message), "{form}: {message}");
        assert!(
            body.contains(r#"id="add-source" class="panel" open"#),
            "{form}"
        );
    }
    // The posted values are filled in again.
    let (_, _, body) = send(
        &st,
        post_form(
            "/sources/add",
            "owner_id=bevy&kind=hn&query=bevy+news&interval_minutes=soon",
            Some(&cookie),
        ),
    )
    .await;
    assert!(body.contains(r#"value="bevy news""#));
    assert!(body.contains(r#"<option value="hn" selected>"#));
    assert_eq!(st.registry.current().config.all_sources().len(), 3);
}

#[tokio::test]
async fn removing_a_config_source_purges_it_until_restored() {
    let st = state();
    let cookie = login(&st).await;
    let remove = format!("source_id={}", urlenc(GODOT_SRC));
    let (status, _, _) = send(&st, post_form("/sources/remove", &remove, Some(&cookie))).await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    // Its item is gone, so the outlet's duplicate is shown instead.
    let (_, _, body) = send(&st, Request::get("/").body(Body::empty()).unwrap()).await;
    assert!(!body.contains("Godot 4.8 released"));
    assert!(body.contains("Godot 4.8<"));
    let (_, body) = get_as(&st, "/sources", &cookie).await;
    assert!(body.contains("Removed from sources.toml (1)"));
    // Only the restore form still refers to it.
    let godot_value = r#"value="godot:github:godotengine&#x2f;godot""#;
    assert_eq!(body.matches(godot_value).count(), 1);

    // Removing it again is refused.
    let (status, _, _) = send(&st, post_form("/sources/remove", &remove, Some(&cookie))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let (status, _, _) = send(&st, post_form("/sources/restore", &remove, Some(&cookie))).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (_, body) = get_as(&st, "/sources", &cookie).await;
    assert!(!body.contains("Removed from sources.toml"));
    assert!(body.contains("godotengine&#x2f;godot"));
}

#[tokio::test]
async fn admin_adds_an_engine_and_its_first_source() {
    let st = state();
    let cookie = login(&st).await;
    let form = "owner_kind=engine&id=stride&name=Stride&homepage=https%3A%2F%2Fstride3d.net\
                &tier=1&csharp=native&aliases=stride%2C+Stride3D";
    let (status, headers, _) = send(&st, post_form("/owners/add", form, Some(&cookie))).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        headers[header::LOCATION],
        "/sources?owner=stride#add-source"
    );
    let engine = st
        .registry
        .current()
        .config
        .engine("stride")
        .cloned()
        .unwrap();
    assert_eq!(engine.aliases, ["stride", "Stride3D"]);

    // It shows up everywhere, with an empty group on /sources.
    let (status, body) = get_as(&st, "/engine/stride", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("native"));
    let (_, body) = get_as(&st, "/engines", &cookie).await;
    assert!(body.contains("Stride"));
    let (_, body) = get_as(&st, "/sources?owner=stride", &cookie).await;
    assert!(body.contains("0 of 0 enabled"));
    assert!(body.contains(r#"<option value="stride" selected>"#));

    let form = "owner_id=stride&kind=github&repo=stride3d%2Fstride";
    let (status, _, _) = send(&st, post_form("/sources/add", form, Some(&cookie))).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (_, body) = get_as(&st, "/engine/stride", &cookie).await;
    assert!(body.contains("stride3d&#x2f;stride (releases)"));

    // Removing it deletes it and its source outright.
    let (status, headers, _) = send(
        &st,
        post_form("/owners/remove", "owner_id=stride", Some(&cookie)),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers[header::LOCATION], "/sources");
    let (status, _) = get_as(&st, "/engine/stride", &cookie).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        st.store
            .lock()
            .unwrap()
            .custom_sources()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn invalid_owners_rerender_the_form() {
    let st = state();
    let cookie = login(&st).await;
    for (form, message) in [
        (
            "owner_kind=engine&id=godot&name=Godot+2&tier=1",
            "defined in sources.toml",
        ),
        (
            "owner_kind=engine&id=Bad+Id&name=X&tier=1",
            "be used as an id",
        ),
        ("owner_kind=engine&id=x&name=X&tier=7", "tier must be"),
        ("owner_kind=outlet&id=x&name=", "Enter a name"),
        ("id=x&name=X", "Choose engine or news outlet"),
    ] {
        let (status, _, body) = send(&st, post_form("/owners/add", form, Some(&cookie))).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{form}");
        assert!(body.contains(message), "{form}: {message}");
        assert!(
            body.contains(r#"id="add-owner" class="panel" open"#),
            "{form}"
        );
    }
    assert!(st.store.lock().unwrap().custom_owners().unwrap().is_empty());
}

#[tokio::test]
async fn removing_a_config_engine_until_restored() {
    let st = state();
    let cookie = login(&st).await;
    let (status, _, _) = send(
        &st,
        post_form("/owners/remove", "owner_id=godot", Some(&cookie)),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    let (status, _) = get_as(&st, "/engine/godot", &cookie).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Its own item is gone and the outlet's copy no longer carries the Godot tag.
    let (_, body) = get_as(&st, "/", &cookie).await;
    assert!(!body.contains("Godot 4.8 released"));
    assert!(body.contains("Godot 4.8<"));
    assert!(!body.contains(r#"href="/engine/godot""#));
    let (_, body) = get_as(&st, "/sources", &cookie).await;
    assert!(body.contains("Removed from sources.toml (1)"));
    assert!(!body.contains("godotengine&#x2f;godot"));

    let (status, _, _) = send(
        &st,
        post_form("/owners/restore", "owner_id=godot", Some(&cookie)),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (status, _) = get_as(&st, "/engine/godot", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    let (_, body) = get_as(&st, "/sources", &cookie).await;
    assert!(!body.contains("Removed from sources.toml"));
}
