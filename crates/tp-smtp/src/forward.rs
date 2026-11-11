//! Пересылка наружу (SRS 4.1; решение 2026-11-01-transport-scope-external-mta): перенаправление Sieve
//! (`Ingested::forward`), правила потока почты, пересылки модулей.
//!
//! Пересылаемое письмо уходит в очередь с отправителем конверта SRS ([`crate::srs`], секция `smtp.forward.srs`)
//! и с ARC-подписью узла (RFC 8617, `smtp.forward.arc`, `mail-auth`): получатель проходит проверки SPF (по нашему
//! узлу) и DMARC (по цепочке ARC). Ключи — из хранилища секретов, в кэше на `key_refresh`. Уведомление о
//! недоставке на адрес SRS принимает smtp-in: адрес проверяется (подпись, срок) и переписывается обратно
//! ([`crate::Delivery::with_forwarding`]).

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use mail_auth::arc::ArcSealer;
use mail_auth::common::crypto::DkimKey;
use mail_auth::dkim::Done;
use tp_contracts::secrets::SecretStore;
use tp_contracts::{Error, Result};

use crate::config::ForwardConfig;
use crate::dkim_sign::parse_key;
use crate::srs::Srs;

/// ARC-подпись узла.
pub type Sealer = ArcSealer<DkimKey, Done>;

#[derive(Default)]
struct Cache {
    srs: Option<(Instant, Arc<Srs>)>,
    sealer: Option<(Instant, Arc<Sealer>)>,
}

/// SRS и ARC пересылки с ключами из хранилища секретов.
pub struct Forwarding {
    secrets: Arc<dyn SecretStore>,
    cfg: ForwardConfig,
    cache: Mutex<Cache>,
}

impl fmt::Debug for Forwarding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Forwarding")
            .field("srs", &self.cfg.srs.as_ref().map(|s| &s.domain))
            .field("arc", &self.cfg.arc.as_ref().map(|a| &a.domain))
            .finish_non_exhaustive()
    }
}

impl Forwarding {
    /// Пересылка по секции `cfg` с ключами из `secrets`.
    pub fn new(secrets: Arc<dyn SecretStore>, cfg: ForwardConfig) -> Self {
        Self {
            secrets,
            cfg,
            cache: Mutex::default(),
        }
    }

    /// Домен адресов SRS (без обращения к хранилищу секретов).
    pub(crate) fn srs_domain(&self) -> Option<&str> {
        self.cfg.srs.as_ref().map(|s| s.domain.as_str())
    }

    /// Схема SRS; `None` — выключена. Ключ недоступен — ошибка (временный отказ).
    pub(crate) async fn srs(&self) -> Result<Option<Arc<Srs>>> {
        let Some(cfg) = &self.cfg.srs else {
            return Ok(None);
        };
        if let Some((at, s)) = &lock(&self.cache).srs
            && at.elapsed() < self.cfg.key_refresh
        {
            return Ok(Some(s.clone()));
        }
        let mut keys = Vec::with_capacity(1 + cfg.previous_keys.len());
        for r in std::iter::once(&cfg.key).chain(&cfg.previous_keys) {
            keys.push(self.secrets.get(r).await?.expose().as_bytes().to_vec());
        }
        if keys.iter().any(Vec::is_empty) {
            return Err(Error::invalid("smtp.forward.srs: empty key"));
        }
        let srs = Arc::new(Srs::new(&cfg.domain, keys, cfg.max_age_days));
        lock(&self.cache).srs = Some((Instant::now(), srs.clone()));
        Ok(Some(srs))
    }

    /// ARC-подпись; `None` — выключена.
    pub(crate) async fn sealer(&self) -> Result<Option<Arc<Sealer>>> {
        let Some(cfg) = &self.cfg.arc else {
            return Ok(None);
        };
        if let Some((at, s)) = &lock(&self.cache).sealer
            && at.elapsed() < self.cfg.key_refresh
        {
            return Ok(Some(s.clone()));
        }
        let secret = self.secrets.get(&cfg.key).await?;
        let key = parse_key(cfg.algorithm, secret.expose()).map_err(|e| {
            Error::Invalid(format!(
                "arc key {}._domainkey.{}: {e}",
                cfg.selector, cfg.domain
            ))
        })?;
        let sealer = Arc::new(
            ArcSealer::from_key(key)
                .domain(cfg.domain.as_str())
                .selector(cfg.selector.as_str())
                .headers(cfg.headers.iter().cloned()),
        );
        lock(&self.cache).sealer = Some((Instant::now(), sealer.clone()));
        Ok(Some(sealer))
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
