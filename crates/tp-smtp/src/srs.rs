//! SRS — переписывание отправителя конверта при пересылке (Sender Rewriting Scheme, схема libsrs2; SRS 4.1;
//! решение 2026-11-01-transport-scope-external-mta).
//!
//! Пересылаемое наружу письмо уходит с отправителем в домене пересылающего (`smtp.forward.srs.domain`): SPF
//! получателя проверяет наш узел, а уведомление о недоставке возвращается к нам и пересылается исходному
//! отправителю.
//!
//! - `SRS0=HHHHHH=TT=domain=local@srs-domain` — исходный адрес `local@domain`; `TT` — день (base32, по модулю 1024),
//!   `HHHHHH` — HMAC-SHA256 ключа из хранилища секретов от `TT`, домена и локальной части (base32, без учёта
//!   регистра);
//! - `SRS1=HHHHHH=D==opaque@srs-domain` — адрес, уже переписанный узлом `D` (`SRS0=opaque@D` или `SRS1`):
//!   цепочка пересылок не удлиняется, срок проверяет первый узел.
//!
//! Обратное преобразование ([`Srs::reverse`]) проверяет подпись (действующим или прежним ключом — ротация) и
//! срок `max_age` дней: подделанный или просроченный адрес — [`SrsError`] (SMTP `550` на `RCPT`).

use std::fmt;

use sha2::{Digest, Sha256};
use time::OffsetDateTime;

/// Алфавит base32 (RFC 4648), как в libsrs2.
const B32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// Длина подписи, символов base32 (30 бит).
const HASH_LEN: usize = 6;

/// Период метки дня.
const DAYS: u32 = 1024;

/// Ошибка обратного преобразования.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SrsError {
    /// Адрес не в формате SRS или не нашего домена.
    #[error("not an SRS address")]
    NotSrs,
    /// Подпись не сходится (подделка, чужой ключ).
    #[error("bad SRS signature")]
    BadHash,
    /// Срок адреса истёк.
    #[error("SRS address expired")]
    Expired,
}

/// Переписывание адресов SRS: домен, ключи (первый подписывает, все проверяют), срок адреса.
#[derive(Clone)]
pub struct Srs {
    domain: String,
    keys: Vec<Vec<u8>>,
    max_age: u32,
}

impl fmt::Debug for Srs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Ключи в отладочный вывод не попадают.
        f.debug_struct("Srs")
            .field("domain", &self.domain)
            .field("keys", &self.keys.len())
            .field("max_age", &self.max_age)
            .finish()
    }
}

impl Srs {
    /// Схема домена `domain` с ключами `keys` (не пусто: первый — действующий) и сроком `max_age` дней.
    pub fn new(domain: &str, keys: Vec<Vec<u8>>, max_age: u32) -> Self {
        Self {
            domain: domain.trim_end_matches('.').to_ascii_lowercase(),
            keys,
            max_age: max_age.min(DAYS - 1),
        }
    }

    /// Домен адресов SRS.
    pub fn domain(&self) -> &str {
        &self.domain
    }

    /// Адрес — SRS нашего домена (по виду, без проверки подписи).
    pub fn is_srs(&self, address: &str) -> bool {
        split_srs(address).is_some_and(|(_, d)| d.eq_ignore_ascii_case(&self.domain))
    }

    /// Отправитель `sender` для пересылки в момент `now`; `None` — без переписывания (пустой отправитель —
    /// уведомление, адрес без `@`, адрес нашего домена SRS).
    pub fn forward(&self, sender: &str, now: OffsetDateTime) -> Option<String> {
        let key = self.keys.first()?;
        let (local, domain) = sender.rsplit_once('@')?;
        if local.is_empty() || domain.eq_ignore_ascii_case(&self.domain) {
            return None;
        }
        let tag = local.get(..4).map(str::to_ascii_uppercase);
        let sep = local.as_bytes().get(4).copied();
        let opaque = local.get(5..).unwrap_or_default();
        let local = match (tag.as_deref(), sep) {
            (Some("SRS0"), Some(b'=' | b'+' | b'-')) => {
                // Переписан узлом `domain`: SRS1 с его именем.
                let opaque = format!("={opaque}");
                format!("SRS1={}={domain}={opaque}", hash(key, &[domain, &opaque]))
            }
            (Some("SRS1"), Some(b'=' | b'+' | b'-')) => {
                // Цепочка: сохраняется первый узел и его адрес, подпись — наша.
                let (_, rest) = opaque.split_once('=')?;
                let (first, opaque) = rest.split_once('=')?;
                if first.is_empty() || !opaque.starts_with('=') {
                    return None;
                }
                format!("SRS1={}={first}={opaque}", hash(key, &[first, opaque]))
            }
            _ => {
                let tt = timestamp(now);
                format!(
                    "SRS0={}={tt}={domain}={local}",
                    hash(key, &[&tt, domain, local])
                )
            }
        };
        Some(format!("{local}@{}", self.domain))
    }

    /// Исходный адрес для адреса SRS `address` нашего домена в момент `now`.
    pub fn reverse(&self, address: &str, now: OffsetDateTime) -> Result<String, SrsError> {
        let (local, domain) = split_srs(address).ok_or(SrsError::NotSrs)?;
        if !domain.eq_ignore_ascii_case(&self.domain) {
            return Err(SrsError::NotSrs);
        }
        let srs1 = local[..4].eq_ignore_ascii_case("SRS1");
        let body = &local[5..];
        let (hash_in, rest) = body.split_once('=').ok_or(SrsError::NotSrs)?;
        if srs1 {
            // SRS1=HHH=D==opaque → SRS0=opaque@D.
            let (first, opaque) = rest.split_once('=').ok_or(SrsError::NotSrs)?;
            if first.is_empty() || !opaque.starts_with('=') {
                return Err(SrsError::NotSrs);
            }
            self.verify(hash_in, &[first, opaque])?;
            return Ok(format!("SRS0{opaque}@{first}"));
        }
        let mut parts = rest.splitn(3, '=');
        let (Some(tt), Some(orig_domain), Some(orig_local)) =
            (parts.next(), parts.next(), parts.next())
        else {
            return Err(SrsError::NotSrs);
        };
        if orig_domain.is_empty() || orig_local.is_empty() {
            return Err(SrsError::NotSrs);
        }
        self.verify(hash_in, &[tt, orig_domain, orig_local])?;
        let then = decode_timestamp(tt).ok_or(SrsError::NotSrs)?;
        let today = day(now);
        if (today + DAYS - then) % DAYS > self.max_age {
            return Err(SrsError::Expired);
        }
        Ok(format!("{orig_local}@{orig_domain}"))
    }

    fn verify(&self, given: &str, parts: &[&str]) -> Result<(), SrsError> {
        let ok = self.keys.iter().any(|k| {
            let want = hash(k, parts);
            // Сравнение без раннего выхода: время не выдаёт совпавший префикс.
            given.len() == want.len()
                && given.bytes().zip(want.bytes()).fold(0u8, |acc, (a, b)| {
                    acc | (a.to_ascii_uppercase() ^ b.to_ascii_uppercase())
                }) == 0
        });
        if ok { Ok(()) } else { Err(SrsError::BadHash) }
    }
}

/// Локальная часть с префиксом `SRS0`/`SRS1` и разделителем, и домен.
fn split_srs(address: &str) -> Option<(&str, &str)> {
    let (local, domain) = address.rsplit_once('@')?;
    let tag = local.get(..4)?;
    let sep = *local.as_bytes().get(4)?;
    (matches!(sep, b'=' | b'+' | b'-')
        && (tag.eq_ignore_ascii_case("SRS0") || tag.eq_ignore_ascii_case("SRS1")))
    .then_some((local, domain))
}

/// День `now` по модулю [`DAYS`].
fn day(now: OffsetDateTime) -> u32 {
    let days = now.unix_timestamp().div_euclid(86_400);
    u32::try_from(days.rem_euclid(i64::from(DAYS))).unwrap_or_default()
}

/// Метка дня: два символа base32.
fn timestamp(now: OffsetDateTime) -> String {
    let t = day(now) as usize;
    [B32[(t >> 5) & 31], B32[t & 31]]
        .iter()
        .map(|&b| b as char)
        .collect()
}

fn decode_timestamp(tt: &str) -> Option<u32> {
    let [a, b] = tt.as_bytes() else {
        return None;
    };
    let pos = |c: &u8| B32.iter().position(|x| x == &c.to_ascii_uppercase());
    Some(((pos(a)? as u32) << 5) | pos(b)? as u32)
}

/// Подпись частей адреса: HMAC-SHA256 (RFC 2104) от частей в нижнем регистре через `=`, первые
/// [`HASH_LEN`] символов base32.
fn hash(key: &[u8], parts: &[&str]) -> String {
    let data = parts.join("=").to_lowercase();
    let mac = hmac_sha256(key, data.as_bytes());
    let mut out = String::with_capacity(HASH_LEN);
    let mut acc = 0u32;
    let mut bits = 0;
    for &b in &mac {
        acc = (acc << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 && out.len() < HASH_LEN {
            bits -= 5;
            out.push(B32[((acc >> bits) & 31) as usize] as char);
        }
        if out.len() == HASH_LEN {
            break;
        }
    }
    out
}

/// HMAC-SHA256 (RFC 2104) на `sha2`.
fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let pad = |byte: u8| k.map(|b| b ^ byte);
    let inner = Sha256::new()
        .chain_update(pad(0x36))
        .chain_update(data)
        .finalize();
    Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize()
        .into()
}

#[cfg(test)]
mod tests;
