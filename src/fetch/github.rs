//! GitHub releases, tags or commits for a repository.

use super::{FetchError, FetchOutcome, Http};
use crate::config::GithubMode;
use crate::model::SourceState;

pub async fn fetch(
    _http: &Http,
    _repo: &str,
    _mode: GithubMode,
    _tag_prefix: Option<&str>,
    _state: &SourceState,
) -> Result<FetchOutcome, FetchError> {
    Err(FetchError::Unimplemented("github"))
}
