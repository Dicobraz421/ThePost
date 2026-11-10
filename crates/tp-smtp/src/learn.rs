//! Обучение антиспама по перемещению писем пользователем (SRS 4.1, по модели imapsieve Dovecot; SRS 2.5).
//!
//! [`SpamLearner`] — подписчик журнала событий ядра ([`EventSink`]): событие `Moved` в папку с ролью «Спам»
//! (`\Junk`) из другой папки — письмо в Rspamd controller на `learnspam`; из «Спама» в другую папку, кроме
//! корзины, — на `learnham`. Письмо читается из хранилища ([`MailStore::fetch_raw`]); удалённое или снова
//! перенесённое (нет письма) и крупнее `learn.max_message_size` — пропускается. Пароль controller — из
//! хранилища секретов (`learn.password`), запрос — [`RspamdController`] по HTTPS (`http://` — только loopback).
//!
//! Доставка событий — не менее одного раза: сбой controller возвращает ошибку, пачка придёт снова; уже
//! обученные письма пачки помнятся (`learn.remember`) и повторно не передаются. Выключено по умолчанию
//! (`learn.enabled`).

use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use futures::TryStreamExt;
use tp_contracts::events::{CoreEventKind, EventSink, LoggedEvent};
use tp_contracts::mailstore::MailStore;
use tp_contracts::meta::{AccountScope, MailboxStore, SpecialUse};
use tp_contracts::secrets::SecretStore;
use tp_contracts::{Error, MailboxId, MessageId, Result};

use crate::config::LearnConfig;

/// Имя подписчика (ключ позиций в журнале событий).
pub const SUBSCRIBER: &str = "smtp-learn";

/// Чему учить.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Learn {
    /// Спам (`learnspam`).
    Spam,
    /// Не спам (`learnham`).
    Ham,
}

impl Learn {
    /// Команда controller.
    pub fn command(self) -> &'static str {
        match self {
            Learn::Spam => "learnspam",
            Learn::Ham => "learnham",
        }
    }
}

/// Controller антиспама.
#[async_trait]
pub trait Controller: Send + Sync + 'static {
    /// Передаёт письмо `raw` на обучение `learn`; `password` — заголовок `Password`.
    async fn learn(&self, learn: Learn, password: Option<&str>, raw: Bytes) -> Result<()>;
}

/// Rspamd controller по HTTP API (`POST /learnspam`, `POST /learnham`).
#[derive(Debug, Clone)]
pub struct RspamdController {
    client: reqwest::Client,
    base: reqwest::Url,
}

impl RspamdController {
    /// Клиент для `learn.url`: `https://`, либо `http://` на адрес loopback; без перенаправлений,
    /// таймаут — `learn.timeout`. Нет адреса или он некорректен — `Error::Invalid`.
    pub fn new(cfg: &LearnConfig) -> Result<Self> {
        let url = cfg
            .url
            .as_deref()
            .ok_or_else(|| Error::invalid("smtp.learn.url is required"))?;
        let base =
            reqwest::Url::parse(url).map_err(|e| Error::invalid(format!("smtp.learn.url: {e}")))?;
        let https = match base.scheme() {
            "https" => true,
            "http" if is_loopback(&base) => false,
            _ => {
                return Err(Error::invalid(
                    "smtp.learn.url: https is required (http only for loopback)",
                ));
            }
        };
        let client = reqwest::Client::builder()
            .https_only(https)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(cfg.timeout)
            .build()
            .map_err(|e| Error::internal(format!("smtp.learn: {e}")))?;
        Ok(Self { client, base })
    }
}

fn is_loopback(url: &reqwest::Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

#[async_trait]
impl Controller for RspamdController {
    async fn learn(&self, learn: Learn, password: Option<&str>, raw: Bytes) -> Result<()> {
        let url = self
            .base
            .join(learn.command())
            .map_err(|e| Error::internal(format!("smtp.learn.url: {e}")))?;
        let mut req = self.client.post(url).body(raw);
        if let Some(p) = password {
            req = req.header("Password", p);
        }
        let resp = req.send().await.map_err(|e| {
            tracing::warn!(error = %e, "smtp: Rspamd controller недоступен");
            Error::UNAVAILABLE
        })?;
        let status = resp.status();
        // 208 — письмо уже обучено тем же классом.
        if status.is_success() {
            return Ok(());
        }
        tracing::warn!(%status, command = learn.command(), "smtp: Rspamd controller отклонил обучение");
        if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            Err(Error::UNAVAILABLE)
        } else if status == reqwest::StatusCode::UNAUTHORIZED
            || status == reqwest::StatusCode::FORBIDDEN
        {
            Err(Error::Forbidden)
        } else {
            Err(Error::internal(format!("rspamd controller: HTTP {status}")))
        }
    }
}

/// Последние обученные письма (повтор пачки событий не обучает повторно).
#[derive(Debug, Default)]
struct Recent {
    set: HashSet<(MessageId, Learn)>,
    order: VecDeque<(MessageId, Learn)>,
}

impl Recent {
    fn insert(&mut self, key: (MessageId, Learn), cap: usize) {
        if cap == 0 || !self.set.insert(key) {
            return;
        }
        self.order.push_back(key);
        while self.order.len() > cap {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
    }
}

/// Подписчик событий перемещения: обучение антиспама.
pub struct SpamLearner {
    cfg: LearnConfig,
    store: Arc<dyn MailStore>,
    mailboxes: Arc<dyn MailboxStore>,
    secrets: Option<Arc<dyn SecretStore>>,
    controller: Arc<dyn Controller>,
    recent: Mutex<Recent>,
}

impl fmt::Debug for SpamLearner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpamLearner")
            .field("enabled", &self.cfg.enabled)
            .finish_non_exhaustive()
    }
}

impl SpamLearner {
    /// Подписчик с параметрами `learn`: письма — из `store`, роли папок — `mailboxes`, пароль — `secrets`
    /// (нужно, если задан `learn.password`), обучение — `controller`.
    pub fn new(
        cfg: LearnConfig,
        store: Arc<dyn MailStore>,
        mailboxes: Arc<dyn MailboxStore>,
        secrets: Option<Arc<dyn SecretStore>>,
        controller: Arc<dyn Controller>,
    ) -> Self {
        Self {
            cfg,
            store,
            mailboxes,
            secrets,
            controller,
            recent: Mutex::new(Recent::default()),
        }
    }

    async fn role(&self, scope: AccountScope, id: MailboxId) -> Result<Option<SpecialUse>> {
        Ok(self.mailboxes.get(scope, id).await?.and_then(|m| m.role))
    }

    /// Чему учить по переносу из `from` в `to`.
    async fn verdict(
        &self,
        scope: AccountScope,
        from: MailboxId,
        to: MailboxId,
    ) -> Result<Option<Learn>> {
        if from == to {
            return Ok(None);
        }
        let to_role = self.role(scope, to).await?;
        let from_role = self.role(scope, from).await?;
        Ok(match (from_role, to_role) {
            (Some(SpecialUse::Junk), Some(SpecialUse::Junk)) => None,
            (_, Some(SpecialUse::Junk)) => Some(Learn::Spam),
            (Some(SpecialUse::Junk), Some(SpecialUse::Trash)) => None,
            (Some(SpecialUse::Junk), _) => Some(Learn::Ham),
            _ => None,
        })
    }

    async fn password(&self) -> Result<Option<String>> {
        let Some(r) = &self.cfg.password else {
            return Ok(None);
        };
        let store = self.secrets.as_ref().ok_or_else(|| {
            Error::internal("smtp.learn.password is set, but no secret store is configured")
        })?;
        Ok(Some(store.get(r).await?.expose().to_owned()))
    }

    async fn raw(&self, scope: AccountScope, message: MessageId) -> Result<Option<Bytes>> {
        let stream = match self.store.fetch_raw(scope, message, None).await {
            Ok(s) => s,
            Err(Error::NotFound) => return Ok(None),
            Err(e) => return Err(e),
        };
        let chunks: Vec<Bytes> = match stream.try_collect().await {
            Ok(c) => c,
            Err(Error::NotFound) => return Ok(None),
            Err(e) => return Err(e),
        };
        Ok(Some(chunks.concat().into()))
    }
}

#[async_trait]
impl EventSink for SpamLearner {
    fn name(&self) -> &str {
        SUBSCRIBER
    }

    async fn handle(&self, batch: &[LoggedEvent]) -> Result<()> {
        if !self.cfg.enabled {
            return Ok(());
        }
        let mut password: Option<Option<String>> = None;
        for logged in batch {
            let e = &logged.event;
            let (
                CoreEventKind::Moved {
                    from_mailbox,
                    message,
                    ..
                },
                Some(to),
            ) = (&e.kind, e.mailbox)
            else {
                continue;
            };
            let scope = AccountScope {
                tenant: e.tenant,
                account: e.account,
            };
            let Some(learn) = self.verdict(scope, *from_mailbox, to).await? else {
                continue;
            };
            let key = (*message, learn);
            if lock(&self.recent).set.contains(&key) {
                continue;
            }
            let Some(raw) = self.raw(scope, *message).await? else {
                continue;
            };
            if raw.len() as u64 > self.cfg.max_message_size {
                tracing::info!(
                    size = raw.len(),
                    "smtp: письмо слишком велико для обучения антиспама"
                );
                continue;
            }
            if password.is_none() {
                password = Some(self.password().await?);
            }
            let pw = password.as_ref().and_then(|p| p.as_deref());
            self.controller.learn(learn, pw, raw).await?;
            tracing::info!(
                account = %e.account,
                command = learn.command(),
                "smtp: письмо передано на обучение антиспама"
            );
            lock(&self.recent).insert(key, self.cfg.remember);
        }
        Ok(())
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests;
