//! `COMPRESS=DEFLATE` (RFC 4978): сжатие потока после успешного `COMPRESS DEFLATE`.
//!
//! После ответа `OK` оба направления — «сырой» DEFLATE (RFC 1951, без заголовка zlib). Ответы сервера
//! сжимаются при каждой отправке с `Z_SYNC_FLUSH`: клиент получает их сразу. Принятое распаковывается
//! частями и передаётся разбору команд, который ограничивает размер сообщения как без сжатия.
//! Повторный `COMPRESS` — `NO [COMPRESSIONACTIVE]`; при `compress.enabled: false` возможность не объявляется.

use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress, Status};
use tp_contracts::{Error, Result};

use crate::proto;
use crate::proto::types::core::Tag;
use crate::proto::types::extensions::compress::CompressionAlgorithm;
use crate::proto::types::response::Code;
use crate::session::{Session, Transport};

/// Часть распакованных данных, передаваемая разбору за раз, байт.
const INFLATE_CHUNK: usize = 64 * 1024;

/// Состояние сжатия сессии.
pub(crate) struct Deflate {
    out: Compress,
    inp: Decompress,
}

impl Deflate {
    pub(crate) fn new(level: u32) -> Self {
        Self {
            out: Compress::new(Compression::new(level.min(9)), false),
            inp: Decompress::new(false),
        }
    }

    /// Сжимает данные для отправки (с `Z_SYNC_FLUSH`).
    pub(crate) fn compress(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(data.len() / 2 + 64);
        let mut consumed = 0usize;
        loop {
            if out.capacity() - out.len() < 64 {
                out.reserve(out.capacity().max(1024));
            }
            let before = self.out.total_in();
            self.out
                .compress_vec(&data[consumed..], &mut out, FlushCompress::Sync)
                .map_err(|e| Error::internal(format!("deflate: {e}")))?;
            let used = usize::try_from(self.out.total_in() - before).unwrap_or(usize::MAX);
            consumed = consumed.saturating_add(used).min(data.len());
            // Сброс завершён, когда вход исчерпан и выходу хватило места.
            if consumed >= data.len() && out.len() < out.capacity() {
                return Ok(out);
            }
        }
    }

    /// Распаковывает принятое; `sink` получает данные частями. Ошибка потока — `Error::Invalid`.
    pub(crate) fn decompress(&mut self, data: &[u8], mut sink: impl FnMut(&[u8])) -> Result<()> {
        let mut consumed = 0usize;
        let mut out = Vec::with_capacity(INFLATE_CHUNK);
        loop {
            out.clear();
            let before = self.inp.total_in();
            let status = self
                .inp
                .decompress_vec(&data[consumed..], &mut out, FlushDecompress::Sync)
                .map_err(|e| Error::invalid(format!("inflate: {e}")))?;
            let used = usize::try_from(self.inp.total_in() - before).unwrap_or(usize::MAX);
            consumed = consumed.saturating_add(used).min(data.len());
            if !out.is_empty() {
                sink(&out);
            }
            let full = out.len() == out.capacity();
            let stalled = used == 0 && out.is_empty();
            if status == Status::StreamEnd || stalled || (consumed >= data.len() && !full) {
                return Ok(());
            }
        }
    }
}

impl<T: Transport> Session<T> {
    /// `COMPRESS DEFLATE`.
    pub(crate) async fn compress(
        &mut self,
        tag: &Tag<'static>,
        algorithm: CompressionAlgorithm,
    ) -> Result<()> {
        let cfg = &self.ctx.config.compress;
        if !cfg.enabled {
            self.bad(Some(tag), "Command not supported");
            return Ok(());
        }
        if self.deflate.is_some() {
            self.no(tag, Some(Code::CompressionActive), "DEFLATE already active");
            return Ok(());
        }
        // non_exhaustive: другие алгоритмы — отказ.
        if !matches!(algorithm, CompressionAlgorithm::Deflate) {
            self.no(
                tag,
                Some(proto::code("CANNOT")),
                "Unsupported compression algorithm",
            );
            return Ok(());
        }
        let level = cfg.level;
        self.ok(tag, None, "DEFLATE active");
        // Ответ уходит без сжатия, дальше — сжатый поток.
        self.flush().await?;
        self.deflate = Some(Deflate::new(level));
        Ok(())
    }

    /// Принятые байты — разбору команд (через распаковку, если сжатие включено).
    pub(crate) fn received(&mut self, data: &[u8]) -> Result<()> {
        match self.deflate.as_mut() {
            Some(d) => {
                let reader = &mut self.reader;
                d.decompress(data, |chunk| reader.push(chunk))
            }
            None => {
                self.reader.push(data);
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_in_pieces() {
        let mut a = Deflate::new(6);
        let mut b = Deflate::new(6);
        let text: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let mut got = Vec::new();
        for part in text.chunks(70_000) {
            let z = a.compress(part).expect("compress");
            // Сжатое приходит произвольными частями.
            for piece in z.chunks(1000) {
                b.decompress(piece, |c| got.extend_from_slice(c))
                    .expect("decompress");
            }
        }
        assert_eq!(got, text);
        assert!(b.decompress(b"\xff\xff\xff garbage", |_| {}).is_err());
    }
}
