//! Каталог организации в LDAP / Active Directory (SRS 5.1; CONTRACTS.md#Directory).
//!
//! Клиент — `ldap3`; канал — LDAPS (`ldaps://`) или StartTLS, открытый `ldap://` — только профиль dev с
//! `tls.allow_plaintext`. Пароль служебной учётной записи — ссылкой на секрет ([`SecretRef`]).
//!
//! Синхронизация инкрементальная ([`SyncMode`]): по `modifyTimestamp` (OpenLDAP, FreeIPA, 389-ds) или по
//! `uSNChanged` (Active Directory, Samba AD). Курсор — непрозрачная строка: позиция последней выданной записи
//! (значение счётчика и идентификатор). Удалённые записи видны только в режиме USN (контроль Show Deleted,
//! `isDeleted=TRUE`); в режиме `modifyTimestamp` их выявляет полная выгрузка (`cursor = None`).
//!
//! Active Directory:
//!
//! ```yaml
//! url: ldaps://dc1.corp.example:636
//! bind_dn: CN=thepost,OU=Service,DC=corp,DC=example
//! bind_password: { secret: "thepost/ldap#password" }
//! base_dn: DC=corp,DC=example
//! sync: usn
//! accounts:
//!   filter: (&(objectCategory=person)(objectClass=user)(mail=*))
//!   id: objectGUID
//!   login: userPrincipalName
//!   display_name: displayName
//!   emails: [mail, proxyAddresses]
//!   disabled: { attribute: userAccountControl, flag: 2 }
//!   gal: { title: title, department: department, phone: telephoneNumber }
//! groups:
//!   filter: (&(objectClass=group)(mail=*))
//!   id: objectGUID
//! domains: [{ name: corp.example, default: true }]
//! ```

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ldap3::adapters::{Adapter, EntriesOnly, PagedResults};
use ldap3::controls::RawControl;
use ldap3::{LdapConnAsync, LdapConnSettings, LdapError, Scope, SearchEntry, ldap_escape};
use rustls::ClientConfig;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tp_contracts::auth::{
    DirAccount, DirAlias, DirChange, DirCursor, DirDomain, DirGroup, DirPage, Directory,
};
use tp_contracts::config::duration;
use tp_contracts::secrets::{Secret, SecretRef, SecretStore};
use tp_contracts::tls::{InternalTls, Profile};
use tp_contracts::{Error, Result, TenantId};

use crate::{GroupSource, check_channel, plaintext_warning};

/// Контроль Active Directory «Show Deleted»: выдача удалённых записей (надгробий).
const SHOW_DELETED: &str = "1.2.840.113556.1.4.417";
/// Код результата LDAP `noSuchObject`: базы поиска нет — пустой результат.
const NO_SUCH_OBJECT: u32 = 32;
/// Код результата LDAP `invalidCredentials`.
const INVALID_CREDENTIALS: u32 = 49;

// ---------- Конфигурация ----------

/// Способ инкрементальной синхронизации.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SyncMode {
    /// По `modifyTimestamp` (OpenLDAP, FreeIPA, 389-ds): удаления — только полной выгрузкой.
    #[default]
    ModifyTimestamp,
    /// По `uSNChanged` контроллера домена (Active Directory, Samba AD), с удалёнными записями.
    Usn,
}

/// Признак отключённой учётной записи.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DisabledRule {
    /// Атрибут (`nsAccountLock`, `userAccountControl`).
    pub attribute: String,
    /// Запись отключена, если значение равно этому (без учёта регистра), например `TRUE`.
    #[serde(default)]
    pub value: Option<String>,
    /// Запись отключена, если в числовом значении установлен бит (`2` — ACCOUNTDISABLE в AD).
    #[serde(default)]
    pub flag: Option<u64>,
}

/// Сопоставление учётных записей.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AccountMap {
    /// База поиска; не задана — `base_dn`.
    pub base: Option<String>,
    /// Фильтр учётных записей.
    pub filter: String,
    /// Постоянный идентификатор (`entryUUID`, `objectGUID` — двоичный GUID AD).
    pub id: String,
    /// Имя входа.
    pub login: String,
    /// Отображаемое имя; нет значения — имя входа.
    pub display_name: String,
    /// Атрибуты адресов по порядку: первый адрес — основной. Префикс `smtp:` (AD `proxyAddresses`)
    /// снимается, значения без `@` и с другими префиксами (`X500:`, `SIP:`) пропускаются.
    pub emails: Vec<String>,
    /// Признак отключённой записи; не задан — все записи включены.
    pub disabled: Option<DisabledRule>,
    /// Атрибуты адресной книги (GAL): имя в The Post → атрибут LDAP.
    pub gal: BTreeMap<String, String>,
    /// Фильтр удалённых записей (режим USN; у надгробий AD остаются `objectClass` и идентификатор).
    pub deleted_filter: String,
}

impl Default for AccountMap {
    fn default() -> Self {
        AccountMap {
            base: None,
            filter: "(&(objectClass=inetOrgPerson)(mail=*))".into(),
            id: "entryUUID".into(),
            login: "mail".into(),
            display_name: "cn".into(),
            emails: vec!["mail".into()],
            disabled: None,
            gal: BTreeMap::new(),
            deleted_filter: "(objectClass=user)".into(),
        }
    }
}

/// Сопоставление групп.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct GroupMap {
    /// База поиска; не задана — `base_dn`.
    pub base: Option<String>,
    /// Фильтр групп.
    pub filter: String,
    /// Постоянный идентификатор.
    pub id: String,
    /// Имя группы (идентификатор в ACL).
    pub name: String,
    /// Адрес рассылки.
    pub email: String,
    /// Члены группы — DN учётных записей и вложенных групп.
    pub member: String,
    /// Фильтр удалённых групп (режим USN).
    pub deleted_filter: String,
}

impl Default for GroupMap {
    fn default() -> Self {
        GroupMap {
            base: None,
            filter: "(objectClass=groupOfNames)".into(),
            id: "entryUUID".into(),
            name: "cn".into(),
            email: "mail".into(),
            member: "member".into(),
            deleted_filter: "(objectClass=group)".into(),
        }
    }
}

/// Дополнительные адреса — записи каталога с адресом и DN получателя.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AliasMap {
    /// База поиска; не задана — `base_dn`.
    #[serde(default)]
    pub base: Option<String>,
    /// Фильтр записей-псевдонимов.
    pub filter: String,
    /// Атрибут адреса.
    pub address: String,
    /// Атрибут с DN учётной записи или группы-получателя.
    pub target: String,
}

/// Почтовый домен организации.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DomainEntry {
    /// Имя домена.
    pub name: String,
    /// Домен по умолчанию.
    #[serde(default)]
    pub default: bool,
}

/// Секция каталога LDAP (`auth.ldap`). Незаданные поля — по умолчанию (OpenLDAP / FreeIPA), неизвестные —
/// ошибка разбора. Изменения применяются новым [`LdapDirectory::connect`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct LdapConfig {
    /// Адрес: `ldaps://узел[:порт]`; `ldap://` — со `starttls: true` или в профиле dev с `tls.allow_plaintext`.
    pub url: String,
    /// StartTLS поверх `ldap://`.
    pub starttls: bool,
    /// DN служебной учётной записи для поиска; пусто — анонимный поиск.
    pub bind_dn: String,
    /// Пароль служебной учётной записи — только ссылка на секрет.
    pub bind_password: Option<SecretRef>,
    /// TLS: CA сервера (`ca_file`; не задан — системные корни), сертификат клиента, `allow_plaintext` для dev.
    pub tls: InternalTls,
    /// Корень поиска (в режиме USN — корень домена: там же контейнер удалённых записей).
    pub base_dn: String,
    /// Способ инкрементальной синхронизации.
    pub sync: SyncMode,
    /// Размер страницы поиска (Paged Results, RFC 2696).
    pub page_size: u32,
    /// Таймаут соединения.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub connect_timeout: Duration,
    /// Таймаут операции LDAP.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub timeout: Duration,
    /// Наибольшая глубина вложенности групп при вычислении групп субъекта.
    pub max_group_depth: usize,
    /// Учётные записи.
    pub accounts: AccountMap,
    /// Группы.
    pub groups: GroupMap,
    /// Дополнительные адреса; не задано — нет.
    pub aliases: Option<AliasMap>,
    /// Почтовые домены организации.
    pub domains: Vec<DomainEntry>,
}

impl Default for LdapConfig {
    fn default() -> Self {
        LdapConfig {
            url: "ldaps://127.0.0.1:636".into(),
            starttls: false,
            bind_dn: String::new(),
            bind_password: None,
            tls: InternalTls::default(),
            base_dn: String::new(),
            sync: SyncMode::default(),
            page_size: 500,
            connect_timeout: Duration::from_secs(5),
            timeout: Duration::from_secs(30),
            max_group_depth: 8,
            accounts: AccountMap::default(),
            groups: GroupMap::default(),
            aliases: None,
            domains: Vec::new(),
        }
    }
}

impl LdapConfig {
    /// Проверяет секцию (`thepost config check`, запуск). `Ok(true)` — канал открытый (только dev).
    pub fn check(&self, profile: Profile) -> Result<bool> {
        let plaintext = if self.starttls {
            check_channel("LDAP", &self.url, &["ldap"], &[], &self.tls, profile)?
        } else {
            check_channel("LDAP", &self.url, &["ldaps"], &["ldap"], &self.tls, profile)?
        };
        if self.base_dn.is_empty() {
            return Err(Error::invalid("LDAP: base_dn is empty"));
        }
        if self.bind_dn.is_empty() != self.bind_password.is_none() {
            return Err(Error::invalid(
                "LDAP: bind_dn and bind_password must be set together",
            ));
        }
        if self.page_size == 0 || i32::try_from(self.page_size).is_err() {
            return Err(Error::invalid("LDAP: invalid page_size"));
        }
        if let Some(d) = &self.accounts.disabled
            && d.value.is_some() == d.flag.is_some()
        {
            return Err(Error::invalid(
                "LDAP: accounts.disabled needs exactly one of value, flag",
            ));
        }
        Ok(plaintext)
    }
}

// ---------- Источник записей ----------

/// Поиск LDAP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Query {
    pub(crate) base: String,
    /// Только сама запись `base` (RootDSE, разрешение DN).
    pub(crate) base_only: bool,
    pub(crate) filter: String,
    pub(crate) attrs: Vec<String>,
    /// С удалёнными записями (контроль Show Deleted).
    pub(crate) show_deleted: bool,
}

/// Доступ к серверу каталога: поиск служебной учётной записью и проверка пароля привязкой.
#[async_trait]
pub(crate) trait Source: Send + Sync + 'static {
    /// Записи по запросу; нет базы поиска — пусто.
    async fn search(&self, q: &Query) -> Result<Vec<SearchEntry>>;

    /// Привязка (simple bind) от имени `dn`: `false` — неверный пароль.
    async fn bind(&self, dn: &str, password: &str) -> Result<bool>;
}

/// Сервер LDAP через `ldap3`: соединение на операцию (служебная привязка, поиск страницами, отключение).
struct Ldap3 {
    url: String,
    starttls: bool,
    bind_dn: String,
    bind_password: Option<Secret>,
    tls: Option<Arc<ClientConfig>>,
    page_size: i32,
    connect_timeout: Duration,
    timeout: Duration,
}

/// Ошибка `ldap3`: сеть, TLS, таймаут — `Unavailable` (повторяемая); ответ сервера — `Internal`.
fn unavailable(e: LdapError) -> Error {
    match e {
        LdapError::LdapResult { result } => {
            Error::Internal(format!("LDAP: rc={} {}", result.rc, result.text))
        }
        other => {
            tracing::warn!(error = %other, "LDAP unavailable");
            Error::Unavailable { retry_after: None }
        }
    }
}

impl Ldap3 {
    async fn connect(&self) -> Result<ldap3::Ldap> {
        let mut settings = LdapConnSettings::new()
            .set_conn_timeout(self.connect_timeout)
            .set_starttls(self.starttls);
        if let Some(tls) = &self.tls {
            settings = settings.set_config(tls.clone());
        }
        let (conn, ldap) = LdapConnAsync::with_settings(settings, &self.url)
            .await
            .map_err(unavailable)?;
        ldap3::drive!(conn);
        Ok(ldap)
    }

    async fn service(&self) -> Result<ldap3::Ldap> {
        let mut ldap = self.connect().await?;
        if let Some(pw) = &self.bind_password {
            let res = ldap
                .with_timeout(self.timeout)
                .simple_bind(&self.bind_dn, pw.expose())
                .await
                .map_err(unavailable)?;
            if res.rc != 0 {
                return Err(Error::Internal(format!(
                    "LDAP: service bind failed, rc={}",
                    res.rc
                )));
            }
        }
        Ok(ldap)
    }
}

#[async_trait]
impl Source for Ldap3 {
    async fn search(&self, q: &Query) -> Result<Vec<SearchEntry>> {
        let mut ldap = self.service().await?;
        let scope = if q.base_only {
            Scope::Base
        } else {
            Scope::Subtree
        };
        if q.show_deleted {
            ldap.with_controls(RawControl {
                ctype: SHOW_DELETED.into(),
                crit: true,
                val: None,
            });
        }
        // Страницы (RFC 2696) — для выборок записей; разрешение DN и надгробия — одним ответом.
        let adapters: Vec<Box<dyn Adapter<_, _>>> = if q.base_only || q.show_deleted {
            vec![Box::new(EntriesOnly::new())]
        } else {
            vec![
                Box::new(EntriesOnly::new()),
                Box::new(PagedResults::new(self.page_size)),
            ]
        };
        let mut stream = ldap
            .with_timeout(self.timeout)
            .streaming_search_with(adapters, &q.base, scope, &q.filter, q.attrs.clone())
            .await
            .map_err(unavailable)?;
        let mut out = Vec::new();
        while let Some(re) = stream.next().await.map_err(unavailable)? {
            out.push(SearchEntry::construct(re));
        }
        let res = stream.finish().await;
        let _ = ldap.unbind().await;
        match res.rc {
            0 => Ok(out),
            NO_SUCH_OBJECT => Ok(Vec::new()),
            rc => Err(Error::Internal(format!(
                "LDAP: search rc={rc} {}",
                res.text
            ))),
        }
    }

    async fn bind(&self, dn: &str, password: &str) -> Result<bool> {
        // Пустой пароль — «неаутентифицированная привязка» (RFC 4513 5.1.2): сервер ответит успехом.
        if dn.is_empty() || password.is_empty() {
            return Ok(false);
        }
        let mut ldap = self.connect().await?;
        let res = ldap
            .with_timeout(self.timeout)
            .simple_bind(dn, password)
            .await
            .map_err(unavailable)?;
        let _ = ldap.unbind().await;
        match res.rc {
            0 => Ok(true),
            INVALID_CREDENTIALS => Ok(false),
            rc => {
                tracing::info!(rc, "LDAP bind rejected");
                Ok(false)
            }
        }
    }
}

/// Конфигурация rustls для LDAPS: CA из `tls.ca_file` или системные корни, сертификат клиента для mTLS.
/// Блокирующий I/O.
fn client_tls(tls: &InternalTls) -> Result<ClientConfig> {
    let provider = rustls::crypto::CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()));
    let mut roots = rustls::RootCertStore::empty();
    match &tls.ca_file {
        Some(path) => {
            let (added, _) = roots.add_parsable_certificates(read_certs(path)?);
            if added == 0 {
                return Err(Error::Invalid(format!(
                    "LDAP: tls.ca_file {}: no usable CA certificates",
                    path.display()
                )));
            }
        }
        None => {
            let native = rustls_native_certs::load_native_certs();
            roots.add_parsable_certificates(native.certs);
        }
    }
    let builder = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::Invalid(format!("LDAP: tls: {e}")))?
        .with_root_certificates(roots);
    match (&tls.cert_file, &tls.key_file) {
        (Some(cert), Some(key)) => {
            let key = PrivateKeyDer::from_pem_file(key)
                .map_err(|e| Error::Invalid(format!("LDAP: tls.key_file: {e}")))?;
            builder
                .with_client_auth_cert(read_certs(cert)?, key)
                .map_err(|e| Error::Invalid(format!("LDAP: tls.cert_file: {e}")))
        }
        _ => Ok(builder.with_no_client_auth()),
    }
}

fn read_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    CertificateDer::pem_file_iter(path)
        .and_then(|it| it.collect::<std::result::Result<Vec<_>, _>>())
        .map_err(|e| Error::Invalid(format!("LDAP: {}: {e}", path.display())))
}

// ---------- Курсор ----------

/// Позиция синхронизации: счётчик изменений и идентификатор последней выданной записи.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
enum Mark {
    ModifyTimestamp {
        at: String,
        id: String,
    },
    Usn {
        usn: u64,
        id: String,
        /// Контроллер домена (`dsServiceName`): USN у каждого свой.
        server: String,
    },
}

/// Ключ упорядочения изменений.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    At(String, String),
    Usn(u64, String),
}

impl Mark {
    fn key(&self) -> Key {
        match self {
            Mark::ModifyTimestamp { at, id } => Key::At(at.clone(), id.clone()),
            Mark::Usn { usn, id, .. } => Key::Usn(*usn, id.clone()),
        }
    }

    fn encode(&self) -> DirCursor {
        DirCursor(serde_json::to_string(self).unwrap_or_default())
    }

    fn decode(c: &DirCursor) -> Result<Mark> {
        serde_json::from_str(&c.0).map_err(|_| Error::Conflict)
    }
}

// ---------- Каталог ----------

/// Каталог организации в LDAP: [`Directory`], проверка пароля ([`LdapDirectory::check_password`]),
/// группы субъекта ([`GroupSource`]).
#[derive(Clone)]
pub struct LdapDirectory {
    tenant: TenantId,
    cfg: Arc<LdapConfig>,
    source: Arc<dyn Source>,
}

impl fmt::Debug for LdapDirectory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LdapDirectory")
            .field("tenant", &self.tenant)
            .field("base_dn", &self.cfg.base_dn)
            .field("sync", &self.cfg.sync)
            .finish_non_exhaustive()
    }
}

/// Выборка порции: база, фильтры, атрибут идентификатора, атрибуты сопоставления.
struct Spec<'a> {
    base: &'a str,
    filter: &'a str,
    deleted_filter: &'a str,
    id_attr: &'a str,
    attrs: Vec<String>,
}

/// Изменение с ключом упорядочения.
struct Keyed<T> {
    key: Key,
    change: DirChange<T>,
}

impl LdapDirectory {
    /// Каталог организации `tenant`: проверяет секцию, читает пароль служебной учётной записи из `secrets`,
    /// готовит TLS. Соединения открываются на операцию.
    pub async fn connect(
        tenant: TenantId,
        cfg: LdapConfig,
        profile: Profile,
        secrets: &dyn SecretStore,
    ) -> Result<Self> {
        let plaintext = cfg.check(profile)?;
        if plaintext {
            tracing::warn!("{}", plaintext_warning("LDAP"));
        }
        let bind_password = match &cfg.bind_password {
            Some(r) => Some(secrets.get(r).await?),
            None => None,
        };
        let tls = if plaintext {
            None
        } else {
            let t = cfg.tls.clone();
            let c = tokio::task::spawn_blocking(move || client_tls(&t))
                .await
                .map_err(|e| Error::Internal(format!("LDAP: tls: {e}")))??;
            Some(Arc::new(c))
        };
        let source = Ldap3 {
            url: cfg.url.clone(),
            starttls: cfg.starttls,
            bind_dn: cfg.bind_dn.clone(),
            bind_password,
            tls,
            page_size: i32::try_from(cfg.page_size).unwrap_or(500),
            connect_timeout: cfg.connect_timeout,
            timeout: cfg.timeout,
        };
        Ok(Self::with_source(tenant, cfg, Arc::new(source)))
    }

    pub(crate) fn with_source(tenant: TenantId, cfg: LdapConfig, source: Arc<dyn Source>) -> Self {
        LdapDirectory {
            tenant,
            cfg: Arc::new(cfg),
            source,
        }
    }

    fn own(&self, tenant: TenantId) -> Result<()> {
        if tenant == self.tenant {
            Ok(())
        } else {
            Err(Error::NotFound)
        }
    }

    fn base<'a>(&'a self, base: &'a Option<String>) -> &'a str {
        base.as_deref().unwrap_or(&self.cfg.base_dn)
    }

    fn change_attr(&self) -> &'static str {
        match self.cfg.sync {
            SyncMode::ModifyTimestamp => "modifyTimestamp",
            SyncMode::Usn => "uSNChanged",
        }
    }

    /// Контроллер домена для курсора USN.
    async fn server(&self) -> Result<String> {
        let rows = self
            .source
            .search(&Query {
                base: String::new(),
                base_only: true,
                filter: "(objectClass=*)".into(),
                attrs: vec!["dsServiceName".into()],
                show_deleted: false,
            })
            .await?;
        Ok(rows
            .first()
            .and_then(|e| first(e, "dsServiceName"))
            .unwrap_or_default())
    }

    /// Начальная позиция и условие выборки после `cursor`.
    async fn since(&self, cursor: Option<&DirCursor>) -> Result<(Mark, Option<String>)> {
        let mark = match (cursor.map(Mark::decode).transpose()?, self.cfg.sync) {
            (None, SyncMode::ModifyTimestamp) => Mark::ModifyTimestamp {
                at: String::new(),
                id: String::new(),
            },
            (None, SyncMode::Usn) => Mark::Usn {
                usn: 0,
                id: String::new(),
                server: self.server().await?,
            },
            (Some(m @ Mark::ModifyTimestamp { .. }), SyncMode::ModifyTimestamp) => m,
            (Some(Mark::Usn { usn, id, server }), SyncMode::Usn) => {
                if server != self.server().await? {
                    return Err(Error::Conflict);
                }
                Mark::Usn { usn, id, server }
            }
            _ => return Err(Error::Conflict),
        };
        let cond = match &mark {
            Mark::ModifyTimestamp { at, .. } if at.is_empty() => None,
            Mark::ModifyTimestamp { at, .. } => {
                Some(format!("(modifyTimestamp>={})", ldap_escape(at.as_str())))
            }
            Mark::Usn { usn: 0, .. } => None,
            Mark::Usn { usn, .. } => Some(format!("(uSNChanged>={usn})")),
        };
        Ok((mark, cond))
    }

    fn key_of(&self, e: &SearchEntry, id: &str) -> Option<Key> {
        let v = first(e, self.change_attr())?;
        Some(match self.cfg.sync {
            SyncMode::ModifyTimestamp => Key::At(v, id.to_owned()),
            SyncMode::Usn => Key::Usn(v.parse().ok()?, id.to_owned()),
        })
    }

    /// Порция изменений после `cursor`: выборка по условию, упорядочение по ключу, первые `limit`.
    async fn page<T, F>(
        &self,
        cursor: Option<&DirCursor>,
        limit: usize,
        spec: Spec<'_>,
        mut map: F,
    ) -> Result<DirPage<T>>
    where
        F: FnMut(&SearchEntry, String) -> Option<T>,
    {
        if limit == 0 {
            return Err(Error::invalid("LDAP: limit is zero"));
        }
        let (mark, cond) = self.since(cursor).await?;
        let change = self.change_attr();
        let Spec {
            base,
            filter,
            deleted_filter,
            id_attr,
            mut attrs,
        } = spec;
        attrs.push(id_attr.to_owned());
        attrs.push(change.to_owned());
        let filter = match &cond {
            Some(c) => format!("(&{filter}{c})"),
            None => filter.to_owned(),
        };
        let rows = self
            .source
            .search(&Query {
                base: base.to_owned(),
                base_only: false,
                filter,
                attrs,
                show_deleted: false,
            })
            .await?;
        let mut all = Vec::with_capacity(rows.len());
        for e in &rows {
            let Some(id) = external_id(e, id_attr) else {
                tracing::warn!(dn = %e.dn, "LDAP entry without id attribute, skipped");
                continue;
            };
            let Some(key) = self.key_of(e, &id) else {
                continue;
            };
            if let Some(v) = map(e, id) {
                all.push(Keyed {
                    key,
                    change: DirChange::Upsert(v),
                });
            }
        }
        if self.cfg.sync == SyncMode::Usn {
            let cond = cond.unwrap_or_default();
            let deleted = self
                .source
                .search(&Query {
                    base: self.cfg.base_dn.clone(),
                    base_only: false,
                    filter: format!("(&(isDeleted=TRUE){deleted_filter}{cond})"),
                    attrs: vec![id_attr.to_owned(), change.to_owned()],
                    show_deleted: true,
                })
                .await?;
            for e in &deleted {
                let Some(id) = external_id(e, id_attr) else {
                    continue;
                };
                if let Some(key) = self.key_of(e, &id) {
                    all.push(Keyed {
                        key,
                        change: DirChange::Removed { external_id: id },
                    });
                }
            }
        }
        let after = mark.key();
        all.retain(|k| k.key > after);
        all.sort_by(|a, b| a.key.cmp(&b.key));
        let has_more = all.len() > limit;
        all.truncate(limit);
        let next = match (all.last().map(|k| &k.key), mark) {
            (Some(Key::At(at, id)), _) => Mark::ModifyTimestamp {
                at: at.clone(),
                id: id.clone(),
            },
            (Some(Key::Usn(usn, id)), Mark::Usn { server, .. }) => Mark::Usn {
                usn: *usn,
                id: id.clone(),
                server,
            },
            (_, mark) => mark,
        };
        Ok(DirPage {
            changes: all.into_iter().map(|k| k.change).collect(),
            next: next.encode(),
            has_more,
        })
    }

    fn account(&self, e: &SearchEntry, external_id: String) -> Option<DirAccount> {
        let m = &self.cfg.accounts;
        let Some(login) = first(e, &m.login) else {
            tracing::warn!(dn = %e.dn, "LDAP account without login attribute, skipped");
            return None;
        };
        let mut emails: Vec<String> = Vec::new();
        for attr in &m.emails {
            for v in values(e, attr) {
                let addr = match v.split_once(':') {
                    Some((p, a)) if p.eq_ignore_ascii_case("smtp") => a,
                    Some(_) => continue,
                    None => v,
                };
                if addr.contains('@') && !emails.iter().any(|x| x.eq_ignore_ascii_case(addr)) {
                    emails.push(addr.to_owned());
                }
            }
        }
        let enabled = match &m.disabled {
            None => true,
            Some(rule) => !disabled(e, rule),
        };
        let attributes = m
            .gal
            .iter()
            .filter_map(|(name, attr)| first(e, attr).map(|v| (name.clone(), v)))
            .collect();
        Some(DirAccount {
            external_id,
            display_name: first(e, &m.display_name).unwrap_or_else(|| login.clone()),
            login,
            emails,
            enabled,
            attributes,
        })
    }

    fn account_attrs(&self) -> Vec<String> {
        let m = &self.cfg.accounts;
        let mut a = vec![m.login.clone(), m.display_name.clone()];
        a.extend(m.emails.iter().cloned());
        a.extend(m.disabled.iter().map(|d| d.attribute.clone()));
        a.extend(m.gal.values().cloned());
        a
    }

    /// Идентификаторы записей по DN (члены групп, получатели псевдонимов); неразрешённые DN пропускаются.
    async fn ids_of(
        &self,
        dns: impl IntoIterator<Item = String>,
        cache: &mut HashMap<String, Option<String>>,
    ) -> Result<()> {
        for dn in dns {
            let key = dn.to_ascii_lowercase();
            if cache.contains_key(&key) {
                continue;
            }
            // Идентификатор группы и учётной записи может называться по-разному: запрашиваем оба.
            let attrs = vec![self.cfg.accounts.id.clone(), self.cfg.groups.id.clone()];
            let rows = self
                .source
                .search(&Query {
                    base: dn.clone(),
                    base_only: true,
                    filter: "(objectClass=*)".into(),
                    attrs,
                    show_deleted: false,
                })
                .await?;
            let id = rows.first().and_then(|e| {
                external_id(e, &self.cfg.accounts.id)
                    .or_else(|| external_id(e, &self.cfg.groups.id))
            });
            cache.insert(key, id);
        }
        Ok(())
    }

    /// DN учётной записи по имени входа; нет или неоднозначно — `None`.
    async fn find_dn(&self, login: &str) -> Result<Option<String>> {
        if login.is_empty() {
            return Ok(None);
        }
        let m = &self.cfg.accounts;
        let rows = self
            .source
            .search(&Query {
                base: self.base(&m.base).to_owned(),
                base_only: false,
                filter: format!("(&{}({}={}))", m.filter, m.login, ldap_escape(login)),
                attrs: vec![m.id.clone()],
                show_deleted: false,
            })
            .await?;
        match rows.as_slice() {
            [one] => Ok(Some(one.dn.clone())),
            [] => Ok(None),
            _ => {
                tracing::warn!("LDAP: login matches several entries, sign-in refused");
                Ok(None)
            }
        }
    }

    /// Проверка пароля привязкой к каталогу от имени учётной записи с именем входа `login`.
    /// `false` — нет записи, запись неоднозначна, неверный или пустой пароль; сеть — `Unavailable`.
    pub async fn check_password(&self, login: &str, password: &str) -> Result<bool> {
        if password.is_empty() {
            return Ok(false);
        }
        match self.find_dn(login).await? {
            Some(dn) => self.source.bind(&dn, password).await,
            None => Ok(false),
        }
    }
}

#[async_trait]
impl Directory for LdapDirectory {
    async fn accounts_since(
        &self,
        tenant: TenantId,
        cursor: Option<&DirCursor>,
        limit: usize,
    ) -> Result<DirPage<DirAccount>> {
        self.own(tenant)?;
        let m = &self.cfg.accounts;
        let spec = Spec {
            base: self.base(&m.base),
            filter: &m.filter,
            deleted_filter: &m.deleted_filter,
            id_attr: &m.id,
            attrs: self.account_attrs(),
        };
        self.page(cursor, limit, spec, |e, id| self.account(e, id))
            .await
    }

    async fn groups_since(
        &self,
        tenant: TenantId,
        cursor: Option<&DirCursor>,
        limit: usize,
    ) -> Result<DirPage<DirGroup>> {
        self.own(tenant)?;
        let m = &self.cfg.groups;
        let spec = Spec {
            base: self.base(&m.base),
            filter: &m.filter,
            deleted_filter: &m.deleted_filter,
            id_attr: &m.id,
            attrs: vec![m.name.clone(), m.email.clone(), m.member.clone()],
        };
        let mut members: Vec<(String, Vec<String>)> = Vec::new();
        let mut page = self
            .page(cursor, limit, spec, |e, id| {
                let name = first(e, &m.name)?;
                members.push((
                    id.clone(),
                    values(e, &m.member).map(str::to_owned).collect(),
                ));
                Some(DirGroup {
                    external_id: id,
                    name,
                    email: first(e, &m.email),
                    members: Vec::new(),
                })
            })
            .await?;
        let mut cache = HashMap::new();
        let wanted: HashSet<&str> = page
            .changes
            .iter()
            .filter_map(|c| match c {
                DirChange::Upsert(g) => Some(g.external_id.as_str()),
                DirChange::Removed { .. } => None,
            })
            .collect();
        let dns: Vec<String> = members
            .iter()
            .filter(|(id, _)| wanted.contains(id.as_str()))
            .flat_map(|(_, dns)| dns.iter().cloned())
            .collect();
        self.ids_of(dns, &mut cache).await?;
        let by_group: HashMap<String, Vec<String>> = members.into_iter().collect();
        for c in &mut page.changes {
            if let DirChange::Upsert(g) = c {
                g.members = by_group
                    .get(&g.external_id)
                    .into_iter()
                    .flatten()
                    .filter_map(|dn| cache.get(&dn.to_ascii_lowercase()).cloned().flatten())
                    .collect();
            }
        }
        Ok(page)
    }

    async fn aliases(&self, tenant: TenantId) -> Result<Vec<DirAlias>> {
        self.own(tenant)?;
        let Some(m) = &self.cfg.aliases else {
            return Ok(Vec::new());
        };
        let rows = self
            .source
            .search(&Query {
                base: self.base(&m.base).to_owned(),
                base_only: false,
                filter: m.filter.clone(),
                attrs: vec![m.address.clone(), m.target.clone()],
                show_deleted: false,
            })
            .await?;
        let mut cache = HashMap::new();
        let dns: Vec<String> = rows
            .iter()
            .flat_map(|e| values(e, &m.target))
            .map(str::to_owned)
            .collect();
        self.ids_of(dns, &mut cache).await?;
        let mut out = Vec::new();
        for e in &rows {
            let targets: Vec<String> = values(e, &m.target)
                .filter_map(|dn| cache.get(&dn.to_ascii_lowercase()).cloned().flatten())
                .collect();
            for address in values(e, &m.address) {
                for target in &targets {
                    out.push(DirAlias {
                        address: address.to_owned(),
                        target: target.clone(),
                    });
                }
            }
        }
        Ok(out)
    }

    async fn domains(&self, tenant: TenantId) -> Result<Vec<DirDomain>> {
        self.own(tenant)?;
        Ok(self
            .cfg
            .domains
            .iter()
            .map(|d| DirDomain {
                name: d.name.to_ascii_lowercase(),
                default: d.default,
            })
            .collect())
    }

    async fn gal_attributes(&self, tenant: TenantId) -> Result<Vec<String>> {
        self.own(tenant)?;
        Ok(self.cfg.accounts.gal.keys().cloned().collect())
    }
}

#[async_trait]
impl GroupSource for LdapDirectory {
    async fn groups_of(&self, login: &str) -> Result<BTreeSet<String>> {
        let mut names = BTreeSet::new();
        let Some(dn) = self.find_dn(login).await? else {
            return Ok(names);
        };
        let m = &self.cfg.groups;
        let mut seen: HashSet<String> = HashSet::from([dn.to_ascii_lowercase()]);
        let mut frontier = vec![dn];
        for _ in 0..self.cfg.max_group_depth {
            let mut next = Vec::new();
            for dn in &frontier {
                let rows = self
                    .source
                    .search(&Query {
                        base: self.base(&m.base).to_owned(),
                        base_only: false,
                        filter: format!(
                            "(&{}({}={}))",
                            m.filter,
                            m.member,
                            ldap_escape(dn.as_str())
                        ),
                        attrs: vec![m.name.clone()],
                        show_deleted: false,
                    })
                    .await?;
                for g in rows {
                    if let Some(name) = first(&g, &m.name) {
                        names.insert(name);
                    }
                    if seen.insert(g.dn.to_ascii_lowercase()) {
                        next.push(g.dn);
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
        Ok(names)
    }
}

// ---------- Атрибуты записи ----------

/// Значения атрибута (имя без учёта регистра).
fn values<'a>(e: &'a SearchEntry, attr: &str) -> impl Iterator<Item = &'a str> {
    e.attrs
        .iter()
        .filter(move |(k, _)| k.eq_ignore_ascii_case(attr))
        .flat_map(|(_, v)| v.iter().map(String::as_str))
}

fn first(e: &SearchEntry, attr: &str) -> Option<String> {
    values(e, attr).find(|v| !v.is_empty()).map(str::to_owned)
}

/// Идентификатор записи: `objectGUID` AD — двоичный GUID в строку, иначе первое значение.
fn external_id(e: &SearchEntry, attr: &str) -> Option<String> {
    if attr.eq_ignore_ascii_case("objectGUID") {
        let bin = e
            .bin_attrs
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case(attr))
            .flat_map(|(_, v)| v.iter().cloned())
            .chain(values(e, attr).map(|s| s.as_bytes().to_vec()))
            .next()?;
        return guid(&bin);
    }
    first(e, attr)
}

/// GUID Active Directory (первые три поля — little-endian) в каноническом виде.
fn guid(b: &[u8]) -> Option<String> {
    let b: &[u8; 16] = b.try_into().ok()?;
    Some(format!(
        "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        u16::from_le_bytes([b[4], b[5]]),
        u16::from_le_bytes([b[6], b[7]]),
        b[8],
        b[9],
        b[10],
        b[11],
        b[12],
        b[13],
        b[14],
        b[15]
    ))
}

fn disabled(e: &SearchEntry, rule: &DisabledRule) -> bool {
    values(e, &rule.attribute).any(|v| match (&rule.value, rule.flag) {
        (Some(want), _) => v.eq_ignore_ascii_case(want),
        (None, Some(flag)) => v
            .trim()
            .parse::<i64>()
            .is_ok_and(|n| (n as u64) & flag != 0),
        (None, None) => false,
    })
}

#[cfg(test)]
mod tests;
