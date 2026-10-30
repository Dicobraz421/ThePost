//! Выдача письма (MS-03).

use std::ops::Range;

use tp_contracts::blob::ByteStream;
use tp_contracts::meta::AccountScope;
use tp_contracts::{Error, MessageId, Result};

use crate::Store;

impl Store {
    pub(crate) async fn fetch_raw_impl(
        &self,
        _scope: AccountScope,
        _message: MessageId,
        _range: Option<Range<u64>>,
    ) -> Result<ByteStream> {
        Err(Error::internal("fetch_raw: MS-03"))
    }

    pub(crate) async fn fetch_part_impl(
        &self,
        _scope: AccountScope,
        _message: MessageId,
        _part_path: &str,
    ) -> Result<ByteStream> {
        Err(Error::internal("fetch_part: MS-03"))
    }
}
