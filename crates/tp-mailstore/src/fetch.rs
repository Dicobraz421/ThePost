//! Выдача письма (CONTRACTS.md#MailStore `fetch_raw`, `fetch_part`).
//!
//! Письмо не собирается в памяти: читается только объект-скелет (заголовки и границы MIME), затем поток
//! отдаёт по порядку байты скелета и диапазоны объектов частей (`BlobStore::get` с диапазоном), открывая
//! следующий объект только после выдачи предыдущего.

use std::ops::Range;
use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use futures::{StreamExt, TryStreamExt, stream};
use tp_contracts::blob::{BlobStore, ByteStream};
use tp_contracts::meta::{AccountScope, Message, PartRef};
use tp_contracts::{BlobKey, Error, MessageId, Result};

use crate::Store;
use crate::split::{Segment, Skeleton};

/// Кусок выдачи.
enum Piece {
    /// Байты скелета.
    Inline(Bytes),
    /// Диапазон объекта части.
    Blob(BlobKey, Range<u64>),
}

impl Store {
    pub(crate) async fn fetch_raw_impl(
        &self,
        scope: AccountScope,
        message: MessageId,
        range: Option<Range<u64>>,
    ) -> Result<ByteStream> {
        let (msg, skel) = self.load(scope, message).await?;
        let range = clamp(range, skel.size())?;
        self.stream(&msg.parts, &skel, range)
    }

    pub(crate) async fn fetch_part_impl(
        &self,
        scope: AccountScope,
        message: MessageId,
        part_path: &str,
    ) -> Result<ByteStream> {
        let (msg, skel) = self.load(scope, message).await?;
        let range = skel.section(part_path).ok_or(Error::NotFound)?;
        self.stream(&msg.parts, &skel, range)
    }

    /// Метаданные письма и его скелет.
    async fn load(&self, scope: AccountScope, message: MessageId) -> Result<(Message, Skeleton)> {
        let msg = self
            .d
            .messages
            .get(scope, message)
            .await?
            .ok_or(Error::NotFound)?;
        let data = self
            .d
            .blobs
            .get(&msg.skeleton, None)
            .await
            .map_err(missing_object)?
            .try_fold(BytesMut::new(), |mut buf, chunk| async move {
                buf.extend_from_slice(&chunk);
                Ok(buf)
            })
            .await
            .map_err(missing_object)?;
        let skel = Skeleton::decode(data.freeze())?;
        Ok((msg, skel))
    }

    /// Поток байтов письма в диапазоне `range` (внутри письма).
    fn stream(&self, parts: &[PartRef], skel: &Skeleton, range: Range<u64>) -> Result<ByteStream> {
        let mut pieces = Vec::new();
        for seg in skel.segments() {
            let r = seg.range();
            let (start, end) = (r.start.max(range.start), r.end.min(range.end));
            if start >= end {
                continue;
            }
            let (from, to) = (start - r.start, end - r.start);
            match seg {
                Segment::Inline { data, .. } => {
                    pieces.push(Piece::Inline(data.slice(from as usize..to as usize)));
                }
                Segment::Part { len, path, .. } => {
                    let part = parts
                        .iter()
                        .find(|p| p.part == *path)
                        .filter(|p| p.size == *len)
                        .ok_or_else(|| {
                            Error::internal(format!("письмо: нет объекта части {path}"))
                        })?;
                    pieces.push(Piece::Blob(part.blob, from..to));
                }
            }
        }
        let blobs: Arc<dyn BlobStore> = self.d.blobs.clone();
        Ok(stream::iter(pieces)
            .then(move |piece| {
                let blobs = blobs.clone();
                async move {
                    match piece {
                        Piece::Inline(b) => Ok(stream::once(async move { Ok(b) }).boxed()),
                        Piece::Blob(key, r) => {
                            blobs.get(&key, Some(r)).await.map_err(missing_object)
                        }
                    }
                }
            })
            .try_flatten()
            .boxed())
    }
}

/// Объект письма пропал из хранилища — нарушение целостности, а не «письма нет».
fn missing_object(e: Error) -> Error {
    match e {
        Error::NotFound => Error::internal("письмо: объект BLOB не найден"),
        e => e,
    }
}

/// Диапазон внутри письма размером `size`: конец обрезается по размеру; начало за концом письма
/// или после конца диапазона — `Error::Invalid` (как у `BlobStore::get`).
fn clamp(range: Option<Range<u64>>, size: u64) -> Result<Range<u64>> {
    let Some(r) = range else {
        return Ok(0..size);
    };
    if r.start > r.end || r.start > size {
        return Err(Error::invalid(format!(
            "диапазон {r:?} вне письма ({size} байт)"
        )));
    }
    Ok(r.start..r.end.min(size))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tp_contracts::TenantId;
    use tp_contracts::mailstore::{Delivery, IngestRequest, MailStore};
    use tp_contracts::meta::SpecialUse;
    use tp_testkit::blob::read_all;
    use tp_testkit::{BlobOp, Clock, LoadModel, SyntheticMail, SystemClock};

    use crate::testenv::{Env, stream as input};

    async fn deliver(env: &Env, raw: &[u8]) -> (AccountScope, MessageId) {
        let scope = env.meta.account("a@example.org").await.expect("acc");
        let delivery = Delivery {
            account: scope.account,
            mailbox: env
                .meta
                .mailbox_by_role(scope, SpecialUse::Inbox)
                .expect("inbox"),
            extra_headers: vec![],
            flags: Default::default(),
        };
        let done = env
            .store
            .ingest(IngestRequest {
                tenant: TenantId::DEFAULT,
                raw: input(raw, 64 * 1024),
                deliveries: vec![delivery],
                received_at: SystemClock.now(),
            })
            .await
            .expect("ingest");
        (scope, done[0].message)
    }

    async fn raw(env: &Env, scope: AccountScope, id: MessageId, r: Option<Range<u64>>) -> Bytes {
        read_all(env.store.fetch_raw(scope, id, r).await.expect("fetch"))
            .await
            .expect("read")
    }

    #[tokio::test]
    async fn corpus_fetch_is_exact() {
        let env = Env::new();
        for mail in tp_testkit::corpus().expect("corpus") {
            let (scope, id) = deliver(&env, &mail.raw).await;
            let got = raw(&env, scope, id, None).await;
            assert!(got.as_ref() == mail.raw.as_slice(), "{}", mail.name);
        }
    }

    #[tokio::test]
    async fn ranges_on_part_boundaries() {
        let env = Env::new();
        let mail = tp_testkit::corpus::corpus_mail("mixed-three-attachments").expect("mail");
        let (scope, id) = deliver(&env, &mail.raw).await;
        let (_, skel) = env.store.load(scope, id).await.expect("load");
        let size = skel.size();
        assert_eq!(size, mail.raw.len() as u64);
        let mut points: Vec<u64> = skel.segments().iter().map(|s| s.range().start).collect();
        points.push(size);
        assert!(points.len() > 4, "письмо из нескольких сегментов");
        for &b in &points {
            for r in [
                b.saturating_sub(1)..(b + 1).min(size),
                b..b,
                b.saturating_sub(3)..b,
                b..(b + 3).min(size),
                0..b,
                b..size,
                b.saturating_sub(1)..size + 100,
            ] {
                let got = raw(&env, scope, id, Some(r.clone())).await;
                let end = r.end.min(size) as usize;
                assert!(
                    got.as_ref() == &mail.raw[r.start as usize..end],
                    "диапазон {r:?}"
                );
            }
        }
        // Диапазон на несколько частей сразу.
        let (a, b) = (points[1] + 1, points[points.len() - 2] - 1);
        let got = raw(&env, scope, id, Some(a..b)).await;
        assert!(got.as_ref() == &mail.raw[a as usize..b as usize]);
        for bad in [size + 1..size + 2, Range { start: 5, end: 4 }] {
            let err = env
                .store
                .fetch_raw(scope, id, Some(bad.clone()))
                .await
                .err();
            assert!(matches!(err, Some(Error::Invalid(_))), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn large_attachment_streams() {
        let env = Env::new();
        let model = LoadModel {
            avg_body: 2048,
            attachment_ratio: 1.0,
            avg_attachment: 4 * 1024 * 1024,
            ..LoadModel::default()
        };
        let mail = SyntheticMail::new(42, model).next().expect("mail");
        let text = String::from_utf8(mail.clone()).expect("ascii");
        let start = text.find("Content-Disposition: attachment").expect("att");
        let start = start + text[start..].find("\r\n\r\n").expect("body") + 4;
        let end = text.rfind("\r\n--syn-1--").expect("end");
        assert!(end - start > 2 * 1024 * 1024);
        let (scope, id) = deliver(&env, &mail).await;

        // Тело вложения — из объекта части кусками хранилища, без сборки письма.
        let before = env.blobs.calls(BlobOp::Get);
        let chunks: Vec<Bytes> = env
            .store
            .fetch_part(scope, id, "2")
            .await
            .expect("part")
            .try_collect()
            .await
            .expect("read");
        assert_eq!(
            env.blobs.calls(BlobOp::Get) - before,
            2,
            "скелет и одна часть"
        );
        assert!(chunks.len() > 100 && chunks.iter().all(|c| c.len() <= 4096));
        let body: Vec<u8> = chunks.concat();
        assert!(body == mail[start..end], "тело в кодировке передачи");

        let got = raw(&env, scope, id, None).await;
        assert!(got.as_ref() == mail.as_slice());
        let r = (start as u64 + 1000)..(start as u64 + 2_000_000);
        let got = raw(&env, scope, id, Some(r.clone())).await;
        assert!(got.as_ref() == &mail[r.start as usize..r.end as usize]);
    }

    #[tokio::test]
    async fn fetch_part_paths() {
        let env = Env::new();
        let mail = tp_testkit::corpus::corpus_mail("mixed-pdf").expect("mail");
        let (scope, id) = deliver(&env, &mail.raw).await;
        let text = read_all(env.store.fetch_part(scope, id, "1").await.expect("1"))
            .await
            .expect("read");
        assert!(!text.is_empty());
        let body = read_all(env.store.fetch_part(scope, id, "").await.expect("body"))
            .await
            .expect("read");
        let hdr_end = mail
            .raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("hdr")
            + 4;
        assert!(
            body.as_ref() == &mail.raw[hdr_end..],
            "тело письма после заголовков"
        );
        let err = env.store.fetch_part(scope, id, "9").await.err();
        assert_eq!(err, Some(Error::NotFound));
        let err = env
            .store
            .fetch_raw(scope, MessageId::new(), None)
            .await
            .err();
        assert_eq!(err, Some(Error::NotFound));
    }

    #[tokio::test]
    async fn nested_part_matches_parser() {
        let env = Env::new();
        let mail = tp_testkit::corpus::corpus_mail("rfc822-with-attachment").expect("mail");
        let (scope, id) = deliver(&env, &mail.raw).await;
        let msg = mail_parser::MessageParser::default()
            .parse(&mail.raw)
            .expect("parse");
        let nested = msg.attachment(0).and_then(|a| a.message()).expect("nested");
        let inner = nested.attachment(0).expect("inner attachment");
        let (_, skel) = env.store.load(scope, id).await.expect("load");
        let paths: Vec<_> = skel.sections().iter().map(|s| s.path.clone()).collect();
        let path = paths
            .iter()
            .rev()
            .find(|p| p.starts_with("2."))
            .expect("nested path");
        let got = read_all(env.store.fetch_part(scope, id, path).await.expect("part"))
            .await
            .expect("read");
        let expected = &mail.raw[inner.offset_body as usize..inner.offset_end as usize];
        assert!(got.as_ref() == expected, "{path} из {paths:?}");
    }

    #[tokio::test]
    async fn lost_object_is_internal() {
        let env = Env::new();
        let mail = tp_testkit::corpus::corpus_mail("mixed-pdf").expect("mail");
        let (scope, id) = deliver(&env, &mail.raw).await;
        env.blobs.fail_next(BlobOp::Get, Error::NotFound);
        let err = env.store.fetch_raw(scope, id, None).await.err();
        assert!(matches!(err, Some(Error::Internal(_))));
    }
}
