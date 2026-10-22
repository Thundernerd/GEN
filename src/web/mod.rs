//! HTTP server: HTML pages, the combined Atom feed and a JSON API.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::routing::get;

use crate::config::{AppConfig, SourcesConfig};
use crate::store::Store;

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Mutex<Store>>,
    pub sources: Arc<SourcesConfig>,
    pub config: Arc<AppConfig>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}
