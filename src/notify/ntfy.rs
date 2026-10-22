//! ntfy push notifications.

use super::{NotifyContext, NotifyError};
use crate::model::Item;

pub async fn send(
    _ctx: &NotifyContext<'_>,
    _server: &str,
    _topic: &str,
    _token: Option<&str>,
    _items: &[Item],
) -> Result<(), NotifyError> {
    Err(NotifyError::Other(
        "ntfy notifications are not implemented yet".into(),
    ))
}
