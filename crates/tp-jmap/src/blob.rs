//! Бинарные данные (RFC 8620 §6): загрузка (`uploadUrl`), скачивание (`downloadUrl`), `Blob/copy`.
//!
//! Идентификаторы blob:
//! - `M<письмо>` — исходное письмо, `M<письмо>_<путь части через _>` — тело части, декодированное из кодировки передачи;
//! - `U<хэш>_<поколение>` — загруженный файл: объект BLOB организации (протокол SRS 3.4). Ссылка, взятая на время
//!   загрузки, сразу снимается: объект без ссылок живёт до сборки мусора по окну и учитывается заново, когда
//!   попадает в письмо (`Email/import`, вложение черновика).

use std::str::FromStr;
use std::sync::Arc;

use axum::body::{Body, Bytes, to_bytes};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures::{StreamExt, TryStreamExt, stream};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tp_contracts::blob::{Acquire, ByteStream, Commit, Compression};
use tp_contracts::meta::{AccountScope, BodyPart};
use tp_contracts::{BlobHash, BlobKey, Error, Gen, MessageId, Result, TenantId};

use crate::core::error::{MethodError, MethodResult};
use crate::core::http::{auth_error, json as json_response, problem};
use crate::core::request::{object, parse_args};
use crate::core::{Ctx, Jmap, Problem};
use crate::email_get::{is_multipart, leaf_path};

/// Источник данных blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BlobRef {
    /// Исходное письмо.
    Message(MessageId),
    /// Часть письма по пути IMAP (`1.2`).
    Part(MessageId, String),
    /// Загруженный файл.
    Upload(BlobKey),
}

/// Идентификатор blob загруженного файла.
pub(crate) fn upload_id(key: &BlobKey) -> String {
    format!("U{}_{}", key.hash, key.generation.0)
}

/// Разбор идентификатора blob; чужой формат — `None`.
pub(crate) fn parse_id(tenant: TenantId, id: &str) -> Option<BlobRef> {
    if let Some(rest) = id.strip_prefix('M') {
        return Some(match rest.split_once('_') {
            Some((m, path)) => BlobRef::Part(MessageId::from_str(m).ok()?, path.replace('_', ".")),
            None => BlobRef::Message(MessageId::from_str(rest).ok()?),
        });
    }
    let (hash, generation) = id.strip_prefix('U')?.split_once('_')?;
    Some(BlobRef::Upload(BlobKey {
        tenant,
        hash: BlobHash::from_str(hash).ok()?,
        generation: Gen(generation.parse().ok()?),
    }))
}

/// Листовая часть письма по пути.
fn find_part<'a>(bp: &'a BodyPart, path: &str) -> Option<&'a BodyPart> {
    if !is_multipart(bp) && leaf_path(bp) == path {
        return Some(bp);
    }
    bp.children.iter().find_map(|c| find_part(c, path))
}

/// Тело части без кодировки передачи (`base64`, `quoted-printable`); ошибка декодирования — как есть.
fn decode_transfer(bp: &BodyPart, body: Vec<u8>) -> Vec<u8> {
    use mail_parser::decoders::base64::base64_decode;
    use mail_parser::decoders::quoted_printable::quoted_printable_decode;
    let decoded = match bp
        .encoding
        .as_deref()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("base64") => base64_decode(&body),
        Some("quoted-printable") => quoted_printable_decode(&body),
        _ => None,
    };
    decoded.unwrap_or(body)
}

/// Читает поток целиком; больше `max` байт — `Error::Invalid`.
async fn collect_max(s: ByteStream, max: u64) -> Result<Vec<u8>> {
    let mut s = s;
    let mut out = Vec::new();
    while let Some(chunk) = s.try_next().await? {
        if (out.len() + chunk.len()) as u64 > max {
            return Err(Error::invalid("blob too large"));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

impl Jmap {
    /// Содержимое blob учётной записи потоком; нет blob — `None`.
    pub(crate) async fn blob_stream(
        &self,
        scope: AccountScope,
        id: &str,
    ) -> Result<Option<ByteStream>> {
        let d = &self.d;
        match parse_id(scope.tenant, id) {
            None => Ok(None),
            Some(BlobRef::Message(m)) => match d.messages.get(scope, m).await? {
                Some(_) => Ok(Some(d.store.fetch_raw(scope, m, None).await?)),
                None => Ok(None),
            },
            Some(BlobRef::Part(m, path)) => {
                let Some(msg) = d.messages.get(scope, m).await? else {
                    return Ok(None);
                };
                let Some(bp) = find_part(&msg.meta.body, &path) else {
                    return Ok(None);
                };
                let raw = collect_max(d.store.fetch_part(scope, m, &path).await?, u64::MAX).await?;
                let data = Bytes::from(decode_transfer(bp, raw));
                Ok(Some(stream::once(async move { Ok(data) }).boxed()))
            }
            Some(BlobRef::Upload(key)) => match d.blob_index.compression(&key).await? {
                None => Ok(None),
                Some(Compression::None) => Ok(Some(d.blobs.get(&key, None).await?)),
                // Загрузка пишет без сжатия; сжатым может быть только совпавший объект письма (CTR-21).
                Some(c) => Err(Error::Internal(format!("сжатый объект загрузки ({c})"))),
            },
        }
    }

    /// Содержимое blob целиком, не больше `max` байт (больше — `Error::Invalid`); нет blob — `None`.
    pub(crate) async fn read_blob(
        &self,
        scope: AccountScope,
        id: &str,
        max: u64,
    ) -> Result<Option<Vec<u8>>> {
        match self.blob_stream(scope, id).await? {
            Some(s) => collect_max(s, max).await.map(Some),
            None => Ok(None),
        }
    }

    /// Есть ли blob у учётной записи.
    async fn blob_exists(&self, scope: AccountScope, id: &str) -> Result<bool> {
        let d = &self.d;
        Ok(match parse_id(scope.tenant, id) {
            None => false,
            Some(BlobRef::Message(m)) => d.messages.get(scope, m).await?.is_some(),
            Some(BlobRef::Part(m, path)) => d
                .messages
                .get(scope, m)
                .await?
                .is_some_and(|msg| find_part(&msg.meta.body, &path).is_some()),
            Some(BlobRef::Upload(key)) => d.blob_index.exists(&key).await?,
        })
    }

    /// Сохраняет загруженный файл как объект BLOB организации без ссылок (протокол SRS 3.4).
    pub(crate) async fn put_upload(&self, tenant: TenantId, data: Bytes) -> Result<BlobKey> {
        let d = &self.d;
        let hash = BlobHash(Sha256::digest(&data).into());
        let generation = match d.blob_index.acquire(tenant, &hash, 1).await? {
            Acquire::Existing { generation } => generation,
            Acquire::Absent { generation } => {
                let mut generation = generation;
                loop {
                    let key = BlobKey {
                        tenant,
                        hash,
                        generation,
                    };
                    d.blobs.put(&key, data.clone()).await?;
                    match d
                        .blob_index
                        .commit_new(tenant, &hash, generation, data.len() as u64, 1)
                        .await?
                    {
                        Commit::Done { generation } => break generation,
                        Commit::Bumped { generation: next } => generation = next,
                    }
                }
            }
        };
        // Ссылка загрузки снимается: объект ждёт сборки мусора по окну или учёта в письме.
        d.blob_index.release(tenant, &hash, generation, 1).await?;
        Ok(BlobKey {
            tenant,
            hash,
            generation,
        })
    }
}

fn http_error(e: &Error) -> Response {
    if e.is_retryable() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    tracing::error!(error = %e, "jmap: ошибка blob");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

/// `POST uploadUrl` (RFC 8620 §6.1): тело — файл, ответ `201` с `blobId`, `type`, `size`.
pub(crate) async fn upload(
    State(j): State<Arc<Jmap>>,
    Path(account): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let p = match j.authenticate(&headers).await {
        Ok(p) => p,
        Err(e) => return auth_error(&e),
    };
    if account != p.account.to_string() {
        return problem(&Problem::http(404, "account not found"));
    }
    let Some(_slot) = j.enter_upload(&p) else {
        return problem(&Problem::limit("maxConcurrentUpload"));
    };
    let limit = usize::try_from(j.cfg.max_size_upload).unwrap_or(usize::MAX);
    let Ok(data) = to_bytes(body, limit).await else {
        return problem(&Problem {
            status: 413,
            ..Problem::limit("maxSizeUpload")
        });
    };
    let kind = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_owned();
    let size = data.len();
    match j.put_upload(p.tenant, data).await {
        Ok(key) => json_response(
            StatusCode::CREATED,
            "application/json",
            &json!({
                "accountId": account,
                "blobId": upload_id(&key),
                "type": kind,
                "size": size,
            }),
        ),
        Err(e) => http_error(&e),
    }
}

/// Параметры `downloadUrl`.
#[derive(Debug, Deserialize)]
pub(crate) struct DownloadQuery {
    accept: Option<String>,
}

/// Тип содержимого `тип/подтип` из безопасных символов; иное — `application/octet-stream`.
fn content_type(accept: Option<&str>) -> String {
    let token = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$&-^_.+".contains(&b))
    };
    match accept.and_then(|a| a.split_once('/')) {
        Some((t, s)) if token(t) && token(s) => format!("{t}/{s}"),
        _ => "application/octet-stream".into(),
    }
}

/// `filename*` по RFC 8187: UTF-8 с процентным кодированием.
fn disposition(name: &str) -> String {
    let mut out = String::from("attachment; filename*=UTF-8''");
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `GET downloadUrl` (RFC 8620 §6.2): содержимое blob, тип — из `accept`, имя — `Content-Disposition`.
pub(crate) async fn download(
    State(j): State<Arc<Jmap>>,
    Path((account, blob, name)): Path<(String, String, String)>,
    Query(q): Query<DownloadQuery>,
    headers: HeaderMap,
) -> Response {
    let p = match j.authenticate(&headers).await {
        Ok(p) => p,
        Err(e) => return auth_error(&e),
    };
    if account != p.account.to_string() {
        return problem(&Problem::http(404, "account not found"));
    }
    let scope = AccountScope {
        tenant: p.tenant,
        account: p.account,
    };
    let data = match j.blob_stream(scope, &blob).await {
        Ok(Some(s)) => s,
        Ok(None) => return problem(&Problem::http(404, "blob not found")),
        Err(e) => return http_error(&e),
    };
    let mut resp = Body::from_stream(data).into_response();
    let h = resp.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&content_type(q.accept.as_deref())) {
        h.insert(header::CONTENT_TYPE, v);
    }
    if let Ok(v) = HeaderValue::from_str(&disposition(&name)) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, immutable, max-age=31536000"),
    );
    resp
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CopyArgs {
    from_account_id: String,
    account_id: String,
    blob_ids: Vec<String>,
}

/// `Blob/copy` (RFC 8620 §6.3): доступна только своя учётная запись, поэтому копирование —
/// проверка, что blob есть (идентификатор не меняется).
pub(crate) async fn copy(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: CopyArgs = parse_args(args)?;
    if a.from_account_id != ctx.account_id() {
        return Err(MethodError::new("fromAccountNotFound"));
    }
    ctx.check_account(&a.account_id)?;
    if a.blob_ids.len() > ctx.jmap.cfg.max_objects_in_set as usize {
        return Err(MethodError::new("requestTooLarge"));
    }
    let (mut copied, mut not_copied) = (Map::new(), Map::new());
    for id in a.blob_ids {
        if ctx.jmap.blob_exists(ctx.scope, &id).await? {
            copied.insert(id.clone(), json!(id));
        } else {
            not_copied.insert(id, json!({"type": "notFound"}));
        }
    }
    let or_null = |m: Map<String, Value>| {
        if m.is_empty() {
            Value::Null
        } else {
            Value::Object(m)
        }
    };
    Ok(object(json!({
        "fromAccountId": a.from_account_id,
        "accountId": a.account_id,
        "copied": or_null(copied),
        "notCopied": or_null(not_copied),
    })))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use tp_contracts::meta::{Flags, SpecialUse};

    use crate::JmapConfig;
    use crate::testenv::{Env, http10, serve};

    const AUTH: &str = "Authorization: Bearer token-1\r\n";

    fn upload_req(account: &str, kind: &str, body: &[u8]) -> Vec<u8> {
        let mut r = format!(
            "POST /jmap/upload/{account}/ HTTP/1.0\r\nHost: x\r\n{AUTH}Content-Type: {kind}\r\n\
             Content-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        r.extend_from_slice(body);
        r
    }

    fn get_req(path: &str) -> Vec<u8> {
        format!("GET {path} HTTP/1.0\r\nHost: x\r\n{AUTH}\r\n").into_bytes()
    }

    /// Загрузка и скачивание: файл, письмо целиком, часть с base64 (декодируется), лимиты, чужая учётная запись.
    #[tokio::test]
    async fn upload_and_download() {
        let cfg = JmapConfig {
            max_size_upload: 1_000,
            ..JmapConfig::default()
        };
        let env = Env::with(cfg, Vec::new()).await;
        let addr = serve(env.jmap.clone()).await;
        let acc = env.account();

        let (code, _, body) = http10(addr, &upload_req(&acc, "text/plain", b"hello world")).await;
        assert_eq!(code, 201, "{}", String::from_utf8_lossy(&body));
        let up: Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(up["size"], 11);
        assert_eq!(up["type"], "text/plain");
        assert_eq!(up["accountId"], acc);
        let blob = up["blobId"].as_str().expect("blobId");
        let (code, head, body) = http10(
            addr,
            &get_req(&format!(
                "/jmap/download/{acc}/{blob}/%D0%BE%D1%82%D1%87%D1%91%D1%82.txt?accept=text/plain"
            )),
        )
        .await;
        assert_eq!(code, 200);
        assert_eq!(body, b"hello world");
        let head = head.to_lowercase();
        assert!(head.contains("content-type: text/plain"), "{head}");
        assert!(
            head.contains("filename*=utf-8''%d0%be%d1%82%d1%87%d1%91%d1%82.txt"),
            "{head}"
        );

        let raw = b"From: a@x.test\r\nSubject: s\r\nMIME-Version: 1.0\r\n\
            Content-Type: multipart/mixed; boundary=b\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\ntext\r\n\
            --b\r\nContent-Type: application/octet-stream\r\nContent-Transfer-Encoding: base64\r\n\
            Content-Disposition: attachment; filename=a.bin\r\n\r\nAAEC/w==\r\n--b--\r\n";
        let id = env
            .deliver(env.mailbox(SpecialUse::Inbox), raw, Flags::new())
            .await;
        let (code, _, body) =
            http10(addr, &get_req(&format!("/jmap/download/{acc}/M{id}/m.eml"))).await;
        assert_eq!(code, 200);
        assert_eq!(body, raw.to_vec());
        let e = env
            .call(
                "Email/get",
                json!({"accountId": acc, "ids": [id.to_string()], "properties": ["attachments"]}),
                "Email/get",
            )
            .await;
        let part = e["list"][0]["attachments"][0]["blobId"]
            .as_str()
            .expect("part blob");
        let (code, head, body) = http10(
            addr,
            &get_req(&format!("/jmap/download/{acc}/{part}/a.bin")),
        )
        .await;
        assert_eq!(code, 200);
        assert_eq!(body, vec![0, 1, 2, 255]);
        assert!(head.to_lowercase().contains("application/octet-stream"));

        let (code, _, _) = http10(addr, &get_req(&format!("/jmap/download/{acc}/Mnope/x"))).await;
        assert_eq!(code, 404);
        let (code, _, _) = http10(addr, &get_req(&format!("/jmap/download/other/{blob}/x"))).await;
        assert_eq!(code, 404);
        let (code, _, body) = http10(addr, &upload_req(&acc, "x/y", &[b'a'; 1_001])).await;
        assert_eq!(code, 413);
        let p: Value = serde_json::from_slice(&body).expect("problem");
        assert_eq!(p["limit"], "maxSizeUpload");
        let (code, _, _) = http10(
            addr,
            b"POST /jmap/upload/x/ HTTP/1.0\r\nHost: x\r\nContent-Length: 1\r\n\r\na",
        )
        .await;
        assert_eq!(code, 401);
    }

    /// Blob/copy: своя учётная запись — тот же id; нет blob — notCopied; чужая — fromAccountNotFound.
    #[tokio::test]
    async fn blob_copy() {
        let env = Env::new().await;
        let acc = env.account();
        let key = env
            .jmap
            .put_upload(env.scope.tenant, axum::body::Bytes::from_static(b"x"))
            .await
            .expect("upload");
        let blob = super::upload_id(&key);
        let r = env
            .call(
                "Blob/copy",
                json!({"fromAccountId": acc, "accountId": acc, "blobIds": [&blob, "Mnope"]}),
                "Blob/copy",
            )
            .await;
        assert_eq!(r["copied"], json!({&blob: &blob}));
        assert_eq!(r["notCopied"]["Mnope"]["type"], "notFound");
        let r = env
            .call(
                "Blob/copy",
                json!({"fromAccountId": "x", "accountId": acc, "blobIds": []}),
                "error",
            )
            .await;
        assert_eq!(r["type"], "fromAccountNotFound");
    }
}
