//! Приём письма (CONTRACTS.md#MailStore `ingest`; SRS 3.3, 3.4).
//!
//! 1. Письмо читается из потока, точки принятия решений (`DecisionHook::on_delivery`) разрешают,
//!    запрещают или дополняют доставку (заголовки, копии в журнальные ящики) — до загрузки;
//!    отправитель для них — с конверта (`IngestRequest::mail_from`), внешние копии возвращаются в `forward`.
//!    Затем квота всех доставок, включая копии ([`crate::quota`]).
//! 2. Разбор на скелет и части ([`Split`]) и метаданные для протоколов ([`meta`]); заголовки получателя —
//!    в его скелете, части общие.
//! 3. Каждый объект учитывается одним `BlobIndex::acquire` на всех получателей (`refs` = число ссылок)
//!    и загружается, только если его ещё нет; всё — вне транзакций БД (протокол SRS 3.4).
//! 4. Метаданные всех доставок, включая копии, — одной транзакцией `MessageStore::insert_batch` (события доставки
//!    пишет хранилище метаданных в ней же; решение 2026-10-31-atomic-delivery); затем `MailboxChanged` в шину.
//!
//! До шага 4 ничего не раскладывается. Сбой загрузки или транзакции снимает добавленные ссылки и не оставляет
//! метаданных: запрос выполняется целиком или не выполняется (вызывающий может повторить).
//!
//! Письмо клиента (`MailStore::append`: IMAP APPEND, JMAP `Email/import`; решение 2026-11-01-trash-purge) — не доставка:
//! шаги 2–3 общие с приёмом, но без точек принятия решений и копий (шаг 1 — только квота), метаданные —
//! `MessageStore::insert`.

use std::collections::HashMap;

use bytes::{Bytes, BytesMut};
use futures::TryStreamExt;
use time::OffsetDateTime;
use tp_contracts::blob::ByteStream;
use tp_contracts::ext::{CopyTo, Decision, DeliveryInfo};
use tp_contracts::mailstore::{Delivered, Delivery, IngestRequest, Ingested};
use tp_contracts::meta::{AccountScope, Flags, NewMessage, PartRef};
use tp_contracts::{BlobHash, BlobKey, Error, MailboxId, Result, TenantId};

use crate::split::{self, Split};
use crate::{Store, meta};

/// Объект-скелет одной группы доставок с одинаковыми заголовками получателя.
struct SkeletonGroup {
    data: Bytes,
    hash: BlobHash,
    /// Индексы доставок группы.
    members: Vec<usize>,
    key: Option<BlobKey>,
}

/// Письмо, подготовленное к раскладке: объекты загружены и учтены, метаданные ещё не вставлены.
struct Staged {
    /// Метаданные доставок в их порядке.
    items: Vec<(AccountScope, MailboxId, NewMessage)>,
    /// Добавленные ссылки на объекты: снимаются, если вставка метаданных не удалась.
    held: Vec<(BlobKey, u32)>,
}

impl Store {
    pub(crate) async fn ingest_impl(&self, req: IngestRequest) -> Result<Ingested> {
        let IngestRequest {
            tenant,
            mail_from,
            raw,
            mut deliveries,
            received_at,
        } = req;
        if deliveries.is_empty() {
            return Err(Error::invalid("ingest: нет доставок"));
        }
        let raw = read_all(raw).await?;

        let info = DeliveryInfo {
            tenant,
            mail_from,
            deliveries: deliveries.clone(),
            size: raw.len() as u64,
        };
        let mut common = Vec::new();
        let mut forward = Vec::new();
        match self.d.hooks.on_delivery(&info).await? {
            Decision::Allow => {}
            Decision::Deny(reason) => {
                tracing::info!(%reason, "mailstore: доставка запрещена точкой принятия решений");
                return Err(Error::Forbidden);
            }
            Decision::Augment(a) => {
                common = a.headers;
                for copy in a.copies {
                    match copy {
                        CopyTo::Mailbox(d) => deliveries.push(d),
                        // Внешние копии ставит в очередь smtp-out вызывающий.
                        CopyTo::Address(a) => forward.push(a),
                    }
                }
            }
        }

        let Staged { items, held } = self
            .stage(tenant, raw, &deliveries, &common, received_at)
            .await?;
        let inserted = match self.d.messages.insert_batch(tenant, items).await {
            Ok(inserted) => inserted,
            Err(e) => {
                self.release(&held).await;
                return Err(e);
            }
        };

        let mut done: Vec<Delivered> = Vec::with_capacity(deliveries.len());
        for (d, ins) in deliveries.iter().zip(inserted) {
            let scope = AccountScope {
                tenant,
                account: d.account,
            };
            self.notify(scope, [d.mailbox], ins.modseq).await;
            done.push(Delivered {
                account: d.account,
                mailbox: d.mailbox,
                message: ins.id,
                uid: ins.uid,
            });
        }
        // Доставки сверх запроса (копии точек принятия решений) вызывающему не возвращаются.
        done.truncate(info.deliveries.len());
        Ok(Ingested {
            delivered: done,
            forward,
        })
    }

    /// Письмо клиента (IMAP APPEND, JMAP `Email/import`): те же шаги 1–3, что у приёма, без точек принятия решений,
    /// копий и добавленных заголовков; метаданные — `MessageStore::insert`.
    pub(crate) async fn append_impl(
        &self,
        scope: AccountScope,
        mailbox: MailboxId,
        raw: ByteStream,
        flags: Flags,
        received_at: OffsetDateTime,
    ) -> Result<Delivered> {
        let raw = read_all(raw).await?;
        let delivery = Delivery {
            account: scope.account,
            mailbox,
            extra_headers: Vec::new(),
            flags,
        };
        let Staged { mut items, held } = self
            .stage(scope.tenant, raw, &[delivery], &[], received_at)
            .await?;
        let inserted = match items.pop() {
            Some((_, _, msg)) => self.d.messages.insert(scope, mailbox, msg).await,
            None => Err(Error::Conflict),
        };
        let ins = match inserted {
            Ok(ins) => ins,
            Err(e) => {
                self.release(&held).await;
                return Err(e);
            }
        };
        self.notify(scope, [mailbox], ins.modseq).await;
        Ok(Delivered {
            account: scope.account,
            mailbox,
            message: ins.id,
            uid: ins.uid,
        })
    }

    /// Шаги 1–3 для доставок `deliveries` с общими заголовками `common`: проверка заголовков, квота (до разбора
    /// и загрузки), разбор, загрузка и учёт объектов. Метаданные не раскладываются; сбой загрузки снимает ссылки.
    async fn stage(
        &self,
        tenant: TenantId,
        raw: Bytes,
        deliveries: &[Delivery],
        common: &[(String, String)],
        received_at: OffsetDateTime,
    ) -> Result<Staged> {
        check_headers(common)?;
        for d in deliveries {
            check_headers(&d.extra_headers)?;
        }
        // Квота — до разбора и загрузки: превышение у одного получателя отклоняет весь запрос.
        let demands: Vec<_> = deliveries
            .iter()
            .map(|d| {
                let size = header_block(&d.extra_headers, common).len() + raw.len();
                (d.account, size as u64)
            })
            .collect();
        self.check_quota(tenant, &demands).await?;

        // Разбор — работа процессора на всё письмо: вне потоков асинхронного рантайма.
        let (split, meta) = tokio::task::spawn_blocking(move || {
            let meta = meta::message_meta(&raw);
            (Split::new(raw), meta)
        })
        .await
        .map_err(|e| Error::internal(format!("ingest: разбор письма: {e}")))?;

        let mut groups: Vec<SkeletonGroup> = Vec::new();
        for (i, d) in deliveries.iter().enumerate() {
            let prefix = header_block(&d.extra_headers, common);
            match groups.iter_mut().find(|g| {
                g.members
                    .first()
                    .is_some_and(|&m| same_prefix(&deliveries[m], d))
            }) {
                Some(g) => g.members.push(i),
                None => {
                    let data = split.skeleton(&prefix);
                    groups.push(SkeletonGroup {
                        hash: split::hash(&data),
                        data,
                        members: vec![i],
                        key: None,
                    });
                }
            }
        }

        // Части: одна загрузка на всё письмо и всех получателей.
        let n = u32::try_from(deliveries.len())
            .map_err(|_| Error::invalid("ingest: много доставок"))?;
        let mut occurrences: Vec<(BlobHash, Bytes, u32)> = Vec::new();
        for cut in split.cuts() {
            match occurrences.iter_mut().find(|(h, _, _)| *h == cut.hash) {
                Some((_, _, k)) => *k += 1,
                None => occurrences.push((cut.hash, split.body(cut), 1)),
            }
        }
        let mut held: Vec<(BlobKey, u32)> = Vec::new();
        let mut part_keys: HashMap<BlobHash, BlobKey> = HashMap::new();
        let uploaded = async {
            for (hash, body, k) in &occurrences {
                let refs = k
                    .checked_mul(n)
                    .ok_or_else(|| Error::invalid("ingest: слишком много ссылок"))?;
                let key = self.store_object(tenant, *hash, body.clone(), refs).await?;
                held.push((key, refs));
                part_keys.insert(*hash, key);
            }
            for g in &mut groups {
                let refs = len_u32(g.members.len());
                let key = self
                    .store_object(tenant, g.hash, g.data.clone(), refs)
                    .await?;
                held.push((key, refs));
                g.key = Some(key);
            }
            Ok::<(), Error>(())
        }
        .await;
        if let Err(e) = uploaded {
            self.release(&held).await;
            return Err(e);
        }

        let items = (|| {
            let mut parts = Vec::with_capacity(split.cuts().len());
            for cut in split.cuts() {
                parts.push(PartRef {
                    part: cut.path.clone(),
                    blob: *part_keys.get(&cut.hash).ok_or(Error::Conflict)?,
                    size: cut.range.end - cut.range.start,
                });
            }
            let mut skeleton_of = vec![None; deliveries.len()];
            for g in &groups {
                for &m in &g.members {
                    skeleton_of[m] = g.key;
                }
            }
            let mut items = Vec::with_capacity(deliveries.len());
            for (d, skeleton) in deliveries.iter().zip(skeleton_of) {
                let scope = AccountScope {
                    tenant,
                    account: d.account,
                };
                let msg = NewMessage {
                    skeleton: skeleton.ok_or(Error::Conflict)?,
                    parts: parts.clone(),
                    size: message_size(&split, &d.extra_headers, common),
                    received_at,
                    flags: d.flags.clone(),
                    meta: meta.clone(),
                };
                items.push((scope, d.mailbox, msg));
            }
            Ok::<_, Error>(items)
        })();
        match items {
            Ok(items) => Ok(Staged { items, held }),
            Err(e) => {
                self.release(&held).await;
                Err(e)
            }
        }
    }

    /// Учитывает объект с `refs` ссылками и загружает его, если его ещё нет (SRS 3.4, шаги 2–3), через
    /// [`tp_blob::BlobWriter`]: хэш `hash` и размер — исходного содержимого, хранится сжатый вид, если сжатие
    /// выгодно (BLOB-13). Тип части не передаётся: тела в кодировке передачи (base64), формат распознаётся по
    /// сигнатуре, порог выигрыша решает остальное.
    async fn store_object(
        &self,
        tenant: TenantId,
        hash: BlobHash,
        data: Bytes,
        refs: u32,
    ) -> Result<BlobKey> {
        self.writer.write_part(tenant, hash, data, refs, None).await
    }

    /// Снимает ссылки (сбой приёма). Ошибка снятия — в журнал: объект останется до ручной сверки.
    async fn release(&self, held: &[(BlobKey, u32)]) {
        for (key, refs) in held {
            if *refs == 0 {
                continue;
            }
            if let Err(e) = self
                .d
                .index
                .release(key.tenant, &key.hash, key.generation, *refs)
                .await
            {
                tracing::error!(error = %e, blob = %key.hash, "mailstore: ссылки не сняты при сбое приёма");
            }
        }
    }
}

/// Читает поток письма целиком (`mail-parser` разбирает буфер).
async fn read_all(stream: ByteStream) -> Result<Bytes> {
    let buf = stream
        .try_fold(BytesMut::new(), |mut buf, chunk| async move {
            buf.extend_from_slice(&chunk);
            Ok(buf)
        })
        .await?;
    Ok(buf.freeze())
}

/// Имя заголовка — печатные ASCII без `:`; значение — без переводов строки, кроме свёртки (CRLF + пробел).
fn check_headers(headers: &[(String, String)]) -> Result<()> {
    for (name, value) in headers {
        let bad_name = name.is_empty() || !name.bytes().all(|b| b.is_ascii_graphic() && b != b':');
        let bytes = value.as_bytes();
        let bad_value = bytes.iter().enumerate().any(|(i, &b)| match b {
            b'\r' => bytes.get(i + 1) != Some(&b'\n'),
            b'\n' => {
                i == 0 || bytes[i - 1] != b'\r' || !matches!(bytes.get(i + 1), Some(b' ' | b'\t'))
            }
            0 => true,
            _ => false,
        });
        if bad_name || bad_value {
            return Err(Error::invalid(format!(
                "ingest: недопустимый заголовок {name:?}"
            )));
        }
    }
    Ok(())
}

/// Заголовки получателя, затем общие дополнения, — перед письмом.
fn header_block(extra: &[(String, String)], common: &[(String, String)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, value) in extra.iter().chain(common) {
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out
}

fn same_prefix(a: &Delivery, b: &Delivery) -> bool {
    a.extra_headers == b.extra_headers
}

fn message_size(split: &Split, extra: &[(String, String)], common: &[(String, String)]) -> u64 {
    (header_block(extra, common).len() + split.raw().len()) as u64
}

fn len_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tp_contracts::MessageId;
    use tp_contracts::ext::{Augment, DecisionHook, DecisionHooks};
    use tp_contracts::mailstore::MailStore;
    use tp_contracts::meta::{Flag, MessageQuery, MessageStore, SpecialUse};
    use tp_testkit::Clock;
    use tp_testkit::{BlobOp, SystemClock};

    use crate::testenv::{Env, object, stream};

    fn mail() -> Vec<u8> {
        tp_testkit::corpus::corpus_mail("mixed-three-attachments")
            .expect("mail")
            .raw
    }

    async fn recipients(env: &Env, n: usize) -> Vec<Delivery> {
        let mut out = Vec::new();
        for i in 0..n {
            let scope = env
                .meta
                .account(&format!("u{i}@example.org"))
                .await
                .expect("acc");
            out.push(Delivery {
                account: scope.account,
                mailbox: env
                    .meta
                    .mailbox_by_role(scope, SpecialUse::Inbox)
                    .expect("inbox"),
                extra_headers: vec![],
                flags: Default::default(),
            });
        }
        out
    }

    fn request(raw: &[u8], deliveries: Vec<Delivery>) -> IngestRequest {
        IngestRequest {
            tenant: TenantId::DEFAULT,
            mail_from: String::new(),
            raw: stream(raw, 1000),
            deliveries,
            received_at: SystemClock.now(),
        }
    }

    /// Письмо, собранное из объектов хранилища (скелет + части по `PartRef`).
    async fn stored(env: &Env, scope: AccountScope, id: MessageId) -> Vec<u8> {
        let msg = env.meta.get(scope, id).await.expect("get").expect("msg");
        let skel = object(env, &msg.skeleton).await;
        let skel = crate::split::Skeleton::decode(skel).expect("decode");
        let mut parts = HashMap::new();
        for p in &msg.parts {
            parts.insert(p.part.clone(), object(env, &p.blob).await);
        }
        skel.join(|path| parts.get(path).cloned()).expect("join")
    }

    async fn inbox_count(env: &Env, d: &Delivery) -> usize {
        let scope = AccountScope {
            tenant: TenantId::DEFAULT,
            account: d.account,
        };
        env.meta
            .list(scope, d.mailbox, MessageQuery::All, 1000)
            .await
            .expect("list")
            .len()
    }

    #[tokio::test]
    async fn ten_recipients_store_parts_once() {
        let env = Env::new();
        let raw = mail();
        let split = Split::new(Bytes::from(raw.clone()));
        let unique: std::collections::HashSet<_> = split.cuts().iter().map(|c| c.hash).collect();
        let mut ds = recipients(&env, 10).await;
        ds[3].extra_headers = vec![("Delivered-To".into(), "u3@example.org".into())];
        let done = env
            .store
            .ingest(request(&raw, ds.clone()))
            .await
            .expect("ingest")
            .delivered;
        assert_eq!(done.len(), 10);
        // Части — один раз; скелетов два: общий и с заголовком получателя.
        assert_eq!(env.blobs.calls(BlobOp::Put), unique.len() as u64 + 2);
        for h in &unique {
            let row = env.index.row(TenantId::DEFAULT, h).expect("row");
            assert_eq!(row.refcnt, 10, "по ссылке на получателя");
        }
        for (d, r) in ds.iter().zip(&done) {
            assert_eq!(inbox_count(&env, d).await, 1);
            let scope = AccountScope {
                tenant: TenantId::DEFAULT,
                account: d.account,
            };
            let msg = env
                .meta
                .get(scope, r.message)
                .await
                .expect("get")
                .expect("msg");
            assert_eq!(msg.parts.len(), split.cuts().len());
            let mut expected = super::header_block(&d.extra_headers, &[]);
            expected.extend_from_slice(&raw);
            assert_eq!(msg.size, expected.len() as u64);
            let got = stored(&env, scope, r.message).await;
            assert!(got == expected, "побайтово");
        }
        // Повторная доставка того же письма: объекты уже есть, загрузок нет.
        let before = env.blobs.calls(BlobOp::Put);
        env.store
            .ingest(request(&raw, ds[..2].to_vec()))
            .await
            .expect("again");
        assert_eq!(env.blobs.calls(BlobOp::Put), before);
        for h in &unique {
            assert_eq!(env.index.row(TenantId::DEFAULT, h).expect("row").refcnt, 12);
        }
    }

    /// Приёмка MS-15: текстовая часть хранится сжатой (zstd), читается без изменений; хэш, размер и
    /// дедупликация — по исходному содержимому.
    #[tokio::test]
    async fn text_part_stored_compressed() {
        use tp_contracts::blob::{BlobIndex, Compression};

        let env = Env::new();
        let text = "Съешь же ещё этих мягких французских булок, да выпей чаю.\r\n".repeat(100);
        let raw = format!(
            "From: a@example.org\r\nTo: u0@example.org\r\nSubject: t\r\nMIME-Version: 1.0\r\n\
             Content-Type: multipart/mixed; boundary=b\r\n\r\n\
             --b\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n\
             {text}\r\n--b\r\nContent-Type: text/plain\r\n\r\nshort\r\n--b--\r\n"
        )
        .into_bytes();
        let split = Split::new(Bytes::from(raw.clone()));
        let cut = split
            .cuts()
            .iter()
            .find(|c| split.body(c).len() >= text.len())
            .expect("text part");
        let ds = recipients(&env, 2).await;
        let done = env
            .store
            .ingest(request(&raw, ds.clone()))
            .await
            .expect("ingest")
            .delivered;
        let scope = AccountScope {
            tenant: TenantId::DEFAULT,
            account: ds[0].account,
        };
        let msg = env
            .meta
            .get(scope, done[0].message)
            .await
            .expect("get")
            .expect("msg");
        let part = msg
            .parts
            .iter()
            .find(|p| p.part == cut.path)
            .expect("part ref");
        assert_eq!(part.blob.hash, cut.hash, "хэш — исходного содержимого");
        assert_eq!(part.blob.hash, split::hash(&split.body(cut)));
        assert_eq!(
            env.index
                .compression(&part.blob)
                .await
                .expect("compression"),
            Some(Compression::Zstd)
        );
        let kept = env.blobs.bytes(&part.blob.path()).expect("object");
        assert!(
            kept.len() < split.body(cut).len() / 2,
            "сжато: {} из {}",
            kept.len(),
            split.body(cut).len()
        );
        assert_eq!(
            env.index
                .row(TenantId::DEFAULT, &cut.hash)
                .expect("row")
                .size,
            split.body(cut).len() as u64,
            "размер в учёте — исходный"
        );
        assert!(object(&env, &part.blob).await == split.body(cut));
        let got: Vec<Bytes> = env
            .store
            .fetch_raw(scope, done[0].message, None)
            .await
            .expect("fetch")
            .try_collect()
            .await
            .expect("read");
        assert!(got.concat() == raw, "письмо читается без изменений");
        let m = env.store.writer_metrics();
        assert!(m.compressed >= 1, "{m:?}");

        // Дедупликация: та же часть — без новых загрузок, ссылки добавляются.
        let puts = env.blobs.calls(BlobOp::Put);
        env.store
            .ingest(request(&raw, ds[..1].to_vec()))
            .await
            .expect("again");
        assert_eq!(env.blobs.calls(BlobOp::Put), puts);
        assert_eq!(
            env.index
                .row(TenantId::DEFAULT, &cut.hash)
                .expect("row")
                .refcnt,
            3
        );
    }

    #[tokio::test]
    async fn upload_failure_leaves_no_metadata() {
        let env = Env::new();
        let raw = mail();
        let split = Split::new(Bytes::from(raw.clone()));
        let unique: std::collections::HashSet<_> = split.cuts().iter().map(|c| c.hash).collect();
        let ds = recipients(&env, 10).await;

        // Сбой первой загрузки.
        env.blobs.fail_next(BlobOp::Put, Error::UNAVAILABLE);
        let err = env
            .store
            .ingest(request(&raw, ds.clone()))
            .await
            .expect_err("fail");
        assert!(err.is_retryable());
        for d in &ds {
            assert_eq!(inbox_count(&env, d).await, 0);
        }
        for h in &unique {
            assert!(
                env.index
                    .row(TenantId::DEFAULT, h)
                    .is_none_or(|r| r.refcnt == 0)
            );
        }

        // Части уже есть (доставка другому получателю); сбой загрузки нового скелета
        // снимает добавленные ссылки частей и общего скелета.
        let other = recipients(&env, 11).await.pop().expect("other");
        env.store
            .ingest(request(&raw, vec![other]))
            .await
            .expect("other");
        let mut ds2 = ds.clone();
        ds2[0].extra_headers = vec![("Delivered-To".into(), "u0@example.org".into())];
        env.blobs.fail_next(BlobOp::Put, Error::UNAVAILABLE);
        let err = env
            .store
            .ingest(request(&raw, ds2))
            .await
            .expect_err("fail");
        assert!(err.is_retryable());
        for d in &ds {
            assert_eq!(inbox_count(&env, d).await, 0);
        }
        for h in &unique {
            assert_eq!(env.index.row(TenantId::DEFAULT, h).expect("row").refcnt, 1);
        }
        let common = split::hash(&split.skeleton(b""));
        assert_eq!(
            env.index
                .row(TenantId::DEFAULT, &common)
                .expect("row")
                .refcnt,
            1
        );

        // После сбоя письмо принимается.
        env.store
            .ingest(request(&raw, ds.clone()))
            .await
            .expect("retry");
        for h in &unique {
            assert_eq!(env.index.row(TenantId::DEFAULT, h).expect("row").refcnt, 11);
        }
    }

    #[tokio::test]
    async fn empty_deliveries_invalid() {
        let env = Env::new();
        let err = env
            .store
            .ingest(request(b"Subject: x\r\n\r\nb", vec![]))
            .await;
        assert!(matches!(err, Err(Error::Invalid(_))));
    }

    struct Journal {
        to: Delivery,
        deny: bool,
    }

    #[async_trait::async_trait]
    impl DecisionHook for Journal {
        fn name(&self) -> &str {
            "journal"
        }

        async fn on_delivery(&self, op: &DeliveryInfo) -> Result<Decision> {
            assert_eq!(op.mail_from, "bounce@example.net");
            if self.deny {
                return Ok(Decision::Deny("policy".into()));
            }
            Ok(Decision::Augment(Augment {
                headers: vec![("X-Journal".into(), "yes".into())],
                copies: vec![
                    CopyTo::Mailbox(self.to.clone()),
                    CopyTo::Address("archive@example.com".into()),
                ],
            }))
        }
    }

    #[tokio::test]
    async fn hook_augments_and_denies() {
        let env = Env::new();
        let js = env.meta.account("journal@example.org").await.expect("acc");
        let to = Delivery {
            account: js.account,
            mailbox: env
                .meta
                .mailbox_by_role(js, SpecialUse::Inbox)
                .expect("inbox"),
            extra_headers: vec![],
            flags: [Flag::Seen].into(),
        };
        let with_hook = |deny: bool| {
            crate::Store::new(crate::Deps {
                hooks: DecisionHooks::new(vec![Arc::new(Journal {
                    to: to.clone(),
                    deny,
                })]),
                ..env.store.d.clone()
            })
        };
        let store = with_hook(false);
        let mut ds = recipients(&env, 2).await;
        // Отправитель — с конверта, не из заголовка.
        ds[0].extra_headers = vec![("Return-Path".into(), "<header@example.net>".into())];
        let raw = mail();
        let req = |ds: Vec<Delivery>| IngestRequest {
            mail_from: "bounce@example.net".into(),
            ..request(&raw, ds)
        };
        let done = store.ingest(req(ds.clone())).await.expect("ingest");
        assert_eq!(done.forward, ["archive@example.com"]);
        assert_eq!(
            done.delivered.len(),
            2,
            "копии журнала вызывающему не возвращаются"
        );
        let copies = env
            .meta
            .list(js, to.mailbox, MessageQuery::All, 10)
            .await
            .expect("list");
        assert_eq!(copies.len(), 1);
        assert!(copies[0].flags.contains(&Flag::Seen));
        let got = stored(&env, js, copies[0].id).await;
        assert!(got.starts_with(b"X-Journal: yes\r\n"));
        assert!(got.ends_with(&raw));

        let err = with_hook(true).ingest(req(ds.clone())).await;
        assert_eq!(err, Err(Error::Forbidden));
        assert_eq!(inbox_count(&env, &ds[1]).await, 1, "запрет не доставляет");
    }

    #[tokio::test]
    async fn insert_failure_leaves_nothing() {
        let env = Env::new();
        let raw = mail();
        let mut ds = recipients(&env, 3).await;
        let events = env.meta.event_log().all().len();
        // Папки нет — сбой на последней доставке: транзакция не раскладывает ни одной.
        ds[2].mailbox = tp_contracts::MailboxId::new();
        let err = env.store.ingest(request(&raw, ds.clone())).await;
        assert_eq!(err, Err(Error::NotFound));
        for d in &ds[..2] {
            assert_eq!(inbox_count(&env, d).await, 0, "копий нет");
            let scope = AccountScope {
                tenant: TenantId::DEFAULT,
                account: d.account,
            };
            let trash = env
                .meta
                .mailbox_by_role(scope, SpecialUse::Trash)
                .expect("trash");
            let trashed = env
                .meta
                .list(scope, trash, MessageQuery::All, 10)
                .await
                .expect("list");
            assert!(trashed.is_empty(), "в корзине копий нет");
        }
        assert_eq!(env.meta.event_log().all().len(), events, "событий нет");
        let split = Split::new(Bytes::from(raw));
        let skeleton = split::hash(&split.skeleton(b""));
        for h in split.cuts().iter().map(|c| c.hash).chain([skeleton]) {
            assert_eq!(env.index.row(TenantId::DEFAULT, &h).expect("row").refcnt, 0);
        }
    }

    #[tokio::test]
    async fn append_is_not_a_delivery() {
        use futures::StreamExt;
        use tp_contracts::bus::{EventBus, EventKind};
        use tp_contracts::meta::Flags;

        let env = Env::new();
        // Точка принятия решений доставки запрещает всё: APPEND её не вызывает.
        let hook = tp_testkit::RecordingDecisionHook::new("journal");
        hook.set_delivery(Decision::Deny("policy".into()));
        let store = crate::Store::new(crate::Deps {
            hooks: DecisionHooks::new(vec![Arc::new(hook.clone())]),
            ..env.store.d.clone()
        });
        let scope = env.meta.account("client@example.org").await.expect("acc");
        let drafts = env
            .meta
            .mailbox_by_role(scope, SpecialUse::Drafts)
            .expect("drafts");
        let mut events = env.bus.subscribe(scope).await.expect("sub");
        let raw = mail();
        let flags: Flags = [Flag::Draft, Flag::Seen].into();
        let at = SystemClock.now();
        let done = store
            .append(scope, drafts, stream(&raw, 777), flags.clone(), at)
            .await
            .expect("append");
        assert_eq!((done.account, done.mailbox), (scope.account, drafts));
        assert!(hook.calls().is_empty(), "on_delivery не вызывается");
        assert_eq!(
            stored(&env, scope, done.message).await,
            raw,
            "побайтово, без добавленных заголовков"
        );
        let msg = env
            .meta
            .get(scope, done.message)
            .await
            .expect("get")
            .expect("msg");
        assert_eq!(
            (msg.uid, msg.flags, msg.size, msg.received_at),
            (done.uid, flags, raw.len() as u64, at)
        );
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), events.next())
            .await
            .expect("event in time")
            .expect("event");
        assert_eq!(
            event.kind,
            EventKind::MailboxChanged {
                mailbox: drafts,
                modseq: msg.modseq
            }
        );
        // Копий в другие ящики нет: письмо одно, только в папке назначения.
        assert_eq!(
            env.meta
                .list(scope, drafts, MessageQuery::All, 10)
                .await
                .expect("list")
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn append_checks_quota_and_mailbox() {
        let env = Env::new();
        let scope = env.meta.account("client@example.org").await.expect("acc");
        let inbox = env
            .meta
            .mailbox_by_role(scope, SpecialUse::Inbox)
            .expect("inbox");
        let raw = mail();
        let at = SystemClock.now();
        env.quota.set_limit(scope, Some(raw.len() as u64 - 1));
        let puts = env.blobs.calls(BlobOp::Put);
        let err = env
            .store
            .append(scope, inbox, stream(&raw, 1000), Default::default(), at)
            .await;
        assert_eq!(err, Err(Error::QuotaExceeded));
        assert_eq!(env.blobs.calls(BlobOp::Put), puts, "квота — до загрузки");

        env.quota.set_limit(scope, Some(raw.len() as u64));
        let err = env
            .store
            .append(
                scope,
                tp_contracts::MailboxId::new(),
                stream(&raw, 1000),
                Default::default(),
                at,
            )
            .await;
        assert_eq!(err, Err(Error::NotFound), "нет папки");
        let split = Split::new(Bytes::from(raw.clone()));
        let skeleton = split::hash(&split.skeleton(b""));
        for h in split.cuts().iter().map(|c| c.hash).chain([skeleton]) {
            assert_eq!(
                env.index.row(TenantId::DEFAULT, &h).expect("row").refcnt,
                0,
                "ссылки сняты"
            );
        }
        let done = env
            .store
            .append(scope, inbox, stream(&raw, 1000), Default::default(), at)
            .await
            .expect("append within quota");
        assert_eq!(stored(&env, scope, done.message).await, raw);
    }

    #[test]
    fn header_checks() {
        let ok = vec![("X-A".to_string(), "v\r\n folded".to_string())];
        assert!(check_headers(&ok).is_ok());
        for (n, v) in [
            ("", "v"),
            ("X A", "v"),
            ("X:", "v"),
            ("X", "a\r\nB: c"),
            ("X", "a\nb"),
        ] {
            assert!(
                check_headers(&[(n.into(), v.into())]).is_err(),
                "{n:?} {v:?}"
            );
        }
    }
}
