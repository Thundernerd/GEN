//! RSS / Atom / JSON Feed sources (via `feed-rs`).

use super::{FetchError, FetchOutcome, Http};
use crate::model::SourceState;

pub async fn fetch(
    _http: &Http,
    _url: &str,
    _cache_bust: bool,
    _state: &SourceState,
) -> Result<FetchOutcome, FetchError> {
    Err(FetchError::Unimplemented("feed"))
}
