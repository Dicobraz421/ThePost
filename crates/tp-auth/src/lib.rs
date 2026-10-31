//! Доступ: LDAP, пароли приложений, OIDC/OAUTHBEARER, Kerberos (feature gssapi), SASL, ACL (SRS 4.5, 5.1).
//!
//! - [`ldap`] — каталог организации ([`Directory`](tp_contracts::auth::Directory)) поверх LDAPS и проверка
//!   пароля привязкой (bind) к каталогу;
//! - [`password`] — [`Authenticator`](tp_contracts::auth::Authenticator) для имени входа и пароля: пароли
//!   приложений (argon2) с отзывом и пароль каталога. Защиту от перебора (`tp-platform`, PLT-07) вызывают
//!   протоколы до проверки и по её результату.

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;
use tp_contracts::auth::Principal;
use tp_contracts::meta::{Account, AccountStore};
use tp_contracts::tls::{InternalTls, Profile};
use tp_contracts::{Error, Result, TenantId};

pub mod ldap;
pub mod password;

pub use ldap::{LdapConfig, LdapDirectory};
pub use password::{
    AppPassword, AppPasswordStore, AppPasswords, PasswordAuthenticator, PasswordCheck,
    PasswordConfig,
};

/// Группы учётной записи для `Principal::groups` (с учётом вложенности). Реализация — [`LdapDirectory`].
#[async_trait]
pub trait GroupSource: Send + Sync + 'static {
    /// Имена групп учётной записи с именем входа `login`; нет записи в каталоге — пусто.
    async fn groups_of(&self, login: &str) -> Result<BTreeSet<String>>;
}

/// Учётные записи организации для субъекта: запись в метаданных по имени входа, группы — из каталога.
#[derive(Clone)]
pub struct Accounts {
    tenant: TenantId,
    store: Arc<dyn AccountStore>,
    groups: Option<Arc<dyn GroupSource>>,
}

impl std::fmt::Debug for Accounts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Accounts")
            .field("tenant", &self.tenant)
            .field("groups", &self.groups.is_some())
            .finish_non_exhaustive()
    }
}

impl Accounts {
    /// Учётные записи организации `tenant` из `store`; без источника групп `Principal::groups` пусто.
    pub fn new(tenant: TenantId, store: Arc<dyn AccountStore>) -> Self {
        Accounts {
            tenant,
            store,
            groups: None,
        }
    }

    /// Группы субъекта — из `groups` (каталог).
    pub fn with_groups(mut self, groups: Arc<dyn GroupSource>) -> Self {
        self.groups = Some(groups);
        self
    }

    /// Организация.
    pub fn tenant(&self) -> TenantId {
        self.tenant
    }

    /// Включённая учётная запись по имени входа; нет или отключена — `Forbidden`.
    pub async fn enabled(&self, login: &str) -> Result<Account> {
        match self.store.by_login(self.tenant, login).await? {
            Some(a) if a.enabled => Ok(a),
            _ => Err(Error::Forbidden),
        }
    }

    /// Субъект учётной записи с группами из каталога.
    pub async fn principal(
        &self,
        account: Account,
        scopes: Option<BTreeSet<String>>,
    ) -> Result<Principal> {
        let groups = match &self.groups {
            Some(g) => g.groups_of(&account.login).await?,
            None => BTreeSet::new(),
        };
        Ok(Principal {
            tenant: account.tenant,
            account: account.id,
            login: account.login,
            groups,
            scopes,
        })
    }
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
pub(crate) mod testutil {
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use async_trait::async_trait;
    use tp_contracts::meta::{AccountStore, AccountUpsert};
    use tp_contracts::{AccountId, DomainId, Result, TenantId};
    use tp_testkit::{MemMetadata, SystemClock};

    use crate::{Accounts, GroupSource};

    /// Учётные записи `(login, enabled)` в `MemMetadata` организации по умолчанию.
    pub(crate) async fn accounts(list: &[(&str, bool)]) -> (Accounts, Vec<AccountId>) {
        let meta = Arc::new(MemMetadata::new(Arc::new(SystemClock)));
        let domain = DomainId::new();
        let mut ids = Vec::new();
        for (login, enabled) in list {
            let a = meta
                .upsert(
                    TenantId::DEFAULT,
                    AccountUpsert {
                        domain,
                        login: (*login).to_owned(),
                        display_name: (*login).to_owned(),
                        external_id: None,
                        enabled: *enabled,
                    },
                )
                .await
                .expect("upsert");
            ids.push(a.id);
        }
        let accounts = Accounts::new(TenantId::DEFAULT, meta).with_groups(Arc::new(StaffOnly));
        (accounts, ids)
    }

    /// Каждый — в группе `staff`.
    struct StaffOnly;

    #[async_trait]
    impl GroupSource for StaffOnly {
        async fn groups_of(&self, _: &str) -> Result<BTreeSet<String>> {
            Ok(BTreeSet::from(["staff".to_owned()]))
        }
    }
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
