//! S3 BlobStore на `object_store` (SRS 3.2; эталон — SeaweedFS).
//!
//! Объекты от `multipart_threshold_bytes` загружаются multipart: части пишутся по мере поступления
//! потока, объект становится видимым только после завершения загрузки; при ошибке загрузка
//! отменяется. Повторы с экспоненциальным backoff и таймауты — средствами клиента `object_store`.
//!
//! Канал до S3 — только `https://` с проверкой сертификата (CONTRACTS.md#Internal TLS): корни — из
//! `tls.ca_file` или системные. `http://` — только профиль dev с `tls.allow_plaintext`, с предупреждением
//! в лог при каждом создании клиента; в остальных случаях — ошибка конфигурации.

use std::fmt;
use std::ops::Range;
use std::path::Path as FsPath;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use bytes::Bytes;
use futures::{StreamExt, TryStreamExt, future, stream};
use object_store::aws::AmazonS3Builder;
use object_store::path::Path;
use object_store::prefix::PrefixStore;
use object_store::{
    BackoffConfig, Certificate, ClientOptions, GetOptions, GetRange, ObjectStore, ObjectStoreExt,
    PutPayload, RetryConfig, WriteMultipart,
};
use schemars::JsonSchema;
use time::OffsetDateTime;
use tp_contracts::blob::{BlobStore, ByteStream, ObjectEntry, ObjectStream};
use tp_contracts::tls::{InternalChannel, InternalTls, Profile, Transport};
use tp_contracts::{BlobKey, Error, Result};

use crate::check_raw_path;

/// Конфигурация S3-хранилища. Ключи доступа, не заданные здесь, берутся из окружения
/// (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, роль экземпляра).
#[derive(Clone, PartialEq, Eq, JsonSchema)]
pub struct S3Config {
    /// Адрес S3 API (`https://seaweedfs:8333`); `None` — AWS по региону (https).
    pub endpoint: Option<String>,
    /// Бакет.
    pub bucket: String,
    /// Регион.
    pub region: String,
    /// Префикс ключей внутри бакета; пустой — без префикса.
    pub prefix: String,
    /// Идентификатор ключа доступа.
    pub access_key_id: Option<String>,
    /// Секрет ключа доступа.
    pub secret_access_key: Option<String>,
    /// Адресация path-style (`endpoint/bucket/key`) — нужна SeaweedFS и MinIO.
    pub path_style: bool,
    /// TLS канала до S3: `ca_file` — корни для проверки сервера, `allow_plaintext` — `http://` в профиле dev.
    /// Клиентский сертификат (mTLS) клиент S3 не поддерживает; `server_name` — только равный узлу адреса.
    pub tls: InternalTls,
    /// Объекты не меньше этого размера загружаются multipart, байт.
    pub multipart_threshold_bytes: u64,
    /// Размер части multipart, байт (S3 — не меньше 5 МиБ, кроме последней).
    pub multipart_part_bytes: usize,
    /// Одновременно загружаемых частей одного объекта.
    pub multipart_concurrency: usize,
    /// Повторов запроса при временных сбоях; 0 — без повторов.
    pub max_retries: usize,
    /// Начальная пауза backoff, мс.
    pub retry_init_backoff_ms: u64,
    /// Максимальная пауза backoff, мс.
    pub retry_max_backoff_ms: u64,
    /// Предел времени повторов запроса от первой попытки, мс.
    pub retry_timeout_ms: u64,
    /// Таймаут запроса, мс.
    pub request_timeout_ms: u64,
    /// Таймаут установления соединения, мс.
    pub connect_timeout_ms: u64,
}

impl Default for S3Config {
    fn default() -> Self {
        Self {
            endpoint: None,
            bucket: "thepost-blob".into(),
            region: "us-east-1".into(),
            prefix: String::new(),
            access_key_id: None,
            secret_access_key: None,
            path_style: true,
            tls: InternalTls::default(),
            multipart_threshold_bytes: 16 << 20,
            multipart_part_bytes: 8 << 20,
            multipart_concurrency: 4,
            max_retries: 10,
            retry_init_backoff_ms: 100,
            retry_max_backoff_ms: 15_000,
            retry_timeout_ms: 180_000,
            request_timeout_ms: 30_000,
            connect_timeout_ms: 5_000,
        }
    }
}

impl fmt::Debug for S3Config {
    /// Секрет ключа доступа не выводится.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3Config")
            .field("endpoint", &self.endpoint)
            .field("bucket", &self.bucket)
            .field("region", &self.region)
            .field("prefix", &self.prefix)
            .field("access_key_id", &self.access_key_id)
            .field(
                "secret_access_key",
                &self.secret_access_key.as_ref().map(|_| "***"),
            )
            .field("path_style", &self.path_style)
            .field("tls", &self.tls)
            .field("multipart_threshold_bytes", &self.multipart_threshold_bytes)
            .field("multipart_part_bytes", &self.multipart_part_bytes)
            .field("multipart_concurrency", &self.multipart_concurrency)
            .field("max_retries", &self.max_retries)
            .field("retry_init_backoff_ms", &self.retry_init_backoff_ms)
            .field("retry_max_backoff_ms", &self.retry_max_backoff_ms)
            .field("retry_timeout_ms", &self.retry_timeout_ms)
            .field("request_timeout_ms", &self.request_timeout_ms)
            .field("connect_timeout_ms", &self.connect_timeout_ms)
            .finish()
    }
}

impl S3Config {
    /// Проверяет адрес и секцию TLS (`thepost config check` и запуск): `https://` с проверкой
    /// сертификата; `http://` — только профиль dev с `tls.allow_plaintext`. Файлы не читаются.
    pub fn check(&self, profile: Profile) -> Result<Transport> {
        let channel = InternalChannel::S3;
        let transport = match &self.endpoint {
            Some(url) => self.tls.check_url(channel, url, profile)?,
            None => {
                self.tls.validate(channel, profile)?;
                Transport::Tls
            }
        };
        if self.tls.cert_file.is_some() || self.tls.key_file.is_some() {
            return Err(Error::invalid(
                "S3: tls.cert_file/tls.key_file (mTLS) are not supported by the S3 client",
            ));
        }
        if let Some(name) = &self.tls.server_name {
            // Клиент S3 проверяет сертификат по узлу из адреса; другое имя не подставить.
            let host = self.endpoint.as_deref().and_then(url_host);
            if host.is_none_or(|host| !host.eq_ignore_ascii_case(name)) {
                return Err(Error::invalid(
                    "S3: tls.server_name must match the host of endpoint",
                ));
            }
        }
        Ok(transport)
    }
}

/// Узел из адреса `scheme://[userinfo@]host[:port][/...]`; IPv6 — без скобок.
fn url_host(url: &str) -> Option<&str> {
    let (_, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = match host_port.strip_prefix('[') {
        Some(v6) => v6.split_once(']')?.0,
        None => host_port.split(':').next().unwrap_or_default(),
    };
    (!host.is_empty()).then_some(host)
}

/// Корневые сертификаты из `tls.ca_file` (PEM). Блокирующий I/O: вызывается при создании клиента.
fn read_ca(path: &FsPath) -> Result<Vec<Certificate>> {
    let bad =
        |e: &dyn fmt::Display| Error::invalid(format!("S3: tls.ca_file {}: {e}", path.display()));
    let pem = std::fs::read(path).map_err(|e| bad(&e))?;
    let certs = Certificate::from_pem_bundle(&pem).map_err(|e| bad(&e))?;
    // Без сертификатов проверка сервера не пройдёт ни разу — это ошибка настройки.
    if certs.is_empty() {
        return Err(bad(&"no PEM certificates"));
    }
    Ok(certs)
}

/// BlobStore поверх S3 (или любого `ObjectStore` — для тестов).
#[derive(Debug)]
pub struct S3Store {
    store: Arc<dyn ObjectStore>,
    cfg: S3Config,
}

impl S3Store {
    /// Клиент S3 по конфигурации; `profile` — профиль узла (`profile` config.yaml). Адрес и секция
    /// TLS проверяются [`S3Config::check`], `tls.ca_file` читается сразу; сеть не используется.
    pub fn new(cfg: S3Config, profile: Profile) -> Result<Self> {
        let ms = Duration::from_millis;
        let transport = cfg.check(profile)?;
        let allow_http = transport == Transport::Plaintext;
        let mut client = ClientOptions::new()
            .with_allow_http(allow_http)
            .with_timeout(ms(cfg.request_timeout_ms))
            .with_connect_timeout(ms(cfg.connect_timeout_ms));
        match transport {
            Transport::Plaintext => {
                tracing::warn!("{}", InternalChannel::S3.plaintext_warning());
            }
            Transport::Tls => {
                if let Some(path) = &cfg.tls.ca_file {
                    // CA из секции заменяет системные корни: доверие — только своему центру.
                    client = client.with_no_system_certificates(true);
                    for cert in read_ca(path)? {
                        client = client.with_root_certificate(cert);
                    }
                }
            }
        }
        let mut builder = AmazonS3Builder::from_env()
            .with_bucket_name(&cfg.bucket)
            .with_region(&cfg.region)
            .with_virtual_hosted_style_request(!cfg.path_style)
            .with_allow_http(allow_http)
            .with_retry(RetryConfig {
                backoff: BackoffConfig {
                    init_backoff: ms(cfg.retry_init_backoff_ms),
                    max_backoff: ms(cfg.retry_max_backoff_ms),
                    base: 2.0,
                },
                max_retries: cfg.max_retries,
                retry_timeout: ms(cfg.retry_timeout_ms),
            })
            .with_client_options(client);
        if let Some(endpoint) = &cfg.endpoint {
            builder = builder.with_endpoint(endpoint);
        }
        if let Some(id) = &cfg.access_key_id {
            builder = builder.with_access_key_id(id);
        }
        if let Some(secret) = &cfg.secret_access_key {
            builder = builder.with_secret_access_key(secret);
        }
        let s3 = builder
            .build()
            .map_err(|e| Error::invalid(format!("s3 config: {e}")))?;
        Ok(Self::with_object_store(Arc::new(s3), cfg))
    }

    /// Хранилище поверх готового `ObjectStore` (InMemory в тестах); `cfg.prefix` применяется.
    pub fn with_object_store(store: Arc<dyn ObjectStore>, cfg: S3Config) -> Self {
        let store = if cfg.prefix.is_empty() {
            store
        } else {
            Arc::new(PrefixStore::new(store, cfg.prefix.as_str()))
        };
        Self { store, cfg }
    }

    /// Запись: до порога данные собираются в память и уходят одним PUT, дальше — multipart.
    async fn write(&self, path: &Path, mut data: ByteStream) -> Result<()> {
        let mut head = Vec::new();
        let mut len = 0u64;
        while len < self.cfg.multipart_threshold_bytes {
            let Some(chunk) = data.try_next().await? else {
                let payload: PutPayload = head.into_iter().collect();
                return self
                    .store
                    .put(path, payload)
                    .await
                    .map(drop)
                    .map_err(map_err);
            };
            len += chunk.len() as u64;
            head.push(chunk);
        }
        self.write_multipart(path, stream::iter(head.into_iter().map(Ok)).chain(data))
            .await
    }

    async fn write_multipart(
        &self,
        path: &Path,
        mut data: impl futures::Stream<Item = Result<Bytes>> + Unpin,
    ) -> Result<()> {
        let upload = self.store.put_multipart(path).await.map_err(map_err)?;
        let mut writer =
            WriteMultipart::new_with_chunk_size(upload, self.cfg.multipart_part_bytes.max(1));
        let concurrency = self.cfg.multipart_concurrency.max(1);
        let res: Result<()> = async {
            while let Some(chunk) = data.try_next().await? {
                writer
                    .wait_for_capacity(concurrency)
                    .await
                    .map_err(map_err)?;
                writer.put(chunk);
            }
            Ok(())
        }
        .await;
        match res {
            Ok(()) => writer.finish().await.map(drop).map_err(map_err),
            Err(e) => {
                if let Err(abort) = writer.abort().await {
                    tracing::warn!(%path, error = %abort, "blob: multipart upload not aborted");
                }
                Err(e)
            }
        }
    }

    async fn read(&self, path: &Path, range: Option<Range<u64>>) -> Result<ByteStream> {
        let Some(range) = range else {
            let res = self.store.get(path).await.map_err(map_err)?;
            return Ok(res.into_stream().map_err(map_err).boxed());
        };
        if range.start > range.end {
            return Err(bad_range(&range));
        }
        if range.start < range.end {
            let opts = GetOptions {
                range: Some(GetRange::Bounded(range.clone())),
                ..GetOptions::default()
            };
            match self.store.get_opts(path, opts).await {
                Ok(res) => return Ok(res.into_stream().map_err(map_err).boxed()),
                Err(e @ object_store::Error::NotFound { .. }) => return Err(map_err(e)),
                // Ошибка диапазона неотличима от прочих: проверяется размером объекта.
                Err(e) => {
                    if self.size(path).await? > Some(range.start) {
                        return Err(map_err(e));
                    }
                }
            }
        }
        // Пустой диапазон или начало на конце объекта и дальше.
        match self.size(path).await? {
            None => Err(Error::NotFound),
            Some(size)
                if range.start == size || (range.start == range.end && range.start < size) =>
            {
                Ok(stream::empty().boxed())
            }
            Some(_) => Err(bad_range(&range)),
        }
    }

    async fn size(&self, path: &Path) -> Result<Option<u64>> {
        match self.store.head(path).await {
            Ok(meta) => Ok(Some(meta.size)),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(map_err(e)),
        }
    }

    async fn remove(&self, path: &Path) -> Result<()> {
        match self.store.delete(path).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(map_err(e)),
        }
    }
}

fn bad_range(range: &Range<u64>) -> Error {
    Error::invalid(format!("range {range:?} out of object"))
}

fn raw(path: &str) -> Result<Path> {
    check_raw_path(path)?;
    Path::parse(path).map_err(|e| Error::invalid(e.to_string()))
}

/// Ошибки `object_store` после исчерпания повторов клиента.
fn map_err(e: object_store::Error) -> Error {
    use object_store::Error as E;
    match e {
        E::NotFound { .. } => Error::NotFound,
        E::AlreadyExists { .. } | E::Precondition { .. } => Error::Conflict,
        E::InvalidPath { .. } => Error::invalid(e.to_string()),
        E::NotSupported { .. } | E::NotImplemented { .. } => Error::internal(e.to_string()),
        e => {
            tracing::warn!(error = %e, "blob: s3 request failed");
            Error::UNAVAILABLE
        }
    }
}

#[async_trait]
impl BlobStore for S3Store {
    async fn put(&self, key: &BlobKey, data: Bytes) -> Result<()> {
        self.write(&Path::from(key.path()), stream::iter([Ok(data)]).boxed())
            .await
    }

    async fn put_stream(
        &self,
        key: &BlobKey,
        data: ByteStream,
        _size_hint: Option<u64>,
    ) -> Result<()> {
        self.write(&Path::from(key.path()), data).await
    }

    async fn get(&self, key: &BlobKey, range: Option<Range<u64>>) -> Result<ByteStream> {
        self.read(&Path::from(key.path()), range).await
    }

    async fn head(&self, key: &BlobKey) -> Result<Option<u64>> {
        self.size(&Path::from(key.path())).await
    }

    async fn delete(&self, key: &BlobKey) -> Result<()> {
        self.remove(&Path::from(key.path())).await
    }

    fn list(&self, prefix: &str, older_than: OffsetDateTime) -> ObjectStream {
        let older_than = SystemTime::from(older_than);
        // object_store перечисляет по сегментам пути: запрос — по каталогу префикса, остаток — фильтром.
        let dir = prefix.rfind('/').map(|i| Path::from(&prefix[..i]));
        let prefix = prefix.to_owned();
        self.store
            .list(dir.as_ref())
            .map_err(map_err)
            .try_filter_map(move |meta| {
                let path = meta.location.to_string();
                let modified = SystemTime::from(meta.last_modified);
                future::ready(Ok((path.starts_with(&prefix) && modified < older_than)
                    .then(|| ObjectEntry {
                        path,
                        size: meta.size,
                        modified: modified.into(),
                    })))
            })
            .boxed()
    }

    async fn put_raw(&self, path: &str, data: Bytes) -> Result<()> {
        self.write(&raw(path)?, stream::iter([Ok(data)]).boxed())
            .await
    }

    async fn get_raw(&self, path: &str, range: Option<Range<u64>>) -> Result<ByteStream> {
        self.read(&raw(path)?, range).await
    }

    async fn delete_raw(&self, path: &str) -> Result<()> {
        self.remove(&raw(path)?).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::memory::InMemory;
    use tp_contracts::{BlobHash, Gen, TenantId};

    fn key(n: u8) -> BlobKey {
        BlobKey {
            tenant: TenantId::DEFAULT,
            hash: BlobHash([n; 32]),
            generation: Gen(1),
        }
    }

    /// Порог multipart 10 байт, часть 4 байта.
    fn small() -> S3Config {
        S3Config {
            multipart_threshold_bytes: 10,
            multipart_part_bytes: 4,
            ..S3Config::default()
        }
    }

    fn memory(cfg: S3Config) -> (Arc<InMemory>, S3Store) {
        let mem = Arc::new(InMemory::new());
        let store = S3Store::with_object_store(mem.clone(), cfg);
        (mem, store)
    }

    async fn read_all(stream: ByteStream) -> Vec<u8> {
        let chunks: Vec<Bytes> = stream.try_collect().await.expect("read");
        chunks.concat()
    }

    fn future_time() -> OffsetDateTime {
        OffsetDateTime::now_utc() + time::Duration::hours(1)
    }

    fn chunks(parts: &'static [&'static [u8]]) -> ByteStream {
        stream::iter(parts.iter().map(|p| Ok(Bytes::from_static(p)))).boxed()
    }

    #[tokio::test]
    async fn put_get_small_and_idempotent() {
        let (_, store) = memory(small());
        let k = key(1);
        store
            .put(&k, Bytes::from_static(b"hello"))
            .await
            .expect("put");
        store
            .put(&k, Bytes::from_static(b"hello"))
            .await
            .expect("put again");
        assert_eq!(store.head(&k).await, Ok(Some(5)));
        assert_eq!(
            read_all(store.get(&k, None).await.expect("get")).await,
            b"hello"
        );
    }

    #[tokio::test]
    async fn multipart_stream() {
        let (_, store) = memory(small());
        let k = key(2);
        let data = chunks(&[b"0123", b"456789", b"abcdef", b"g"]);
        store.put_stream(&k, data, None).await.expect("put_stream");
        assert_eq!(store.head(&k).await, Ok(Some(17)));
        assert_eq!(
            read_all(store.get(&k, None).await.expect("get")).await,
            b"0123456789abcdefg"
        );
        let big = Bytes::from(vec![7u8; 33]);
        store
            .put(&key(3), big.clone())
            .await
            .expect("put multipart");
        assert_eq!(
            read_all(store.get(&key(3), None).await.expect("get")).await,
            big
        );
    }

    #[tokio::test]
    async fn interrupted_stream_is_invisible() {
        let (_, store) = memory(small());
        for (n, parts) in [(4u8, 1usize), (5, 4)] {
            let k = key(n);
            let ok = (0..parts).map(|_| Ok(Bytes::from_static(b"0123")));
            let data = stream::iter(ok.chain([Err(Error::UNAVAILABLE)])).boxed();
            assert_eq!(
                store.put_stream(&k, data, None).await,
                Err(Error::UNAVAILABLE)
            );
            assert_eq!(store.head(&k).await, Ok(None), "{parts} parts");
        }
    }

    #[tokio::test]
    async fn ranges() {
        let (_, store) = memory(small());
        let k = key(6);
        store
            .put(&k, Bytes::from_static(b"0123456789"))
            .await
            .expect("put");
        let get = |r| store.get(&k, Some(r));
        assert_eq!(read_all(get(2..7).await.expect("get")).await, b"23456");
        assert_eq!(read_all(get(8..100).await.expect("get")).await, b"89");
        assert_eq!(read_all(get(10..12).await.expect("get")).await, b"");
        assert_eq!(read_all(get(3..3).await.expect("get")).await, b"");
        assert!(matches!(get(11..12).await, Err(Error::Invalid(_))));
        assert!(matches!(
            get(Range { start: 5, end: 4 }).await,
            Err(Error::Invalid(_))
        ));
        assert!(matches!(
            store.get(&key(7), Some(0..1)).await,
            Err(Error::NotFound)
        ));
    }

    #[tokio::test]
    async fn missing_and_delete() {
        let (_, store) = memory(small());
        let k = key(8);
        assert!(matches!(store.get(&k, None).await, Err(Error::NotFound)));
        assert_eq!(store.head(&k).await, Ok(None));
        assert_eq!(store.delete(&k).await, Ok(()));
        store.put(&k, Bytes::from_static(b"x")).await.expect("put");
        store.delete(&k).await.expect("delete");
        assert_eq!(store.head(&k).await, Ok(None));
    }

    #[tokio::test]
    async fn list_and_prefix() {
        let (mem, store) = memory(S3Config {
            prefix: "blob".into(),
            ..small()
        });
        let k = key(9);
        store
            .put(&k, Bytes::from_static(b"abc"))
            .await
            .expect("put");
        store
            .put_raw("idx/t/a/seg1", Bytes::from_static(b"s"))
            .await
            .expect("put_raw");
        assert!(
            mem.head(&Path::from(format!("blob/{}", k.path())))
                .await
                .is_ok()
        );

        let list = |prefix: &str, older_than| {
            store
                .list(prefix, older_than)
                .map_ok(|e| e.path)
                .try_collect::<Vec<_>>()
        };
        let tenant = BlobKey::tenant_prefix(TenantId::DEFAULT);
        assert_eq!(list(&tenant, future_time()).await, Ok(vec![k.path()]));
        assert_eq!(
            list("idx/t/a/se", future_time()).await,
            Ok(vec!["idx/t/a/seg1".into()])
        );
        assert_eq!(list("idx/t/b", future_time()).await, Ok(vec![]));
        let past = OffsetDateTime::now_utc() - time::Duration::hours(1);
        assert_eq!(list("", past).await, Ok(vec![]));
        let mut all = list("", future_time()).await.expect("list");
        all.sort();
        assert_eq!(all, vec![k.path(), "idx/t/a/seg1".to_owned()]);
    }

    #[tokio::test]
    async fn raw_paths_are_checked() {
        let (_, store) = memory(small());
        for bad in ["../x", "/abs", "a//b"] {
            assert!(
                matches!(
                    store.put_raw(bad, Bytes::new()).await,
                    Err(Error::Invalid(_))
                ),
                "{bad}"
            );
        }
        store
            .put_raw("idx/x", Bytes::from_static(b"r"))
            .await
            .expect("put_raw");
        assert_eq!(
            read_all(store.get_raw("idx/x", None).await.expect("get")).await,
            b"r"
        );
        store.delete_raw("idx/x").await.expect("delete_raw");
        assert!(matches!(
            store.get_raw("idx/x", None).await,
            Err(Error::NotFound)
        ));
    }

    fn plaintext() -> InternalTls {
        InternalTls {
            allow_plaintext: true,
            ..InternalTls::default()
        }
    }

    fn endpoint(url: &str, tls: InternalTls) -> S3Config {
        S3Config {
            endpoint: Some(url.into()),
            tls,
            ..S3Config::default()
        }
    }

    fn invalid(res: Result<S3Store>) -> String {
        match res {
            Err(Error::Invalid(m)) => m,
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn http_outside_dev_is_config_error() {
        let http = "http://seaweedfs:8333";
        // Без allow_plaintext — ошибка в любом профиле, с подсказкой https://.
        for profile in [Profile::Prod, Profile::Dev] {
            let cfg = endpoint(http, InternalTls::default());
            assert!(matches!(cfg.check(profile), Err(Error::Invalid(_))));
            assert!(invalid(S3Store::new(cfg, profile)).contains("https://"));
        }
        // allow_plaintext вне dev — ошибка даже при https://.
        for url in [http, "https://s3.internal"] {
            let cfg = endpoint(url, plaintext());
            assert!(invalid(S3Store::new(cfg, Profile::Prod)).contains("profile: dev"));
        }
        let no_endpoint = S3Config {
            tls: plaintext(),
            ..S3Config::default()
        };
        assert!(matches!(
            no_endpoint.check(Profile::Prod),
            Err(Error::Invalid(_))
        ));
        // Dev с allow_plaintext — открытый канал допустим.
        let dev = endpoint(http, plaintext());
        assert_eq!(dev.check(Profile::Dev), Ok(Transport::Plaintext));
        assert!(S3Store::new(dev, Profile::Dev).is_ok());
        assert!(matches!(
            endpoint("ftp://s3", InternalTls::default()).check(Profile::Prod),
            Err(Error::Invalid(_))
        ));
    }

    #[test]
    fn https_with_ca_from_section() {
        let https = "https://s3.internal:8333";
        assert_eq!(
            endpoint(https, InternalTls::default()).check(Profile::Prod),
            Ok(Transport::Tls)
        );
        assert_eq!(S3Config::default().check(Profile::Prod), Ok(Transport::Tls));
        assert!(S3Store::new(endpoint(https, InternalTls::default()), Profile::Prod).is_ok());

        let ca = |path: &FsPath| InternalTls {
            ca_file: Some(path.to_path_buf()),
            ..InternalTls::default()
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let good = dir.path().join("ca.pem");
        std::fs::write(&good, include_bytes!("testdata/ca.pem")).expect("write ca");
        assert!(S3Store::new(endpoint(https, ca(&good)), Profile::Prod).is_ok());

        let empty = dir.path().join("empty.pem");
        std::fs::write(&empty, b"not a certificate").expect("write");
        assert!(
            invalid(S3Store::new(endpoint(https, ca(&empty)), Profile::Prod)).contains("ca_file")
        );
        let missing = dir.path().join("missing.pem");
        assert!(
            invalid(S3Store::new(endpoint(https, ca(&missing)), Profile::Prod)).contains("ca_file")
        );
    }

    #[test]
    fn unsupported_tls_fields() {
        let https = "https://s3.internal:8333";
        let mtls = InternalTls {
            cert_file: Some("/etc/thepost/node.pem".into()),
            key_file: Some("/etc/thepost/node.key".into()),
            ..InternalTls::default()
        };
        assert!(invalid(S3Store::new(endpoint(https, mtls), Profile::Prod)).contains("mTLS"));

        let name = |n: &str| InternalTls {
            server_name: Some(n.into()),
            ..InternalTls::default()
        };
        assert_eq!(
            endpoint(https, name("S3.internal")).check(Profile::Prod),
            Ok(Transport::Tls)
        );
        assert!(
            endpoint(https, name("other.internal"))
                .check(Profile::Prod)
                .is_err()
        );
        assert!(
            S3Config {
                tls: name("s3.amazonaws.com"),
                ..S3Config::default()
            }
            .check(Profile::Prod)
            .is_err()
        );
    }

    #[test]
    fn hosts() {
        assert_eq!(url_host("https://s3.internal"), Some("s3.internal"));
        assert_eq!(url_host("https://k:s@s3:8333/b?x"), Some("s3"));
        assert_eq!(url_host("https://[::1]:8333"), Some("::1"));
        assert_eq!(url_host("https:///x"), None);
        assert_eq!(url_host("s3.internal"), None);
    }

    #[test]
    fn debug_hides_secret() {
        let cfg = S3Config {
            secret_access_key: Some("top-secret".into()),
            ..S3Config::default()
        };
        assert!(!format!("{cfg:?}").contains("top-secret"));
    }

    /// Интеграционный: SeaweedFS из compose (`TEST_S3_URL`, ключи — в окружении, `deploy/README.md`);
    /// бакет `TEST_S3_BUCKET` (по умолчанию `thepost-test`) должен существовать.
    #[tokio::test]
    async fn seaweedfs() {
        let Ok(endpoint) = std::env::var("TEST_S3_URL") else {
            return;
        };
        // Стенд dev: SeaweedFS может слушать http:// — допустимо только в профиле dev.
        let store = S3Store::new(
            S3Config {
                endpoint: Some(endpoint),
                bucket: std::env::var("TEST_S3_BUCKET").unwrap_or_else(|_| "thepost-test".into()),
                prefix: format!("blob-{}", std::process::id()),
                tls: plaintext(),
                multipart_threshold_bytes: 6 << 20,
                multipart_part_bytes: 5 << 20,
                ..S3Config::default()
            },
            Profile::Dev,
        )
        .expect("s3 client");

        let k = key(10);
        let big = Bytes::from(vec![42u8; 11 << 20]);
        store.put(&k, big.clone()).await.expect("put multipart");
        assert_eq!(store.head(&k).await, Ok(Some(big.len() as u64)));
        assert_eq!(
            read_all(store.get(&k, Some(1..4)).await.expect("range")).await,
            vec![42u8; 3]
        );
        let listed: Vec<ObjectEntry> = store
            .list("", future_time())
            .try_collect()
            .await
            .expect("list");
        assert!(listed.iter().any(|e| e.key() == Some(k)));
        store.delete(&k).await.expect("delete");
        assert_eq!(store.head(&k).await, Ok(None));
    }
}
