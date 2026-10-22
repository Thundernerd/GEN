//! Discord webhook notifications.

use super::{NotifyContext, NotifyError};
use crate::model::Item;

pub async fn send(
    _ctx: &NotifyContext<'_>,
    _webhook_url: &str,
    _items: &[Item],
) -> Result<(), NotifyError> {
    Err(NotifyError::Other(
        "discord notifications are not implemented yet".into(),
    ))
}
