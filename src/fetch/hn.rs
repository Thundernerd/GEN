//! Hacker News stories via the Algolia search API.

use super::{FetchError, FetchOutcome, Http};
use crate::model::SourceState;

pub async fn fetch(
    _http: &Http,
    _query: &str,
    _state: &SourceState,
) -> Result<FetchOutcome, FetchError> {
    Err(FetchError::Unimplemented("hn"))
}
