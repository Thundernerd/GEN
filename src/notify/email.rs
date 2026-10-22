//! Email notifications over SMTP (lettre).

use super::{NotifyContext, NotifyError};
use crate::config::ChannelTarget;
use crate::model::Item;

/// `target` is always `ChannelTarget::Email`.
pub async fn send(
    _ctx: &NotifyContext<'_>,
    _target: &ChannelTarget,
    _items: &[Item],
) -> Result<(), NotifyError> {
    Err(NotifyError::Other(
        "email notifications are not implemented yet".into(),
    ))
}
