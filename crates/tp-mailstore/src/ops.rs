//! Флаги, копирование, перемещение (MS-04).

use tp_contracts::meta::{AccountScope, FlagOp, FlagsUpdate, Inserted};
use tp_contracts::{Error, MailboxId, MessageId, ModSeq, Result};

use crate::Store;

impl Store {
    pub(crate) async fn set_flags_impl(
        &self,
        _scope: AccountScope,
        _ids: &[MessageId],
        _op: FlagOp,
        _unchanged_since: Option<ModSeq>,
    ) -> Result<FlagsUpdate> {
        Err(Error::internal("set_flags: MS-04"))
    }

    pub(crate) async fn copy_impl(
        &self,
        _scope: AccountScope,
        _ids: &[MessageId],
        _to: MailboxId,
    ) -> Result<Vec<Inserted>> {
        Err(Error::internal("copy: MS-04"))
    }

    pub(crate) async fn move_impl(
        &self,
        _scope: AccountScope,
        _ids: &[MessageId],
        _to: MailboxId,
    ) -> Result<Vec<Inserted>> {
        Err(Error::internal("move_to: MS-04"))
    }

    pub(crate) async fn trash_impl(
        &self,
        _scope: AccountScope,
        _ids: &[MessageId],
    ) -> Result<ModSeq> {
        Err(Error::internal("delete_to_trash: MS-05"))
    }

    pub(crate) async fn restore_impl(
        &self,
        _scope: AccountScope,
        _ids: &[MessageId],
    ) -> Result<Vec<Inserted>> {
        Err(Error::internal("restore: MS-05"))
    }
}
