//! Доступ: LDAP, пароли приложений, OIDC/OAUTHBEARER, Kerberos (feature gssapi), SASL, ACL (SRS 4.5, 5.1).
//!
//! - [`ldap`] — каталог организации ([`Directory`](tp_contracts::auth::Directory)) поверх LDAPS и проверка
//!   пароля привязкой (bind) к каталогу.

use std::collections::BTreeSet;

use async_trait::async_trait;
use tp_contracts::tls::{InternalTls, Profile};
use tp_contracts::{Error, Result};

pub mod ldap;

pub use ldap::{LdapConfig, LdapDirectory};

/// Группы учётной записи для `Principal::groups` (с учётом вложенности). Реализация — [`LdapDirectory`].
#[async_trait]
pub trait GroupSource: Send + Sync + 'static {
    /// Имена групп учётной записи с именем входа `login`; нет записи в каталоге — пусто.
    async fn groups_of(&self, login: &str) -> Result<BTreeSet<String>>;
}

/// Проверка канала к внешней службе (LDAP, поставщик OIDC): адрес со схемой из `secure` — TLS; из `plain` —
/// открытый, только в профиле dev с `tls.allow_plaintext`. `Ok(true)` — канал открытый: вызывающий пишет
/// [`plaintext_warning`] в лог.
pub(crate) fn check_channel(
    what: &str,
    url: &str,
    secure: &[&str],
    plain: &[&str],
    tls: &InternalTls,
    profile: Profile,
) -> Result<bool> {
    if tls.allow_plaintext && profile != Profile::Dev {
        return Err(Error::Invalid(format!(
            "{what}: tls.allow_plaintext is allowed only with profile: dev"
        )));
    }
    if tls.cert_file.is_some() != tls.key_file.is_some() {
        return Err(Error::Invalid(format!(
            "{what}: tls.cert_file and tls.key_file must be set together"
        )));
    }
    if tls.server_name.is_some() {
        return Err(Error::Invalid(format!(
            "{what}: tls.server_name is not supported; the host from the address is verified"
        )));
    }
    let scheme = url
        .split_once("://")
        .map(|(s, _)| s.to_ascii_lowercase())
        .ok_or_else(|| Error::Invalid(format!("{what}: address without scheme")))?;
    if secure.contains(&scheme.as_str()) {
        Ok(false)
    } else if plain.contains(&scheme.as_str()) {
        if tls.allow_plaintext {
            Ok(true)
        } else {
            Err(Error::Invalid(format!(
                "{what}: address without TLS; use {}:// (plaintext only with profile: dev and tls.allow_plaintext)",
                secure.first().copied().unwrap_or("https")
            )))
        }
    } else {
        Err(Error::Invalid(format!(
            "{what}: unsupported address scheme {scheme}://"
        )))
    }
}

/// Предупреждение для лога об открытом канале (без адреса: в нём могут быть учётные данные).
pub(crate) fn plaintext_warning(what: &str) -> String {
    format!(
        "channel {what} is NOT encrypted (tls.allow_plaintext, profile dev); never use in production"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_tls() {
        let tls = InternalTls::default();
        let (s, p) = (["ldaps"], ["ldap"]);
        assert_eq!(
            check_channel("LDAP", "ldaps://dc:636", &s, &p, &tls, Profile::Prod),
            Ok(false)
        );
        assert!(check_channel("LDAP", "ldap://dc", &s, &p, &tls, Profile::Prod).is_err());
        assert!(check_channel("LDAP", "http://dc", &s, &p, &tls, Profile::Prod).is_err());
        assert!(check_channel("LDAP", "dc", &s, &p, &tls, Profile::Prod).is_err());
        let dev = InternalTls {
            allow_plaintext: true,
            ..InternalTls::default()
        };
        assert_eq!(
            check_channel("LDAP", "ldap://dc", &s, &p, &dev, Profile::Dev),
            Ok(true)
        );
        assert!(check_channel("LDAP", "ldap://dc", &s, &p, &dev, Profile::Prod).is_err());
    }
}
