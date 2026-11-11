//! Личные списки надёжных и заблокированных отправителей ящика (CONTRACTS.md#SenderLists; SRS 4.1,
//! решение 2026-11-04-exchange-parity; CTR-35).
//!
//! Применяются при доставке поверх вердикта антиспама (`tp-smtp`): письмо заблокированного отправителя — в папку
//! «Спам», надёжного — во «Входящие». Управление — JMAP-расширение `urn:thepost:senderlists` и Admin API.
//! Записи — адрес (`user@example.org`) или домен (`example.org`, на входе допускается `@example.org`); сравнение без
//! учёта регистра ASCII, домен — только точное совпадение (поддомены не входят). Реализация — `tp-meta` (META-33),
//! in-memory — `tp-testkit`.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::meta::AccountScope;

/// Наибольшее число записей в каждом списке.
pub const MAX_SENDER_ENTRIES: usize = 1024;

/// Наибольшая длина записи, байт (адрес — RFC 5321).
pub const MAX_SENDER_ENTRY_LEN: usize = 320;

/// Вердикт личных списков для отправителя.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SenderVerdict {
    /// Надёжный: во «Входящие» поверх вердикта антиспама.
    Safe,
    /// Заблокированный: в «Спам».
    Blocked,
}

/// Списки отправителей ящика.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SenderList {
    /// Надёжные адреса и домены.
    pub safe: Vec<String>,
    /// Заблокированные адреса и домены.
    pub blocked: Vec<String>,
}

/// Приводит запись к хранимому виду: нижний регистр ASCII, домен без ведущего `@`. Пустая, длиннее
/// [`MAX_SENDER_ENTRY_LEN`], с пробелами или управляющими символами, с пустой локальной частью или доменом —
/// `Error::Invalid`.
pub fn normalize_entry(entry: &str) -> Result<String> {
    let e = entry
        .strip_prefix('@')
        .unwrap_or(entry)
        .to_ascii_lowercase();
    let bad = || Error::invalid(format!("sender list: invalid entry {entry:?}"));
    if e.is_empty()
        || e.len() > MAX_SENDER_ENTRY_LEN
        || e.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(bad());
    }
    match e.rsplit_once('@') {
        Some((local, domain)) if local.is_empty() || domain.is_empty() => Err(bad()),
        _ => Ok(e),
    }
}

fn normalize_list(list: Vec<String>) -> Result<Vec<String>> {
    let mut out = list
        .iter()
        .map(|e| normalize_entry(e))
        .collect::<Result<Vec<_>>>()?;
    out.sort_unstable();
    out.dedup();
    if out.len() > MAX_SENDER_ENTRIES {
        return Err(Error::invalid(format!(
            "sender list: more than {MAX_SENDER_ENTRIES} entries"
        )));
    }
    Ok(out)
}

impl SenderList {
    /// Хранимый вид: записи [`normalize_entry`], по возрастанию без повторов, не более [`MAX_SENDER_ENTRIES`] в
    /// каждом списке; иначе `Error::Invalid`. Запись может быть в обоих списках (действует правило [`SenderList::verdict`]).
    pub fn normalized(self) -> Result<SenderList> {
        Ok(SenderList {
            safe: normalize_list(self.safe)?,
            blocked: normalize_list(self.blocked)?,
        })
    }

    /// Вердикт для отправителя `from` (адрес конверта или `From`): сначала адрес целиком, затем домен (часть после
    /// последнего `@`); на одном уровне надёжный приоритетнее заблокированного. Пустой адрес или нет совпадений — `None`.
    pub fn verdict(&self, from: &str) -> Option<SenderVerdict> {
        let from = from.to_ascii_lowercase();
        let (_, domain) = from.rsplit_once('@')?;
        let has = |list: &[String], key: &str| list.iter().any(|e| e.eq_ignore_ascii_case(key));
        for key in [from.as_str(), domain] {
            if key.is_empty() {
                continue;
            }
            if has(&self.safe, key) {
                return Some(SenderVerdict::Safe);
            }
            if has(&self.blocked, key) {
                return Some(SenderVerdict::Blocked);
            }
        }
        None
    }
}

/// Списки отправителей ящиков. Нет записи ящика — пустые списки, состояние 0.
#[async_trait]
pub trait SenderLists: Send + Sync + 'static {
    /// Списки ящика (хранимый вид [`SenderList::normalized`]).
    async fn sender_list(&self, scope: AccountScope) -> Result<SenderList>;

    /// Заменяет списки целиком (сохраняется [`SenderList::normalized`]); возвращает новое состояние.
    /// Некорректный список — `Error::Invalid`.
    async fn put_sender_list(&self, scope: AccountScope, list: SenderList) -> Result<u64>;

    /// Состояние списков ящика (JMAP `state`): растёт при каждом `put_sender_list`, у нового ящика 0.
    async fn sender_list_state(&self, scope: AccountScope) -> Result<u64>;

    /// Вердикт для отправителя ([`SenderList::verdict`]); по умолчанию — через [`SenderLists::sender_list`].
    async fn sender_verdict(
        &self,
        scope: AccountScope,
        from: &str,
    ) -> Result<Option<SenderVerdict>> {
        Ok(self.sender_list(scope).await?.verdict(from))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_safe() {
        fn _lists(_: std::sync::Arc<dyn SenderLists>) {}
    }

    #[test]
    fn normalize() {
        assert_eq!(
            normalize_entry("@Example.ORG").expect("domain"),
            "example.org"
        );
        assert_eq!(
            normalize_entry("Bob@Example.org").expect("address"),
            "bob@example.org"
        );
        for bad in ["", "@", "a@", "a b@example.org", "x@y\n", "@@example.org"] {
            assert!(normalize_entry(bad).is_err(), "{bad:?}");
        }
        let list = SenderList {
            safe: vec!["B@x.org".into(), "b@x.org".into(), "@a.org".into()],
            blocked: vec![],
        }
        .normalized()
        .expect("normalized");
        assert_eq!(list.safe, ["a.org", "b@x.org"]);
        let many = SenderList {
            safe: (0..=MAX_SENDER_ENTRIES)
                .map(|i| format!("u{i}@x.org"))
                .collect(),
            blocked: vec![],
        };
        assert!(matches!(many.normalized(), Err(Error::Invalid(_))));
    }

    #[test]
    fn verdict_order() {
        let list = SenderList {
            safe: vec!["joe@spam.example".into(), "partner.example".into()],
            blocked: vec![
                "spam.example".into(),
                "bad@partner.example".into(),
                "joe@spam.example".into(),
            ],
        }
        .normalized()
        .expect("normalized");
        assert_eq!(list.verdict("Joe@Spam.example"), Some(SenderVerdict::Safe));
        assert_eq!(
            list.verdict("eve@spam.example"),
            Some(SenderVerdict::Blocked)
        );
        assert_eq!(
            list.verdict("bad@partner.example"),
            Some(SenderVerdict::Blocked)
        );
        assert_eq!(
            list.verdict("ann@partner.example"),
            Some(SenderVerdict::Safe)
        );
        assert_eq!(list.verdict("ann@sub.partner.example"), None);
        assert_eq!(list.verdict(""), None);
        assert_eq!(list.verdict("nobody@else.example"), None);
    }
}
