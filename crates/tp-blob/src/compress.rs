//! Сжатие объектов zstd (SRS 3.2; CONTRACTS.md#BlobStore «Сжатие», CTR-21).
//!
//! - Запись ([`crate::BlobWriter`]): объект сжимается одним кадром zstd с размером исходного содержимого
//!   в заголовке и контрольной суммой, если учёт хранит признак сжатия ([`BlobIndex::supports_compression`])
//!   и выигрыш не меньше [`CompressionConfig::min_gain_percent`]. Уже сжатые форматы — по типу MIME
//!   от вызывающего и по сигнатуре (`infer`) — не сжимаются. Хэш и `size` в учёте — от исходного содержимого,
//!   поэтому дедупликация не зависит от того, сжат ли объект.
//! - Чтение ([`Decompressing`]): обёртка над хранилищем распаковывает объекты с признаком
//!   [`Compression::Zstd`] потоком; диапазон — по исходному содержимому. Под обёрткой стоит кэш NVMe
//!   ([`crate::BlobCache`]) — он хранит сжатый вид. Порядок слоёв: `Decompressing` → `BlobCache` → диск или S3.

use std::io::{self, Write};
use std::ops::Range;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use schemars::JsonSchema;
use serde::Deserialize;
use time::OffsetDateTime;
use tp_contracts::blob::{BlobIndex, BlobStore, ByteStream, Compression, ObjectStream};
use tp_contracts::{BlobKey, Error, Result};
use zstd::stream::raw::{Decoder, InBuffer, Operation, OutBuffer};

use crate::clamp_range;

/// Наибольший заголовок кадра zstd (`ZSTD_FRAMEHEADERSIZE_MAX`): в нём размер исходного содержимого.
const FRAME_HEADER_MAX: u64 = 18;

/// Распакованных байтов за один шаг потока чтения.
const OUT_CHUNK: usize = 256 << 10;

/// Сжатие объектов при записи (секция `compression` писателя BLOB; поля необязательны, неизвестное — ошибка).
///
/// ```yaml
/// enabled: true
/// level: 3
/// min_gain_percent: 10
/// min_bytes: 512
/// skip_types: [image/jpeg, video/*]
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CompressionConfig {
    /// Сжимать объекты.
    pub enabled: bool,
    /// Уровень zstd (1–22; 3 — по умолчанию zstd).
    pub level: i32,
    /// Наименьший выигрыш, % исходного размера: меньше — объект хранится как есть.
    pub min_gain_percent: u8,
    /// Объекты меньше этого размера не сжимаются, байт.
    pub min_bytes: u64,
    /// Уже сжатые форматы — типы MIME (`*` в конце — префикс): по типу от вызывающего и по сигнатуре.
    pub skip_types: Vec<String>,
}

impl Default for CompressionConfig {
    fn default() -> Self {
        let skip = [
            "image/jpeg",
            "image/png",
            "image/gif",
            "image/webp",
            "image/heif",
            "image/avif",
            "image/jxl",
            "video/*",
            "audio/*",
            "application/zip",
            "application/gzip",
            "application/zstd",
            "application/x-bzip2",
            "application/x-xz",
            "application/x-7z-compressed",
            "application/vnd.rar",
            "application/x-rar-compressed",
            "application/java-archive",
            "application/epub+zip",
            "application/vnd.openxmlformats-officedocument.*",
            "application/vnd.oasis.opendocument.*",
        ];
        Self {
            enabled: true,
            level: 3,
            min_gain_percent: 10,
            min_bytes: 512,
            skip_types: skip.into_iter().map(str::to_owned).collect(),
        }
    }
}

/// Почему объект не сжат.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Skip {
    /// Сжатие выключено, учёт не хранит признак или объект мал.
    Off,
    /// Уже сжатый формат.
    Type,
    /// Выигрыш меньше порога.
    Gain,
}

impl CompressionConfig {
    /// Тип MIME — уже сжатый формат (параметры и регистр не важны).
    fn skips_type(&self, media_type: &str) -> bool {
        let t = media_type
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        self.skip_types.iter().any(|p| match p.strip_suffix('*') {
            Some(prefix) => t.starts_with(&prefix.to_ascii_lowercase()),
            None => t == p.to_ascii_lowercase(),
        })
    }

    /// Нужно ли пробовать сжатие: размер, тип от вызывающего, сигнатура начала объекта.
    pub(crate) fn precheck(
        &self,
        size: u64,
        media_type: Option<&str>,
        head: &[u8],
    ) -> Option<Skip> {
        if !self.enabled || size < self.min_bytes.max(1) {
            return Some(Skip::Off);
        }
        let by_hint = media_type.is_some_and(|t| self.skips_type(t));
        let by_magic = infer::get(head).is_some_and(|t| self.skips_type(t.mime_type()));
        (by_hint || by_magic).then_some(Skip::Type)
    }

    /// Сжимает части объекта размером `size` одним кадром; `Err(Skip::Gain)` — выигрыш меньше порога
    /// (сжатие прерывается, как только результат перерос допустимый размер). Работа процессора —
    /// вызывать вне потоков асинхронного рантайма.
    pub(crate) fn compress(&self, chunks: &[Bytes], size: u64) -> std::result::Result<Bytes, Skip> {
        let keep = 100 - u64::from(self.min_gain_percent.min(100));
        let limit = usize::try_from(size.saturating_mul(keep) / 100).unwrap_or(usize::MAX);
        let run = || -> io::Result<Vec<u8>> {
            let out = Bounded {
                buf: Vec::with_capacity(limit.min(1 << 20)),
                limit,
            };
            let mut enc = zstd::stream::write::Encoder::new(out, self.level)?;
            enc.set_pledged_src_size(Some(size))?;
            enc.include_contentsize(true)?;
            enc.include_checksum(true)?;
            for c in chunks {
                enc.write_all(c)?;
            }
            Ok(enc.finish()?.buf)
        };
        match run() {
            Ok(buf) => Ok(Bytes::from(buf)),
            Err(e) => {
                if e.kind() != io::ErrorKind::WriteZero {
                    tracing::warn!(error = %e, "сжатие объекта BLOB не удалось, хранится как есть");
                }
                Err(Skip::Gain)
            }
        }
    }
}

/// Буфер сжатого вывода с пределом: переполнение — `WriteZero` (выигрыш недостаточен).
struct Bounded {
    buf: Vec<u8>,
    limit: usize,
}

impl Write for Bounded {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.buf.len() + data.len() > self.limit {
            return Err(io::ErrorKind::WriteZero.into());
        }
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Размер исходного содержимого по заголовку кадра zstd; неполный или битый заголовок — `None`.
pub(crate) fn frame_content_size(header: &[u8]) -> Option<u64> {
    zstd::zstd_safe::get_frame_content_size(header)
        .ok()
        .flatten()
}

/// Размер исходного содержимого сжатого объекта: заголовок кадра — первые байты объекта.
/// Объекта нет — `None`; заголовок без размера или битый — `Error::Internal`.
pub(crate) async fn stored_content_size(
    store: &dyn BlobStore,
    key: &BlobKey,
) -> Result<Option<u64>> {
    let stream = match store.get(key, Some(0..FRAME_HEADER_MAX)).await {
        Ok(s) => s,
        Err(Error::NotFound) => return Ok(None),
        Err(e) => return Err(e),
    };
    let head = read_prefix(stream, FRAME_HEADER_MAX as usize).await?;
    frame_content_size(&head)
        .map(Some)
        .ok_or_else(|| Error::internal(format!("blob {key}: битый заголовок zstd")))
}

/// Читает начало потока не короче `n` байт (или весь поток, если он короче).
async fn read_prefix(mut stream: ByteStream, n: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        match stream.next().await {
            Some(chunk) => out.extend_from_slice(&chunk?),
            None => break,
        }
    }
    Ok(out)
}

/// Хранилище с распаковкой при чтении: `get` и `head` объектов с признаком [`Compression::Zstd`] — по исходному
/// содержимому; запись, удаление, листинг и `*_raw` — как у источника (сжимает писатель).
/// Объект без записи учёта (запись не завершена, «сирота») читается как есть.
pub struct Decompressing {
    inner: Arc<dyn BlobStore>,
    index: Arc<dyn BlobIndex>,
}

impl std::fmt::Debug for Decompressing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Decompressing").finish_non_exhaustive()
    }
}

impl Decompressing {
    /// Распаковка поверх `inner` (кэш или хранилище) по признакам учёта `index`.
    pub fn new(inner: Arc<dyn BlobStore>, index: Arc<dyn BlobIndex>) -> Self {
        Self { inner, index }
    }

    async fn compressed(&self, key: &BlobKey) -> Result<bool> {
        Ok(self.index.compression(key).await? == Some(Compression::Zstd))
    }
}

#[async_trait]
impl BlobStore for Decompressing {
    async fn put(&self, key: &BlobKey, data: Bytes) -> Result<()> {
        self.inner.put(key, data).await
    }

    async fn put_stream(
        &self,
        key: &BlobKey,
        data: ByteStream,
        size_hint: Option<u64>,
    ) -> Result<()> {
        self.inner.put_stream(key, data, size_hint).await
    }

    async fn get(&self, key: &BlobKey, range: Option<Range<u64>>) -> Result<ByteStream> {
        if !self.compressed(key).await? {
            return self.inner.get(key, range).await;
        }
        let mut stream = self.inner.get(key, None).await?;
        // Заголовок кадра — из начала того же потока: один запрос к хранилищу.
        let mut head = Vec::new();
        let size = loop {
            if let Some(size) = frame_content_size(&head) {
                break size;
            }
            match stream.next().await {
                Some(chunk) if head.len() < FRAME_HEADER_MAX as usize => {
                    head.extend_from_slice(&chunk?)
                }
                _ => return Err(Error::internal(format!("blob {key}: битый заголовок zstd"))),
            }
        };
        let range = clamp_range(range, size)?;
        let decoder = Decoder::new().map_err(|e| Error::internal(format!("zstd: {e}")))?;
        let state = Unzstd {
            inner: stream,
            decoder,
            input: Bytes::from(head),
            at: 0,
            pending: false,
            frame_done: false,
            pos: 0,
            size,
            range,
            key: *key,
            finished: false,
        };
        Ok(futures::stream::unfold(state, Unzstd::step).boxed())
    }

    async fn head(&self, key: &BlobKey) -> Result<Option<u64>> {
        if self.compressed(key).await? {
            stored_content_size(self.inner.as_ref(), key).await
        } else {
            self.inner.head(key).await
        }
    }

    async fn delete(&self, key: &BlobKey) -> Result<()> {
        self.inner.delete(key).await
    }

    fn list(&self, prefix: &str, older_than: OffsetDateTime) -> ObjectStream {
        self.inner.list(prefix, older_than)
    }

    async fn put_raw(&self, path: &str, data: Bytes) -> Result<()> {
        self.inner.put_raw(path, data).await
    }

    async fn get_raw(&self, path: &str, range: Option<Range<u64>>) -> Result<ByteStream> {
        self.inner.get_raw(path, range).await
    }

    async fn delete_raw(&self, path: &str) -> Result<()> {
        self.inner.delete_raw(path).await
    }
}

/// Состояние потоковой распаковки диапазона `range` исходного содержимого.
struct Unzstd {
    inner: ByteStream,
    decoder: Decoder<'static>,
    /// Текущий кусок сжатых данных и прочитанная его часть.
    input: Bytes,
    at: usize,
    /// Прошлый шаг заполнил буфер вывода: в декодере могут остаться данные.
    pending: bool,
    /// Кадр распакован до конца.
    frame_done: bool,
    /// Распаковано байтов исходного содержимого.
    pos: u64,
    /// Размер исходного содержимого по заголовку кадра.
    size: u64,
    range: Range<u64>,
    key: BlobKey,
    finished: bool,
}

impl Unzstd {
    async fn step(mut self) -> Option<(Result<Bytes>, Self)> {
        loop {
            // Чтение до конца объекта распаковывает кадр целиком: zstd сверяет контрольную сумму.
            let tail = self.range.end == self.size && !self.frame_done;
            if self.finished || (self.pos >= self.range.end && !tail) {
                return None;
            }
            if self.at == self.input.len() && !self.pending {
                match self.inner.next().await {
                    Some(Ok(chunk)) => {
                        self.input = chunk;
                        self.at = 0;
                        continue;
                    }
                    Some(Err(e)) => return self.fail(e),
                    None => {
                        let e = Error::internal(format!(
                            "blob {}: сжатый объект оборван на {} байт",
                            self.key, self.pos
                        ));
                        return self.fail(e);
                    }
                }
            }
            if self.frame_done || self.pos > self.size {
                // Кадр кончился раньше размера из заголовка, за ним лишние данные или он длиннее заголовка.
                let e = Error::internal(format!(
                    "blob {}: размер не совпал с заголовком zstd",
                    self.key
                ));
                return self.fail(e);
            }
            let mut out = Vec::with_capacity(OUT_CHUNK);
            let mut input = InBuffer::around(&self.input[self.at..]);
            let mut output = OutBuffer::around(&mut out);
            let hint = match self.decoder.run(&mut input, &mut output) {
                Ok(hint) => hint,
                Err(e) => {
                    let e = Error::internal(format!("blob {}: zstd: {e}", self.key));
                    return self.fail(e);
                }
            };
            self.at += input.pos();
            let n = output.pos();
            self.pending = n == OUT_CHUNK;
            self.frame_done = hint == 0;
            let start = self.pos;
            self.pos += n as u64;
            let from = self.range.start.max(start) - start;
            let to = self.range.end.min(self.pos).saturating_sub(start);
            if from < to {
                out.truncate(to as usize);
                let chunk = Bytes::from(out).slice(from as usize..);
                return Some((Ok(chunk), self));
            }
        }
    }

    fn fail(mut self, e: Error) -> Option<(Result<Bytes>, Self)> {
        self.finished = true;
        Some((Err(e), self))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::writer::{BlobHasher, BlobWriter, WriterConfig};
    use crate::{BlobCache, CacheConfig};
    use tp_contracts::blob::{Acquire, Commit};
    use tp_contracts::{BlobHash, Gen, TenantId};
    use tp_testkit::blob::{MemBlobIndex, MemBlobStore, read_all};

    const T: TenantId = TenantId::DEFAULT;

    struct Env {
        index: MemBlobIndex,
        store: MemBlobStore,
        writer: BlobWriter,
        reader: Decompressing,
    }

    fn env(cfg: WriterConfig) -> Env {
        let index = MemBlobIndex::default();
        let store = MemBlobStore::default();
        let writer = BlobWriter::new(Arc::new(index.clone()), Arc::new(store.clone()), cfg);
        let reader = Decompressing::new(Arc::new(store.clone()), Arc::new(index.clone()));
        Env {
            index,
            store,
            writer,
            reader,
        }
    }

    /// Текст письма: хорошо сжимается.
    fn text(len: usize) -> Bytes {
        let line =
            b"Subject: report\r\nThe quick brown fox jumps over the lazy dog. 0123456789\r\n";
        Bytes::from(line.iter().copied().cycle().take(len).collect::<Vec<u8>>())
    }

    /// Псевдослучайные байты: не сжимаются.
    fn noise(len: usize) -> Bytes {
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        Bytes::from(
            (0..len)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    (x >> 24) as u8
                })
                .collect::<Vec<u8>>(),
        )
    }

    async fn read(
        store: &dyn BlobStore,
        key: &BlobKey,
        range: Option<Range<u64>>,
    ) -> Result<Bytes> {
        read_all(store.get(key, range).await?).await
    }

    #[tokio::test]
    async fn roundtrip_with_ranges() {
        let e = env(WriterConfig::default());
        // Больше буфера распаковки: несколько шагов потока.
        let data = text(3 * OUT_CHUNK + 1234);
        let key = e.writer.write(T, data.clone(), 1).await.expect("write");
        assert_eq!(
            key.hash,
            BlobHasher::digest(&data),
            "хэш исходного содержимого"
        );
        let stored = e.store.bytes(&key.path()).expect("объект");
        assert!(stored.len() * 10 < data.len(), "{}", stored.len());
        assert_eq!(e.index.compression(&key).await, Ok(Some(Compression::Zstd)));
        assert_eq!(
            e.index.row(T, &key.hash).expect("row").size,
            data.len() as u64
        );

        assert_eq!(read(&e.reader, &key, None).await.expect("read"), data);
        let n = data.len() as u64;
        let edge = OUT_CHUNK as u64;
        for r in [
            0..1,
            5..100,
            edge - 3..edge + 3,
            edge..2 * edge,
            n - 10..n,
            n - 1..n + 100,
            n..n,
        ] {
            let want = data.slice(r.start as usize..r.end.min(n) as usize);
            let got = read(&e.reader, &key, Some(r.clone())).await.expect("range");
            assert_eq!(got, want, "{r:?}");
        }
        assert!(matches!(
            read(&e.reader, &key, Some(n + 1..n + 2)).await,
            Err(Error::Invalid(_))
        ));
        assert_eq!(e.reader.head(&key).await, Ok(Some(n)));
        assert_eq!(e.store.head(&key).await, Ok(Some(stored.len() as u64)));

        let m = e.writer.metrics();
        assert_eq!((m.compressed, m.original_bytes), (1, n));
        assert!(m.compression_ratio() < 0.1, "{}", m.compression_ratio());
    }

    #[tokio::test]
    async fn stream_write_compresses() {
        let e = env(WriterConfig::default());
        let data = text(100_000);
        let parts: Vec<Result<Bytes>> = data
            .chunks(7_777)
            .map(|c| Ok(Bytes::copy_from_slice(c)))
            .collect();
        let key = e
            .writer
            .write_stream(T, futures::stream::iter(parts).boxed(), 1)
            .await
            .expect("write");
        assert_eq!(e.index.compression(&key).await, Ok(Some(Compression::Zstd)));
        assert_eq!(read(&e.reader, &key, None).await.expect("read"), data);
    }

    #[tokio::test]
    async fn dedup_compressed_and_plain() {
        // Сначала без сжатия, затем писатель со сжатием: та же запись учёта, без загрузки.
        let plain = env(WriterConfig {
            compression: CompressionConfig {
                enabled: false,
                ..CompressionConfig::default()
            },
            ..WriterConfig::default()
        });
        let zipping = BlobWriter::new(
            Arc::new(plain.index.clone()),
            Arc::new(plain.store.clone()),
            WriterConfig::default(),
        );
        let data = text(10_000);
        let a = plain.writer.write(T, data.clone(), 1).await.expect("write");
        let b = zipping.write(T, data.clone(), 2).await.expect("write");
        assert_eq!(a, b);
        assert_eq!(zipping.metrics().dedup, 1);
        assert_eq!(
            plain.index.compression(&a).await,
            Ok(Some(Compression::None))
        );
        assert_eq!(plain.index.row(T, &a.hash).expect("row").refcnt, 3);
        assert_eq!(read(&plain.reader, &a, None).await.expect("read"), data);

        // Наоборот: сжатый объект найден писателем без сжатия.
        let other = text(10_001);
        let c = zipping.write(T, other.clone(), 1).await.expect("write");
        let d = plain
            .writer
            .write(T, other.clone(), 1)
            .await
            .expect("write");
        assert_eq!(c, d);
        assert_eq!(
            plain.index.compression(&c).await,
            Ok(Some(Compression::Zstd))
        );
        assert_eq!(read(&plain.reader, &c, None).await.expect("read"), other);
    }

    #[tokio::test]
    async fn compressed_formats_are_stored_as_is() {
        let e = env(WriterConfig::default());
        // JPEG по сигнатуре: тело из нулей сжалось бы, но формат уже сжатый.
        let mut jpeg = b"\xFF\xD8\xFF\xE0\x00\x10JFIF\x00\x01".to_vec();
        jpeg.resize(8192, 0);
        let jpeg = Bytes::from(jpeg);
        let key = e.writer.write(T, jpeg.clone(), 1).await.expect("write");
        assert_eq!(e.store.bytes(&key.path()), Some(jpeg.clone()));
        assert_eq!(e.index.compression(&key).await, Ok(Some(Compression::None)));
        assert_eq!(
            read(&e.reader, &key, Some(2..4)).await.expect("read"),
            jpeg.slice(2..4)
        );

        // По типу MIME части: текст с типом image/png не сжимается.
        let data = text(8192);
        let key = e
            .writer
            .write_part(
                T,
                BlobHasher::digest(&data),
                data.clone(),
                1,
                Some("Image/PNG; name=a.png"),
            )
            .await
            .expect("write");
        assert_eq!(e.store.bytes(&key.path()), Some(data));
        // Шаблон с `*`: docx.
        assert!(
            CompressionConfig::default().skips_type(
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
            )
        );
        assert!(!CompressionConfig::default().skips_type("text/plain"));

        // Несжимаемое без сигнатуры: выигрыш меньше порога.
        let rnd = noise(8192);
        let key = e.writer.write(T, rnd.clone(), 1).await.expect("write");
        assert_eq!(e.store.bytes(&key.path()), Some(rnd));
        // Малый объект не сжимается.
        let key = e.writer.write(T, text(100), 1).await.expect("write");
        assert_eq!(e.index.compression(&key).await, Ok(Some(Compression::None)));

        let m = e.writer.metrics();
        assert_eq!((m.compressed, m.skipped_type, m.skipped_gain), (0, 2, 1));
        assert!((m.compression_ratio() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn gain_threshold() {
        let cfg = CompressionConfig::default();
        let data = text(4096);
        let zipped = cfg
            .compress(std::slice::from_ref(&data), 4096)
            .expect("сжимается");
        assert_eq!(frame_content_size(&zipped), Some(4096));
        // Порог выше достижимого выигрыша.
        let strict = CompressionConfig {
            min_gain_percent: 100,
            ..cfg
        };
        assert_eq!(strict.compress(&[data], 4096), Err(Skip::Gain));
    }

    /// Учёт без признака сжатия (методы контракта по умолчанию).
    struct Plain(MemBlobIndex);

    #[async_trait]
    impl BlobIndex for Plain {
        async fn acquire(&self, t: TenantId, h: &BlobHash, refs: u32) -> Result<Acquire> {
            self.0.acquire(t, h, refs).await
        }
        async fn commit_new(
            &self,
            t: TenantId,
            h: &BlobHash,
            g: Gen,
            size: u64,
            refs: u32,
        ) -> Result<Commit> {
            self.0.commit_new(t, h, g, size, refs).await
        }
        async fn release(&self, t: TenantId, h: &BlobHash, g: Gen, refs: u32) -> Result<()> {
            self.0.release(t, h, g, refs).await
        }
        async fn gc_candidates(&self, o: OffsetDateTime, l: usize) -> Result<Vec<BlobKey>> {
            self.0.gc_candidates(o, l).await
        }
        async fn mark_deleting(&self, k: &BlobKey, o: OffsetDateTime) -> Result<bool> {
            self.0.mark_deleting(k, o).await
        }
        async fn finalize_delete(&self, k: &BlobKey) -> Result<bool> {
            self.0.finalize_delete(k).await
        }
        async fn exists(&self, k: &BlobKey) -> Result<bool> {
            self.0.exists(k).await
        }
    }

    #[tokio::test]
    async fn index_without_compression_stores_plain() {
        let store = MemBlobStore::default();
        let writer = BlobWriter::new(
            Arc::new(Plain(MemBlobIndex::default())),
            Arc::new(store.clone()),
            WriterConfig::default(),
        );
        let data = text(10_000);
        let key = writer.write(T, data.clone(), 1).await.expect("write");
        assert_eq!(store.bytes(&key.path()), Some(data));
    }

    #[tokio::test]
    async fn damaged_object_is_an_error() {
        let e = env(WriterConfig::default());
        let data = text(50_000);
        let key = e.writer.write(T, data, 1).await.expect("write");
        let stored = e.store.bytes(&key.path()).expect("объект");
        // Испорчен конец (контрольная сумма), оборван, заменён мусором.
        let mut flipped = stored.to_vec();
        let last = flipped.len() - 1;
        flipped[last] ^= 0xFF;
        let cut = stored.slice(..stored.len() / 2);
        for bad in [Bytes::from(flipped), cut, Bytes::from_static(b"garbage")] {
            // Объекты неизменяемы: подмена — удалением и новой записью.
            e.store.delete(&key).await.expect("delete");
            e.store.put(&key, bad).await.expect("put");
            let r = read(&e.reader, &key, None).await;
            assert!(
                matches!(r, Err(Error::Internal(_))),
                "битый объект читается без ошибки: {:?}",
                r.map(|b| b.len())
            );
        }
    }

    #[tokio::test]
    async fn cache_keeps_compressed_view() {
        let dir = tempfile::tempdir().expect("dir");
        let index = MemBlobIndex::default();
        let source = MemBlobStore::default();
        let cache = Arc::new(
            BlobCache::open(
                CacheConfig {
                    dir: dir.path().to_path_buf(),
                    ..CacheConfig::default()
                },
                Arc::new(source.clone()),
            )
            .await
            .expect("cache"),
        );
        let writer = BlobWriter::new(
            Arc::new(index.clone()),
            cache.clone(),
            WriterConfig::default(),
        );
        let reader = Decompressing::new(cache, Arc::new(index));
        let data = text(20_000);
        let key = writer.write(T, data.clone(), 1).await.expect("write");
        assert_eq!(read(&reader, &key, None).await.expect("read"), data);
        let file = std::fs::read(dir.path().join("objects").join(key.path())).expect("файл кэша");
        assert!(
            file.starts_with(&[0x28, 0xB5, 0x2F, 0xFD]),
            "кэш хранит кадр zstd"
        );
        assert_eq!(
            Bytes::from(file),
            source.bytes(&key.path()).expect("объект")
        );
    }
}
