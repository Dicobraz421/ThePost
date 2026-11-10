//! Адресация (SRS 4.1, модель Postfix: `recipient_delimiter`, `canonical_maps`; решение
//! 2026-11-01-borrowings-security): подадреса, переписывание адресов конверта. Псевдонимы и адрес для всей почты
//! домена (catch-all) — записи каталога ([`crate::deliver`]).
//!
//! - Подадрес `user+tag@domain`: разделитель — любой символ `addressing.subaddress_separators` (первый в
//!   локальной части); письмо доставляется в ящик `user@domain`, полный адрес — в заголовке `Delivered-To`
//!   копии получателя (тег для Sieve, расширение `subaddress` RFC 5233).
//! - Catch-all: псевдоним каталога с адресом `@domain` получает письма на неизвестные адреса домена.
//!   Порядок разрешения: адрес целиком, адрес без тега, `@domain`.
//! - Переписывание адресов конверта — таблица организации [`TenantTransport::rewrites`] (Admin API, снимок
//!   настроек транспорта [`crate::routes::Routes`], перечитывается по сигналу `TransportChanged`; SMTP-29):
//!   правило для адреса точнее правила для домена, одно правило на адрес, без цепочек
//!   ([`TenantTransport::rewrite`]). Отправитель конверта переписывается после проверок SPF, DKIM, DMARC (они —
//!   по исходному), получатели — до разрешения ([`crate::deliver`]). Заголовки письма не меняются (это — правила
//!   потока почты). Ключ `smtp.addressing.rewrite` упразднён: его наличие — ошибка разбора конфигурации.
//!
//! [`TenantTransport::rewrites`]: tp_contracts::transport::TenantTransport::rewrites
//! [`TenantTransport::rewrite`]: tp_contracts::transport::TenantTransport::rewrite

use crate::config::AddressingConfig;

/// Правила адресации секции `smtp.addressing`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Addressing {
    separators: Vec<char>,
}

impl Addressing {
    /// Правила из секции.
    pub fn new(cfg: &AddressingConfig) -> Self {
        Self {
            separators: cfg.subaddress_separators.chars().collect(),
        }
    }

    /// Подадрес: `(адрес без тега, тег)`; `None` — разделителя нет или подадреса выключены.
    pub fn split<'a>(&self, address: &'a str) -> Option<(String, &'a str)> {
        let (local, domain) = address.rsplit_once('@')?;
        let i = local.find(|c| self.separators.contains(&c))?;
        let base = &local[..i];
        if base.is_empty() {
            return None;
        }
        let sep = local[i..].chars().next().map_or(1, char::len_utf8);
        Some((format!("{base}@{domain}"), &local[i + sep..]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subaddress() {
        let a = Addressing::new(&AddressingConfig {
            subaddress_separators: "+-".into(),
            ..AddressingConfig::default()
        });
        assert_eq!(
            a.split("user+news@example.org"),
            Some(("user@example.org".into(), "news"))
        );
        assert_eq!(
            a.split("first-last+x@example.org"),
            Some(("first@example.org".into(), "last+x"))
        );
        assert_eq!(a.split("+tag@example.org"), None);
        assert_eq!(a.split("user@example.org"), None);
        // Без разделителей подадреса выключены.
        let off = Addressing::new(&AddressingConfig {
            subaddress_separators: String::new(),
            ..AddressingConfig::default()
        });
        assert_eq!(off.split("user+news@example.org"), None);
    }
}
