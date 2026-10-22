//! Scraped HTML pages, using CSS selectors from config.

use super::{FetchError, FetchOutcome, Http};
use crate::model::SourceState;

/// CSS selectors; `title`, `link` and `date` are relative to `item`.
pub struct Selectors<'a> {
    pub item: &'a str,
    pub title: &'a str,
    pub link: &'a str,
    pub date: Option<&'a str>,
}

pub async fn fetch(
    _http: &Http,
    _url: &str,
    _selectors: &Selectors<'_>,
    _state: &SourceState,
) -> Result<FetchOutcome, FetchError> {
    Err(FetchError::Unimplemented("html"))
}
