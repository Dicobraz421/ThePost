//! Адресация (SRS 4.1, модель Postfix: `recipient_delimiter`, `canonical_maps`; решение
//! 2026-11-01-borrowings-security): подадреса, переписывание адресов конверта. Псевдонимы и адрес для всей почты
//! домена (catch-all) — записи каталога ([`crate::deliver`]).
//!
//! - Подадрес `user+tag@domain`: разделитель — любой символ `addressing.subaddress_separators` (первый в
//!   локальной части); письмо доставляется в ящик `user@domain`, полный адрес — в заголовке `Delivered-To`
//!   копии получателя (тег для Sieve, расширение `subaddress` RFC 5233).
//! - Catch-all: псевдоним каталога с адресом `@domain` получает письма на неизвестные адреса домена.
//!   Порядок разрешения: адрес целиком, адрес без тега, `@domain`.
//! - Переписывание (`addressing.rewrite`): правило для адреса (`old@example.org`) точнее правила для домена
//!   (`@old.example`); замена — адрес или домен (`@new.example`: локальная часть сохраняется). Одно правило
//!   на адрес, без цепочек. Отправитель конверта переписывается после проверок SPF, DKIM, DMARC (они — по
//!   исходному), получатели — до разрешения. Заголовки письма не меняются (это — правила потока почты).

use crate::config::{AddressingConfig, RewriteRule};

/// Правила адресации секции `smtp.addressing`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Addressing {
    separators: Vec<char>,
    rules: Vec<Rule>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Rule {
    /// Адрес или `@домен`, нижний регистр.
    from: String,
    to: String,
    sender: bool,
    recipient: bool,
}

/// Сторона конверта.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// Отправитель (`MAIL FROM`).
    Sender,
    /// Получатель (`RCPT TO`).
    Recipient,
}

impl Addressing {
    /// Правила из секции.
    pub fn new(cfg: &AddressingConfig) -> Self {
        Self {
            separators: cfg.subaddress_separators.chars().collect(),
            rules: cfg.rewrite.iter().map(Rule::from).collect(),
        }
    }

    /// Адрес `address` после переписывания для стороны `side`; `None` — правила нет.
    pub fn rewrite(&self, address: &str, side: Side) -> Option<String> {
        let (local, domain) = address.rsplit_once('@')?;
        let lower = address.to_lowercase();
        let applies = |r: &&Rule| match side {
            Side::Sender => r.sender,
            Side::Recipient => r.recipient,
        };
        let by_domain = format!("@{}", domain.to_lowercase());
        let rule = self
            .rules
            .iter()
            .filter(applies)
            .find(|r| r.from == lower)
            .or_else(|| {
                self.rules
                    .iter()
                    .filter(applies)
                    .find(|r| r.from == by_domain)
            })?;
        Some(match rule.to.strip_prefix('@') {
            Some(new_domain) => format!("{local}@{new_domain}"),
            None => rule.to.clone(),
        })
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

impl From<&RewriteRule> for Rule {
    fn from(r: &RewriteRule) -> Self {
        Self {
            from: r.from.to_lowercase(),
            to: r.to.clone(),
            sender: r.sender,
            recipient: r.recipient,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(from: &str, to: &str, sender: bool, recipient: bool) -> RewriteRule {
        RewriteRule {
            from: from.into(),
            to: to.into(),
            sender,
            recipient,
        }
    }

    #[test]
    fn subaddress_and_rewrite() {
        let a = Addressing::new(&AddressingConfig {
            subaddress_separators: "+-".into(),
            rewrite: vec![
                rule("@old.example", "@new.example", true, true),
                rule("boss@old.example", "chief@new.example", true, true),
                rule("@legacy.example", "inbox@new.example", false, true),
            ],
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
        // Адрес точнее домена; домен — с сохранением локальной части.
        assert_eq!(
            a.rewrite("Boss@Old.example", Side::Sender).as_deref(),
            Some("chief@new.example")
        );
        assert_eq!(
            a.rewrite("ann@OLD.example", Side::Recipient).as_deref(),
            Some("ann@new.example")
        );
        // Правило только для получателей.
        assert_eq!(
            a.rewrite("x@legacy.example", Side::Recipient).as_deref(),
            Some("inbox@new.example")
        );
        assert_eq!(a.rewrite("x@legacy.example", Side::Sender), None);
        assert_eq!(a.rewrite("x@other.example", Side::Sender), None);
        assert_eq!(a.rewrite("", Side::Sender), None);
        // Без разделителей подадреса выключены.
        let off = Addressing::new(&AddressingConfig {
            subaddress_separators: String::new(),
            rewrite: Vec::new(),
        });
        assert_eq!(off.split("user+news@example.org"), None);
    }
}
