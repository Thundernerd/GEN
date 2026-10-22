//! Unity editor releases from the public releases JSON API.

use super::{FetchError, FetchOutcome, Http};
use crate::model::SourceState;

pub async fn fetch(
    _http: &Http,
    _limit: u32,
    _state: &SourceState,
) -> Result<FetchOutcome, FetchError> {
    Err(FetchError::Unimplemented("unity"))
}
