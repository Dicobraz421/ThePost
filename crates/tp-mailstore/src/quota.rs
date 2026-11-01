//! Квоты (CONTRACTS.md#Trash, Retention, Quota (Ф1); SRS 4.7, 3.8).
//!
//! Использование считает реализация [`Quota`](tp_contracts::retention::Quota) по метаданным: сумма размеров писем
//! учётной записи, включая корзину; лимит — из `PolicyResolver::quota_bytes`. Поэтому учёт размера здесь —
//! проверка добавляемых байтов перед изменением:
//! - `ingest` — размер копии каждой доставки (с заголовками получателя), суммарно по учётной записи; превышение
//!   у одного получателя — `Error::QuotaExceeded` для всего запроса, до разбора и загрузки объектов;
//! - `copy` — сумма размеров копируемых писем;
//! - [`Store::check_quota`] — проверка без приёма (SMTP до `250`: `RCPT TO`, `MAIL FROM SIZE=`).
//!
//! `move_to`, `delete_to_trash` и `restore` использование не меняют (корзина входит в квоту); окончательное
//! удаление (`expunge_final`, истечение корзины) уменьшает его в хранилище метаданных.
//! Проверка и изменение — не одна транзакция: параллельные доставки могут превысить лимит на размер письма.
//!
//! # Предупреждения (SRS 4.7)
//!
//! После успешных `ingest` и `copy` использование до изменения сравнивается с порогами [`QuotaWarnConfig`]
//! (по умолчанию 80 и 95 % лимита): пересечённый порог — служебное письмо во «Входящие» учётной записи
//! (без [`Store::with_mailboxes`] — в папку, куда шла доставка или копия). Письмо квоту не проверяет и новых
//! предупреждений не вызывает; при пересечении нескольких порогов сразу — одно письмо о старшем.
//! Не чаще раза за [`QuotaWarnConfig::interval`] на порог: отметки — в памяти узла (после перезапуска
//! или на другом узле повтор возможен, но только при новом пересечении — колебании около порога).
//! Ошибка отправки предупреждения операцию не отменяет.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use schemars::JsonSchema;
use serde::Deserialize;
use time::OffsetDateTime;
use tp_contracts::config::duration;
use tp_contracts::mailstore::{Delivery, IngestRequest};
use tp_contracts::meta::{AccountScope, Flags, MailboxStore, QuotaUsage, SpecialUse};
use tp_contracts::{AccountId, Error, MailboxId, MessageId, Result, TenantId};

use crate::Store;

/// Предупреждения о заполнении квоты (секция `quota` конфигурации хранилища).
///
/// ```yaml
/// thresholds: [80, 95]
/// interval: 1d
/// from: postmaster@localhost
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct QuotaWarnConfig {
    /// Пороги заполнения, % лимита (1–100); пусто — без предупреждений.
    pub thresholds: Vec<u8>,
    /// Не чаще одного предупреждения на порог за этот срок.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub interval: Duration,
    /// Отправитель предупреждения (заголовок `From`).
    pub from: String,
}

impl Default for QuotaWarnConfig {
    fn default() -> Self {
        Self {
            thresholds: vec![80, 95],
            interval: Duration::from_secs(86_400),
            from: "postmaster@localhost".to_owned(),
        }
    }
}

/// Отметка предупреждения: организация, учётная запись, порог.
type WarnKey = (TenantId, AccountId, u8);

/// Отметки отправленных предупреждений узла: ключ → момент.
#[derive(Debug, Clone, Default)]
pub(crate) struct Warned(Arc<Mutex<HashMap<WarnKey, Instant>>>);

/// Проверенное добавление: использование до него (для порогов предупреждений).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Pending {
    pub(crate) account: AccountId,
    before: QuotaUsage,
    added: u64,
    /// Папка для предупреждения, если «Входящие» не найти: папка доставки или копии.
    pub(crate) fallback: Option<MailboxId>,
}

impl Store {
    /// Можно ли принять письмо `size` байт для учётной записи, иначе `Error::QuotaExceeded` — проверка
    /// получателя без приёма письма (SMTP до ответа `250`, SRS 4.1). Ничего не меняет.
    pub async fn check_quota(&self, scope: AccountScope, size: u64) -> Result<()> {
        self.check_quotas(scope.tenant, &[(scope.account, size)])
            .await
            .map(drop)
    }

    /// «Входящие» для предупреждений о квоте (без этого — папка доставки или копии).
    pub fn with_mailboxes(mut self, mailboxes: Arc<dyn MailboxStore>) -> Self {
        self.mailboxes = Some(mailboxes);
        self
    }

    /// Проверяет квоты: `demands` — (учётная запись, добавляемые байты), повторы суммируются.
    pub(crate) async fn check_quotas(
        &self,
        tenant: TenantId,
        demands: &[(AccountId, u64)],
    ) -> Result<Vec<Pending>> {
        let mut total: Vec<(AccountId, u64)> = Vec::new();
        for &(account, bytes) in demands {
            match total.iter_mut().find(|(a, _)| *a == account) {
                Some((_, sum)) => *sum = sum.saturating_add(bytes),
                None => total.push((account, bytes)),
            }
        }
        let mut pending = Vec::with_capacity(total.len());
        for (account, added) in total {
            let before = self.d.quota.usage(AccountScope { tenant, account }).await?;
            if !before.allows(added) {
                return Err(Error::QuotaExceeded);
            }
            pending.push(Pending {
                account,
                before,
                added,
                fallback: None,
            });
        }
        Ok(pending)
    }

    /// Проверяет квоту на копии писем (отсутствующие пропускаются, как в `copy`).
    pub(crate) async fn check_copy_quota(
        &self,
        scope: AccountScope,
        ids: &[MessageId],
    ) -> Result<Vec<Pending>> {
        let mut bytes = 0u64;
        for id in ids {
            if let Some(m) = self.d.messages.get(scope, *id).await? {
                bytes = bytes.saturating_add(m.size);
            }
        }
        if bytes == 0 {
            return Ok(Vec::new());
        }
        self.check_quotas(scope.tenant, &[(scope.account, bytes)])
            .await
    }

    /// Предупреждения о пересечённых порогах после добавления.
    pub(crate) async fn warn_quota(&self, tenant: TenantId, pending: &[Pending]) {
        let cfg = &self.cfg.quota;
        for p in pending {
            let Some(limit) = p.before.limit_bytes else {
                continue;
            };
            let after = p.before.used_bytes.saturating_add(p.added);
            let crossed: Vec<u8> = cfg
                .thresholds
                .iter()
                .copied()
                .filter(|&t| {
                    (1..=100).contains(&t)
                        && reaches(p.before.used_bytes, limit, t).is_lt()
                        && reaches(after, limit, t).is_ge()
                })
                .collect();
            let Some(&top) = crossed.iter().max() else {
                continue;
            };
            if !self
                .warned
                .mark(tenant, p.account, &crossed, top, cfg.interval)
            {
                continue;
            }
            let scope = AccountScope {
                tenant,
                account: p.account,
            };
            if let Err(e) = self
                .send_warning(scope, top, after, limit, p.fallback)
                .await
            {
                tracing::warn!(error = %e, threshold = top, "mailstore: предупреждение о квоте не доставлено");
                self.warned.unmark(tenant, p.account, top);
            }
        }
    }

    async fn send_warning(
        &self,
        scope: AccountScope,
        threshold: u8,
        used: u64,
        limit: u64,
        fallback: Option<MailboxId>,
    ) -> Result<()> {
        let inbox = match &self.mailboxes {
            Some(m) => m
                .list(scope)
                .await?
                .into_iter()
                .find(|b| b.role == Some(SpecialUse::Inbox))
                .map(|b| b.id),
            None => None,
        };
        let mailbox = inbox.or(fallback).ok_or(Error::NotFound)?;
        let now = OffsetDateTime::now_utc();
        let raw = warning_letter(&self.cfg.quota.from, threshold, used, limit, now);
        let chunk: Result<bytes::Bytes> = Ok(bytes::Bytes::from(raw));
        self.ingest_inner(
            IngestRequest {
                tenant: scope.tenant,
                mail_from: String::new(),
                raw: Box::pin(futures::stream::iter([chunk])),
                deliveries: vec![Delivery {
                    account: scope.account,
                    mailbox,
                    extra_headers: vec![],
                    flags: Flags::new(),
                }],
                received_at: now,
            },
            true,
        )
        .await
        .map(drop)
    }
}

impl Warned {
    /// Отмечает пороги `crossed`; `false` — о `top` уже предупреждали за `interval`.
    fn mark(
        &self,
        tenant: TenantId,
        account: AccountId,
        crossed: &[u8],
        top: u8,
        interval: Duration,
    ) -> bool {
        let now = Instant::now();
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let recent = |at: &Instant| now.saturating_duration_since(*at) < interval;
        if map.get(&(tenant, account, top)).is_some_and(recent) {
            return false;
        }
        // Пересечения редки: устаревшие отметки снимаются при каждой новой.
        map.retain(|_, at| recent(at));
        for &t in crossed {
            map.insert((tenant, account, t), now);
        }
        true
    }

    fn unmark(&self, tenant: TenantId, account: AccountId, threshold: u8) {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        map.remove(&(tenant, account, threshold));
    }
}

/// Сравнивает долю `used / limit` с порогом `percent` %.
fn reaches(used: u64, limit: u64, percent: u8) -> std::cmp::Ordering {
    (u128::from(used) * 100).cmp(&(u128::from(limit) * u128::from(percent)))
}

/// Служебное письмо о заполнении (RFC 5322; `Auto-Submitted` RFC 3834 — автоответы на него не отправляются).
fn warning_letter(
    from: &str,
    threshold: u8,
    used: u64,
    limit: u64,
    now: OffsetDateTime,
) -> Vec<u8> {
    let percent = u128::from(used) * 100 / u128::from(limit.max(1));
    let subject = encoded_word(&format!("Почтовый ящик заполнен на {threshold} %"));
    let date = rfc5322_date(now);
    let id = MessageId::new();
    let (used, limit) = (human(used), human(limit));
    format!(
        "From: {from}\r\n\
         Subject: {subject}\r\n\
         Date: {date}\r\n\
         Message-ID: <quota.{threshold}.{id}@thepost>\r\n\
         Auto-Submitted: auto-generated\r\n\
         X-ThePost-Quota-Warning: {threshold}\r\n\
         MIME-Version: 1.0\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Transfer-Encoding: 8bit\r\n\
         \r\n\
         Почтовый ящик заполнен на {percent} %: занято {used} из {limit}.\r\n\
         \r\n\
         Когда место закончится, новые письма перестанут приниматься. Удалите ненужные письма\r\n\
         и очистите корзину: удалённые письма в ней тоже занимают место.\r\n"
    )
    .into_bytes()
}

/// Encoded-word RFC 2047 (`Q`, UTF-8).
fn encoded_word(s: &str) -> String {
    let mut out = String::from("=?utf-8?q?");
    for b in s.bytes() {
        match b {
            b' ' => out.push('_'),
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' => out.push(char::from(b)),
            _ => {
                let _ = write!(out, "={b:02X}");
            }
        }
    }
    out.push_str("?=");
    out
}

/// Дата RFC 5322 в UTC.
fn rfc5322_date(t: OffsetDateTime) -> String {
    let t = t.to_offset(time::UtcOffset::UTC);
    let weekday = t.weekday().to_string();
    let month = t.month().to_string();
    format!(
        "{}, {} {} {} {:02}:{:02}:{:02} +0000",
        &weekday[..3],
        t.day(),
        &month[..3],
        t.year(),
        t.hour(),
        t.minute(),
        t.second()
    )
}

/// Размер для человека: байты, КБ, МБ, ГБ (степени 1024).
fn human(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["байт", "КБ", "МБ", "ГБ"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[0])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use futures::TryStreamExt;
    use tp_contracts::Error;
    use tp_contracts::mailstore::{Delivery, IngestRequest, Ingested, MailStore};
    use tp_contracts::meta::{
        Flags, MailboxStore, MessageQuery, MessageStore, NewMailbox, SpecialUse,
    };
    use tp_contracts::{BlobKey, MailboxId};

    use super::*;
    use crate::testenv::{Env, stream};

    struct Fixture {
        env: Env,
        scope: AccountScope,
        inbox: MailboxId,
        raw: Vec<u8>,
    }

    /// Без предупреждений: служебные письма не меняют использование.
    async fn fixture() -> Fixture {
        let quiet = warn_config(QuotaWarnConfig {
            thresholds: vec![],
            ..QuotaWarnConfig::default()
        });
        fixture_mail(Env::with_config(quiet, false), "mixed-pdf").await
    }

    /// Большое письмо: служебные письма малы относительно него и порогов не сдвигают.
    async fn fixture_with(env: Env) -> Fixture {
        fixture_mail(env, "long-line-100k-html").await
    }

    async fn fixture_mail(env: Env, mail: &str) -> Fixture {
        let scope = env.meta.account("a@example.org").await.expect("acc");
        let inbox = env
            .meta
            .mailbox_by_role(scope, SpecialUse::Inbox)
            .expect("inbox");
        let raw = tp_testkit::corpus::corpus_mail(mail).expect("mail").raw;
        Fixture {
            env,
            scope,
            inbox,
            raw,
        }
    }

    impl Fixture {
        async fn ingest(&self, copies: usize) -> Result<Ingested> {
            self.ingest_to(self.inbox, copies).await
        }

        async fn ingest_to(&self, mailbox: MailboxId, copies: usize) -> Result<Ingested> {
            let delivery = Delivery {
                account: self.scope.account,
                mailbox,
                extra_headers: vec![],
                flags: Flags::new(),
            };
            self.env
                .store
                .ingest(IngestRequest {
                    tenant: TenantId::DEFAULT,
                    mail_from: String::new(),
                    raw: stream(&self.raw, 4096),
                    deliveries: vec![delivery; copies],
                    received_at: OffsetDateTime::now_utc(),
                })
                .await
        }

        async fn count(&self, mailbox: MailboxId) -> usize {
            MessageStore::list(&self.env.meta, self.scope, mailbox, MessageQuery::All, 100)
                .await
                .expect("list")
                .len()
        }

        fn size(&self) -> u64 {
            self.raw.len() as u64
        }

        /// Темы предупреждений о квоте в папке.
        async fn warnings(&self, mailbox: MailboxId) -> Vec<String> {
            MessageStore::list(&self.env.meta, self.scope, mailbox, MessageQuery::All, 1000)
                .await
                .expect("list")
                .into_iter()
                .filter_map(|m| m.meta.envelope.subject)
                .filter(|s| s.starts_with("Почтовый ящик заполнен"))
                .collect()
        }

        async fn archive(&self) -> MailboxId {
            self.env
                .meta
                .create(
                    self.scope,
                    NewMailbox {
                        parent: None,
                        name: "Archive".into(),
                        role: None,
                    },
                )
                .await
                .expect("create")
                .id
        }

        /// Окончательно удаляет `n` писем (не предупреждений) папки.
        async fn purge(&self, mailbox: MailboxId, n: usize) {
            let ids: Vec<MessageId> =
                MessageStore::list(&self.env.meta, self.scope, mailbox, MessageQuery::All, 1000)
                    .await
                    .expect("list")
                    .into_iter()
                    .filter(|m| {
                        m.meta
                            .envelope
                            .from
                            .first()
                            .is_none_or(|a| a.email != "postmaster@localhost")
                    })
                    .take(n)
                    .map(|m| m.id)
                    .collect();
            assert_eq!(ids.len(), n);
            self.env
                .store
                .delete_to_trash(self.scope, &ids)
                .await
                .expect("trash");
            MessageStore::expunge_final(&self.env.meta, self.scope, &ids)
                .await
                .expect("expunge");
        }
    }

    fn warn_config(quota: QuotaWarnConfig) -> crate::Config {
        crate::Config {
            quota,
            ..crate::Config::default()
        }
    }

    fn no_interval() -> crate::Config {
        warn_config(QuotaWarnConfig {
            interval: Duration::ZERO,
            ..QuotaWarnConfig::default()
        })
    }

    #[tokio::test]
    async fn ingest_at_threshold() {
        let f = fixture().await;
        f.env.quota.set_default_limit(Some(2 * f.size()));
        let first = f.ingest(1).await.expect("ingest").delivered[0].message;
        f.ingest(1).await.expect("ровно на пороге");
        let m = MessageStore::get(&f.env.meta, f.scope, first)
            .await
            .expect("get")
            .expect("msg");
        let keys: Vec<BlobKey> = std::iter::once(m.skeleton)
            .chain(m.parts.iter().map(|p| p.blob))
            .collect();
        let refs = |f: &Fixture| {
            keys.iter()
                .map(|k| f.env.index.row(k.tenant, &k.hash).expect("row").refcnt)
                .collect::<Vec<_>>()
        };
        let before = refs(&f);

        // Сверх порога — отказ без метаданных и новых ссылок.
        assert_eq!(f.ingest(1).await, Err(Error::QuotaExceeded));
        assert_eq!(f.count(f.inbox).await, 2);
        assert_eq!(refs(&f), before);

        // Корзина входит в квоту; окончательное удаление освобождает место.
        f.env
            .store
            .delete_to_trash(f.scope, &[first])
            .await
            .expect("trash");
        assert_eq!(f.ingest(1).await, Err(Error::QuotaExceeded));
        MessageStore::expunge_final(&f.env.meta, f.scope, &[first])
            .await
            .expect("expunge");
        f.ingest(1).await.expect("место освободилось");
    }

    #[tokio::test]
    async fn ingest_sums_deliveries_of_account() {
        let f = fixture().await;
        f.env.quota.set_default_limit(Some(2 * f.size() - 1));
        assert_eq!(f.ingest(2).await, Err(Error::QuotaExceeded));
        assert_eq!(f.count(f.inbox).await, 0);
        f.ingest(1).await.expect("одна копия помещается");
    }

    #[tokio::test]
    async fn copy_checks_quota() {
        let f = fixture().await;
        let a = f.ingest(1).await.expect("ingest").delivered[0].message;
        let b = f.ingest(1).await.expect("ingest").delivered[0].message;
        let to = f
            .env
            .meta
            .create(
                f.scope,
                NewMailbox {
                    parent: None,
                    name: "Archive".into(),
                    role: None,
                },
            )
            .await
            .expect("create")
            .id;
        f.env.quota.set_default_limit(Some(3 * f.size()));
        assert_eq!(
            f.env.store.copy(f.scope, &[a, b], to).await,
            Err(Error::QuotaExceeded)
        );
        assert_eq!(f.count(to).await, 0);
        f.env.store.copy(f.scope, &[a], to).await.expect("copy");
        assert_eq!(f.count(to).await, 1);
        // Перенос квоту не меняет.
        f.env.store.move_to(f.scope, &[b], to).await.expect("move");
    }

    #[tokio::test]
    async fn warns_once_per_threshold() {
        let f = fixture_with(Env::new()).await;
        let archive = f.archive().await;
        f.env.quota.set_default_limit(Some(10 * f.size()));
        for _ in 0..7 {
            f.ingest_to(archive, 1).await.expect("ingest");
        }
        assert!(f.warnings(archive).await.is_empty());
        // 80 %: одно письмо — в папку доставки (без `with_mailboxes`).
        f.ingest_to(archive, 1).await.expect("ingest");
        assert_eq!(
            f.warnings(archive).await,
            ["Почтовый ящик заполнен на 80 %"]
        );
        // Выше порога — без новых писем.
        f.ingest_to(archive, 1).await.expect("ingest");
        assert_eq!(f.warnings(archive).await.len(), 1);
        // Колебание около порога в пределах суток — без повтора.
        f.purge(archive, 2).await;
        f.ingest_to(archive, 1).await.expect("ingest");
        assert_eq!(f.warnings(archive).await.len(), 1);
        assert!(f.warnings(f.inbox).await.is_empty());
    }

    #[tokio::test]
    async fn warning_goes_to_inbox_and_repeats_after_interval() {
        let f = fixture_with(Env::with_config(no_interval(), true)).await;
        let archive = f.archive().await;
        f.env.quota.set_default_limit(Some(10 * f.size()));
        for _ in 0..8 {
            f.ingest_to(archive, 1).await.expect("ingest");
        }
        assert_eq!(f.warnings(f.inbox).await.len(), 1);
        assert!(f.warnings(archive).await.is_empty());

        // Письмо — служебное: отправитель из конфигурации, без автоответов, тело в UTF-8.
        let warning = MessageStore::list(&f.env.meta, f.scope, f.inbox, MessageQuery::All, 10)
            .await
            .expect("list")
            .remove(0);
        assert_eq!(
            warning.meta.envelope.from[0].email,
            QuotaWarnConfig::default().from
        );
        let raw: Vec<u8> = f
            .env
            .store
            .fetch_raw(f.scope, warning.id, None)
            .await
            .expect("fetch")
            .try_fold(Vec::new(), |mut v, c| async move {
                v.extend_from_slice(&c);
                Ok(v)
            })
            .await
            .expect("read");
        let text = String::from_utf8(raw).expect("utf-8");
        assert!(
            text.contains("\r\nAuto-Submitted: auto-generated\r\n"),
            "{text}"
        );
        assert!(text.contains("заполнен на 80 %: занято"), "{text}");

        // Ниже порога и снова выше: срок вышел (interval = 0) — повтор.
        f.purge(archive, 1).await;
        f.ingest_to(archive, 1).await.expect("ingest");
        assert_eq!(f.warnings(f.inbox).await.len(), 2);
    }

    #[tokio::test]
    async fn several_thresholds_at_once_warn_about_top() {
        let f = fixture_with(Env::with_config(no_interval(), true)).await;
        f.env.quota.set_default_limit(Some(10 * f.size()));
        f.ingest(7).await.expect("ingest");
        f.ingest(3).await.expect("ingest");
        assert_eq!(
            f.warnings(f.inbox).await,
            ["Почтовый ящик заполнен на 95 %"]
        );
        // Служебное письмо квоту не проверяет; новые письма уже не помещаются.
        assert_eq!(f.ingest(1).await, Err(Error::QuotaExceeded));
    }

    #[tokio::test]
    async fn copy_warns() {
        let f = fixture_with(Env::new()).await;
        let a = f.ingest(1).await.expect("ingest").delivered[0].message;
        let to = f.archive().await;
        f.env.quota.set_default_limit(Some(2 * f.size()));
        f.env.store.copy(f.scope, &[a], to).await.expect("copy");
        assert_eq!(f.warnings(to).await, ["Почтовый ящик заполнен на 95 %"]);
    }

    #[tokio::test]
    async fn no_limit_or_thresholds_no_warnings() {
        let f = fixture_with(Env::new()).await;
        f.ingest(3).await.expect("ingest");
        assert!(f.warnings(f.inbox).await.is_empty());

        let cfg = warn_config(QuotaWarnConfig {
            thresholds: vec![],
            ..QuotaWarnConfig::default()
        });
        let f = fixture_with(Env::with_config(cfg, true)).await;
        f.env.quota.set_default_limit(Some(2 * f.size()));
        f.ingest(2).await.expect("ingest");
        assert!(f.warnings(f.inbox).await.is_empty());
    }

    #[tokio::test]
    async fn check_quota_without_ingest() {
        let f = fixture().await;
        f.env.quota.set_default_limit(Some(2 * f.size()));
        f.env
            .store
            .check_quota(f.scope, 2 * f.size())
            .await
            .expect("пусто");
        f.ingest(1).await.expect("ingest");
        f.env
            .store
            .check_quota(f.scope, f.size())
            .await
            .expect("ровно на пороге");
        assert_eq!(
            f.env.store.check_quota(f.scope, f.size() + 1).await,
            Err(Error::QuotaExceeded)
        );
        // Проверка ничего не принимает и не предупреждает.
        assert_eq!(f.count(f.inbox).await, 1);
    }

    #[test]
    fn letter_parts() {
        assert_eq!(
            encoded_word("Ящик 9 %"),
            "=?utf-8?q?=D0=AF=D1=89=D0=B8=D0=BA_9_=25?="
        );
        let t = OffsetDateTime::from_unix_timestamp(1_761_955_200).expect("t");
        assert_eq!(rfc5322_date(t), "Sat, 1 Nov 2025 00:00:00 +0000");
        assert_eq!(human(512), "512 байт");
        assert_eq!(human(3 * 1024 * 1024 / 2), "1.5 МБ");
        assert!(reaches(80, 100, 80).is_eq());
        assert!(reaches(79, 100, 80).is_lt());
        assert!(reaches(u64::MAX, u64::MAX, 100).is_eq());
    }
}
