//! Фейки Ф1 для протокольных полос (smtp, imap, jmap, admin): каталог, аутентификация, SASL, поиск,
//! манифест индекса, очередь исходящей почты, корзина, квоты, точка принятия решений.
//!
//! Клоны разделяют состояние (`Arc` внутри). Тестовые данные задаются методами фейков (`add_*`, `set_*`, `index`).
//!
//! ```
//! # futures::executor::block_on(async {
//! use std::sync::Arc;
//! use tp_contracts::sasl::{Mechanism, SaslContext, SaslServer, SaslStep};
//! use tp_contracts::TenantId;
//! use tp_testkit::{FakeAuthenticator, FakeSasl, principal};
//!
//! let auth = FakeAuthenticator::default();
//! let p = principal(TenantId::DEFAULT, "user@example.com");
//! auth.add_password("user@example.com", "secret", p.clone());
//! let sasl = FakeSasl::new(Arc::new(auth));
//! let ctx = SaslContext { service: "smtp", tls: true };
//! let mut s = sasl.start(Mechanism::Plain, &ctx).expect("start");
//! let step = s.step(Some(b"\0user@example.com\0secret")).await.expect("step");
//! assert_eq!(step, SaslStep::Done(p));
//! # });
//! ```

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use mail_parser::{Address, MessageParser, MimeHeaders};
use time::OffsetDateTime;
use tp_contracts::auth::{
    Authenticator, Credentials, DirAccount, DirAlias, DirChange, DirCursor, DirDomain, DirGroup,
    DirPage, Directory, Principal,
};
use tp_contracts::ext::DecisionHooks;
use tp_contracts::ext::{Decision, DecisionHook, DeliveryInfo, FinalDelete, SubmissionInfo};
use tp_contracts::meta::{AccountScope, AccountStore, MessageStore, PolicyResolver, QuotaUsage};
use tp_contracts::queue::{
    Claimed, OutMessage, OutRcpt, QueueClaim, QueueStore, QueuedRcpt, RcptStatus,
};
use tp_contracts::retention::{
    PurgeOutcome, Quota, Restored, TrashItem, TrashObject, TrashPurge, TrashStore,
};
use tp_contracts::sasl::{Mechanism, SaslContext, SaslFailure, SaslServer, SaslSession, SaslStep};
use tp_contracts::search::{
    IndexManifest, IndexManifestStore, ManifestUpdate, RebuildRequest, SearchIndex, SearchSort,
    Snippet, TextField, TextQuery,
};
use tp_contracts::{AccountId, BlobKey, Error, MailboxId, MessageId, QueueId, Result, TenantId};

use crate::infra::{Clock, lock};
use crate::meta::{MemMetadata, TrashRow};
use crate::policies::MemPolicies;

// ---------- Authenticator ----------

/// Субъект без групп и ограничений областей доступа (хелпер тестов).
pub fn principal(tenant: TenantId, login: &str) -> Principal {
    Principal {
        tenant,
        account: AccountId::new(),
        login: login.to_owned(),
        groups: BTreeSet::new(),
        scopes: None,
    }
}

#[derive(Debug, Default)]
struct AuthState {
    /// Имя входа (нижний регистр) → пароль и субъект.
    passwords: HashMap<String, (String, Principal)>,
    tokens: HashMap<String, Principal>,
    kerberos: HashMap<String, Principal>,
    failure: Option<Error>,
}

/// [`Authenticator`] по заданным паролям, токенам и принципалам Kerberos. Неизвестные или неверные
/// данные — `Forbidden`; [`FakeAuthenticator::set_failure`] — ошибка каждого вызова (`Unavailable`).
#[derive(Debug, Clone, Default)]
pub struct FakeAuthenticator(Arc<Mutex<AuthState>>);

impl FakeAuthenticator {
    /// Пароль для имени входа (без учёта регистра); повтор заменяет прежний.
    pub fn add_password(&self, login: &str, secret: &str, principal: Principal) {
        lock(&self.0)
            .passwords
            .insert(login.to_lowercase(), (secret.to_owned(), principal));
    }

    /// Действующий токен доступа OAuth.
    pub fn add_token(&self, token: &str, principal: Principal) {
        lock(&self.0).tokens.insert(token.to_owned(), principal);
    }

    /// Принципал Kerberos (`user@REALM`).
    pub fn add_kerberos(&self, name: &str, principal: Principal) {
        lock(&self.0).kerberos.insert(name.to_owned(), principal);
    }

    /// Отзывает пароль и токены субъекта с именем входа `login` (запись отключена).
    pub fn disable(&self, login: &str) {
        let mut st = lock(&self.0);
        st.passwords.remove(&login.to_lowercase());
        st.tokens
            .retain(|_, p| !p.login.eq_ignore_ascii_case(login));
        st.kerberos
            .retain(|_, p| !p.login.eq_ignore_ascii_case(login));
    }

    /// Каждый `verify` возвращает `err` (недоступность каталога); `None` — снять.
    pub fn set_failure(&self, err: Option<Error>) {
        lock(&self.0).failure = err;
    }
}

#[async_trait]
impl Authenticator for FakeAuthenticator {
    async fn verify(&self, credentials: Credentials) -> Result<Principal> {
        let st = lock(&self.0);
        if let Some(e) = &st.failure {
            return Err(e.clone());
        }
        let found = match &credentials {
            Credentials::Password { login, secret } => st
                .passwords
                .get(&login.to_lowercase())
                .filter(|(s, _)| s == secret)
                .map(|(_, p)| p),
            Credentials::Bearer(token) => st.tokens.get(token),
            Credentials::Gssapi { principal, authzid } => st.kerberos.get(principal).filter(|p| {
                authzid
                    .as_ref()
                    .is_none_or(|a| a.eq_ignore_ascii_case(&p.login))
            }),
        };
        found.cloned().ok_or(Error::Forbidden)
    }
}

// ---------- Directory ----------

/// Записи одного вида с порядковыми номерами изменений (как USN LDAP).
#[derive(Debug)]
struct DirLog<T> {
    seq: u64,
    /// Курсоры меньше — устарели (`Conflict`).
    floor: u64,
    /// `external_id` → номер последнего изменения и запись (`None` — удалена).
    items: HashMap<String, (u64, Option<T>)>,
}

impl<T> Default for DirLog<T> {
    fn default() -> Self {
        Self {
            seq: 0,
            floor: 0,
            items: HashMap::new(),
        }
    }
}

impl<T: Clone> DirLog<T> {
    fn set(&mut self, id: String, item: Option<T>) {
        self.seq += 1;
        self.items.insert(id, (self.seq, item));
    }

    fn page(&self, cursor: Option<&DirCursor>, limit: usize) -> Result<DirPage<T>> {
        let since = match cursor {
            None => 0,
            Some(c) => {
                c.0.parse::<u64>()
                    .ok()
                    .filter(|s| *s >= self.floor && *s <= self.seq)
                    .ok_or(Error::Conflict)?
            }
        };
        let mut rows: Vec<_> = self
            .items
            .iter()
            .filter(|(_, (seq, item))| *seq > since && (cursor.is_some() || item.is_some()))
            .collect();
        rows.sort_unstable_by_key(|(_, (seq, _))| *seq);
        let has_more = rows.len() > limit;
        rows.truncate(limit);
        let next = match rows.last() {
            Some((_, (seq, _))) if has_more => *seq,
            _ if has_more => since,
            _ => self.seq,
        };
        let changes = rows
            .into_iter()
            .map(|(id, (_, item))| match item {
                Some(v) => DirChange::Upsert(v.clone()),
                None => DirChange::Removed {
                    external_id: id.clone(),
                },
            })
            .collect();
        Ok(DirPage {
            changes,
            next: DirCursor(next.to_string()),
            has_more,
        })
    }

    /// Забывает удаления: прежние курсоры устаревают.
    fn compact(&mut self) {
        self.floor = self.seq;
        self.items.retain(|_, (_, item)| item.is_some());
    }
}

#[derive(Debug, Default)]
struct TenantDir {
    accounts: DirLog<DirAccount>,
    groups: DirLog<DirGroup>,
    aliases: Vec<DirAlias>,
    domains: Vec<DirDomain>,
    gal: Vec<String>,
}

/// [`Directory`] в памяти. Курсор — номер последнего изменения; полная выгрузка (`cursor = None`) — без удалений,
/// продолжение — с ними. [`FakeDirectory::expire_cursors`] — выданные курсоры устаревают (`Conflict`).
#[derive(Debug, Clone, Default)]
pub struct FakeDirectory(Arc<Mutex<HashMap<TenantId, TenantDir>>>);

impl FakeDirectory {
    fn with<R>(&self, tenant: TenantId, f: impl FnOnce(&mut TenantDir) -> R) -> R {
        f(lock(&self.0).entry(tenant).or_default())
    }

    /// Создаёт или изменяет учётную запись (по `external_id`).
    pub fn upsert_account(&self, tenant: TenantId, account: DirAccount) {
        self.with(tenant, |d| {
            d.accounts.set(account.external_id.clone(), Some(account));
        });
    }

    /// Удаляет учётную запись.
    pub fn remove_account(&self, tenant: TenantId, external_id: &str) {
        self.with(tenant, |d| d.accounts.set(external_id.to_owned(), None));
    }

    /// Создаёт или изменяет группу (по `external_id`).
    pub fn upsert_group(&self, tenant: TenantId, group: DirGroup) {
        self.with(tenant, |d| {
            d.groups.set(group.external_id.clone(), Some(group));
        });
    }

    /// Удаляет группу.
    pub fn remove_group(&self, tenant: TenantId, external_id: &str) {
        self.with(tenant, |d| d.groups.set(external_id.to_owned(), None));
    }

    /// Дополнительные адреса (заменяет список).
    pub fn set_aliases(&self, tenant: TenantId, aliases: Vec<DirAlias>) {
        self.with(tenant, |d| d.aliases = aliases);
    }

    /// Почтовые домены (заменяет список).
    pub fn set_domains(&self, tenant: TenantId, domains: Vec<DirDomain>) {
        self.with(tenant, |d| d.domains = domains);
    }

    /// Атрибуты адресной книги (заменяет список).
    pub fn set_gal_attributes(&self, tenant: TenantId, names: Vec<String>) {
        self.with(tenant, |d| d.gal = names);
    }

    /// Выданные курсоры организации устаревают: следующий запрос с ними — `Conflict`.
    pub fn expire_cursors(&self, tenant: TenantId) {
        self.with(tenant, |d| {
            d.accounts.compact();
            d.groups.compact();
        });
    }
}

#[async_trait]
impl Directory for FakeDirectory {
    async fn accounts_since(
        &self,
        tenant: TenantId,
        cursor: Option<&DirCursor>,
        limit: usize,
    ) -> Result<DirPage<DirAccount>> {
        self.with(tenant, |d| d.accounts.page(cursor, limit))
    }

    async fn groups_since(
        &self,
        tenant: TenantId,
        cursor: Option<&DirCursor>,
        limit: usize,
    ) -> Result<DirPage<DirGroup>> {
        self.with(tenant, |d| d.groups.page(cursor, limit))
    }

    async fn aliases(&self, tenant: TenantId) -> Result<Vec<DirAlias>> {
        Ok(self.with(tenant, |d| d.aliases.clone()))
    }

    async fn domains(&self, tenant: TenantId) -> Result<Vec<DirDomain>> {
        Ok(self.with(tenant, |d| d.domains.clone()))
    }

    async fn gal_attributes(&self, tenant: TenantId) -> Result<Vec<String>> {
        Ok(self.with(tenant, |d| d.gal.clone()))
    }
}

// ---------- SASL ----------

/// [`SaslServer`] поверх [`Authenticator`]: `PLAIN` (RFC 4616) и `OAUTHBEARER` (RFC 7628).
/// Без TLS механизмы не предлагаются ([`Mechanism::plaintext`]).
#[derive(Clone)]
pub struct FakeSasl {
    auth: Arc<dyn Authenticator>,
    mechanisms: Vec<Mechanism>,
}

impl std::fmt::Debug for FakeSasl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeSasl")
            .field("mechanisms", &self.mechanisms)
            .finish_non_exhaustive()
    }
}

impl FakeSasl {
    /// `PLAIN` и `OAUTHBEARER` поверх `auth`.
    pub fn new(auth: Arc<dyn Authenticator>) -> Self {
        Self {
            auth,
            mechanisms: vec![Mechanism::Plain, Mechanism::OauthBearer],
        }
    }

    /// Предлагать только `mechanisms` (из поддерживаемых `PLAIN`, `OAUTHBEARER`).
    pub fn with_mechanisms(mut self, mechanisms: &[Mechanism]) -> Self {
        self.mechanisms = mechanisms
            .iter()
            .copied()
            .filter(|m| matches!(m, Mechanism::Plain | Mechanism::OauthBearer))
            .collect();
        self
    }
}

impl SaslServer for FakeSasl {
    fn mechanisms(&self, tls: bool) -> Vec<Mechanism> {
        self.mechanisms
            .iter()
            .copied()
            .filter(|m| tls || !m.plaintext())
            .collect()
    }

    fn start(&self, mechanism: Mechanism, ctx: &SaslContext) -> Result<Box<dyn SaslSession>> {
        if !self.mechanisms(ctx.tls).contains(&mechanism) {
            return Err(Error::invalid(format!(
                "SASL mechanism not available: {mechanism}"
            )));
        }
        Ok(Box::new(FakeSaslSession {
            auth: self.auth.clone(),
            mechanism,
            state: SessionState::Initial,
        }))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionState {
    Initial,
    Waiting,
    /// OAUTHBEARER: отправлен вызов с ошибкой, ждём `%x01` клиента.
    OauthError,
    Finished,
}

struct FakeSaslSession {
    auth: Arc<dyn Authenticator>,
    mechanism: Mechanism,
    state: SessionState,
}

/// Ответ сервера OAUTHBEARER на неверный токен (RFC 7628, 3.2.2).
const OAUTH_ERROR: &[u8] = br#"{"status":"invalid_token"}"#;

impl FakeSaslSession {
    async fn verify(&mut self, creds: Credentials, authzid: Option<&str>) -> Result<SaslStep> {
        match self.auth.verify(creds).await {
            Ok(p) => {
                self.state = SessionState::Finished;
                if authzid.is_some_and(|a| !a.eq_ignore_ascii_case(&p.login)) {
                    Ok(SaslStep::Fail(SaslFailure::Authorization))
                } else {
                    Ok(SaslStep::Done(p))
                }
            }
            Err(Error::Forbidden) if self.mechanism == Mechanism::OauthBearer => {
                self.state = SessionState::OauthError;
                Ok(SaslStep::Challenge(OAUTH_ERROR.to_vec()))
            }
            Err(Error::Forbidden) => {
                self.state = SessionState::Finished;
                Ok(SaslStep::Fail(SaslFailure::Authentication))
            }
            Err(e) => {
                self.state = SessionState::Finished;
                Err(e)
            }
        }
    }

    async fn plain(&mut self, msg: &[u8]) -> Result<SaslStep> {
        let malformed = || Error::invalid("PLAIN: malformed message");
        let mut it = msg.split(|b| *b == 0);
        let (Some(authzid), Some(login), Some(secret), None) =
            (it.next(), it.next(), it.next(), it.next())
        else {
            return Err(malformed());
        };
        let text = |b: &[u8]| {
            std::str::from_utf8(b)
                .map(str::to_owned)
                .map_err(|_| malformed())
        };
        let (authzid, login, secret) = (text(authzid)?, text(login)?, text(secret)?);
        if login.is_empty() {
            return Err(malformed());
        }
        let authzid = (!authzid.is_empty()).then_some(authzid);
        self.verify(Credentials::Password { login, secret }, authzid.as_deref())
            .await
    }

    async fn oauthbearer(&mut self, msg: &[u8]) -> Result<SaslStep> {
        let malformed = || Error::invalid("OAUTHBEARER: malformed message");
        let msg = std::str::from_utf8(msg).map_err(|_| malformed())?;
        // gs2-header %x01 *(kvpair %x01) %x01; gs2-header = ("n" | "y") "," ["a=" authzid] ","
        let (gs2, rest) = msg.split_once('\x01').ok_or_else(malformed)?;
        let mut gs2 = gs2.split(',');
        let (Some("n" | "y"), Some(a), Some(""), None) =
            (gs2.next(), gs2.next(), gs2.next(), gs2.next())
        else {
            return Err(malformed());
        };
        let authzid = match a {
            "" => None,
            a => Some(a.strip_prefix("a=").ok_or_else(malformed)?.to_owned()),
        };
        let token = rest
            .split('\x01')
            .find_map(|kv| kv.strip_prefix("auth="))
            .and_then(|v| {
                let (scheme, token) = v.split_once(' ')?;
                scheme.eq_ignore_ascii_case("bearer").then_some(token)
            })
            .filter(|t| !t.is_empty())
            .ok_or_else(malformed)?;
        self.verify(Credentials::Bearer(token.to_owned()), authzid.as_deref())
            .await
    }
}

#[async_trait]
impl SaslSession for FakeSaslSession {
    async fn step(&mut self, response: Option<&[u8]>) -> Result<SaslStep> {
        match (self.state, response) {
            (SessionState::Initial, None) => {
                self.state = SessionState::Waiting;
                Ok(SaslStep::Challenge(Vec::new()))
            }
            (SessionState::Initial | SessionState::Waiting, Some(msg)) => match self.mechanism {
                Mechanism::OauthBearer => self.oauthbearer(msg).await,
                _ => self.plain(msg).await,
            },
            (SessionState::OauthError, _) => {
                self.state = SessionState::Finished;
                Ok(SaslStep::Fail(SaslFailure::Authentication))
            }
            (SessionState::Waiting, None) => Err(Error::invalid("SASL: response expected")),
            (SessionState::Finished, _) => Err(Error::invalid("SASL: exchange finished")),
        }
    }
}

// ---------- SearchIndex ----------

/// Письмо в [`FakeSearch`]: тексты полей.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SearchDoc {
    /// Письмо.
    pub message: MessageId,
    /// Папка.
    pub mailbox: MailboxId,
    /// Момент приёма (порядок результатов).
    pub received_at: Option<OffsetDateTime>,
    /// Тема.
    pub subject: String,
    /// `From`.
    pub from: String,
    /// `To`.
    pub to: String,
    /// `Cc`.
    pub cc: String,
    /// `Bcc`.
    pub bcc: String,
    /// Текст письма.
    pub body: String,
    /// Имена и текст вложений.
    pub attachments: String,
    /// Заголовки: имя и значение (без декодирования).
    pub headers: Vec<(String, String)>,
}

fn addresses(a: Option<&Address<'_>>) -> String {
    a.map(|a| {
        a.iter()
            .map(|x| match (x.name(), x.address()) {
                (Some(n), Some(e)) => format!("{n} <{e}>"),
                (n, e) => n.or(e).unwrap_or_default().to_owned(),
            })
            .collect::<Vec<_>>()
            .join(", ")
    })
    .unwrap_or_default()
}

impl SearchDoc {
    /// Разбирает письмо RFC 5322 (`mail-parser`); не разбирается — `Error::Invalid`.
    pub fn parse(
        message: MessageId,
        mailbox: MailboxId,
        received_at: OffsetDateTime,
        raw: &[u8],
    ) -> Result<SearchDoc> {
        let msg = MessageParser::default()
            .parse(raw)
            .ok_or_else(|| Error::invalid("message does not parse"))?;
        let headers = msg
            .headers()
            .iter()
            .map(|h| {
                let value = raw
                    .get(h.offset_start() as usize..h.offset_end() as usize)
                    .map(String::from_utf8_lossy)
                    .unwrap_or_default();
                (h.name().to_owned(), value.trim().to_owned())
            })
            .collect();
        let body = (0..)
            .map_while(|i| msg.body_text(i))
            .collect::<Vec<_>>()
            .join("\n");
        let attachments = msg
            .attachments()
            .map(|a| {
                let name = a.attachment_name().unwrap_or_default();
                match a.text_contents() {
                    Some(t) => format!("{name}\n{t}"),
                    None => name.to_owned(),
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(SearchDoc {
            message,
            mailbox,
            received_at: Some(received_at),
            subject: msg.subject().unwrap_or_default().to_owned(),
            from: addresses(msg.from()),
            to: addresses(msg.to()),
            cc: addresses(msg.cc()),
            bcc: addresses(msg.bcc()),
            body,
            attachments,
            headers,
        })
    }

    fn fields(&self, field: &TextField) -> Vec<&str> {
        match field {
            TextField::Any => {
                let mut v = vec![
                    self.subject.as_str(),
                    &self.from,
                    &self.to,
                    &self.cc,
                    &self.bcc,
                    &self.body,
                    &self.attachments,
                ];
                v.extend(self.headers.iter().map(|(_, h)| h.as_str()));
                v
            }
            TextField::Body => vec![&self.body],
            TextField::Subject => vec![&self.subject],
            TextField::From => vec![&self.from],
            TextField::To => vec![&self.to],
            TextField::Cc => vec![&self.cc],
            TextField::Bcc => vec![&self.bcc],
            TextField::Attachment => vec![&self.attachments],
            TextField::Header(name) => self
                .headers
                .iter()
                .filter(|(n, _)| n.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
                .collect(),
        }
    }

    fn matches(&self, q: &TextQuery) -> bool {
        match q {
            TextQuery::Term { field, text } => {
                let fields = self.fields(field);
                if text.is_empty() {
                    !fields.is_empty()
                } else {
                    fields.iter().any(|f| !find_all(f, text).is_empty())
                }
            }
            TextQuery::And(qs) => qs.iter().all(|q| self.matches(q)),
            TextQuery::Or(qs) => qs.iter().any(|q| self.matches(q)),
            TextQuery::Not(q) => !self.matches(q),
        }
    }
}

/// Байтовые диапазоны вхождений `needle` в `hay` без учёта регистра.
fn find_all(hay: &str, needle: &str) -> Vec<(usize, usize)> {
    let low = |c: char| c.to_lowercase().next().unwrap_or(c);
    let hay: Vec<(usize, char)> = hay.char_indices().map(|(i, c)| (i, low(c))).collect();
    let needle: Vec<char> = needle.chars().map(low).collect();
    let mut out = Vec::new();
    if needle.is_empty() {
        return out;
    }
    let mut i = 0;
    while i + needle.len() <= hay.len() {
        if hay[i..i + needle.len()]
            .iter()
            .zip(&needle)
            .all(|((_, h), n)| h == n)
        {
            let (start, _) = hay[i];
            let (last, c) = hay[i + needle.len() - 1];
            out.push((start, last + c.len_utf8()));
            i += needle.len();
        } else {
            i += 1;
        }
    }
    out
}

/// Тексты условий запроса вне `Not` для поля (подсветка).
fn positive_terms<'q>(q: &'q TextQuery, pick: &dyn Fn(&TextField) -> bool, out: &mut Vec<&'q str>) {
    match q {
        TextQuery::Term { field, text } if pick(field) && !text.is_empty() => out.push(text),
        TextQuery::Term { .. } | TextQuery::Not(_) => {}
        TextQuery::And(qs) | TextQuery::Or(qs) => {
            for q in qs {
                positive_terms(q, pick, out);
            }
        }
    }
}

fn escape(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
}

/// Текст с `<mark>` вокруг вхождений `terms`; совпадений нет — `None`.
fn highlight(text: &str, terms: &[&str]) -> Option<String> {
    let mut hits: Vec<_> = terms.iter().flat_map(|t| find_all(text, t)).collect();
    hits.sort_unstable();
    let mut out = String::new();
    let mut pos = 0;
    for (start, end) in hits {
        if start < pos {
            continue;
        }
        escape(&text[pos..start], &mut out);
        out.push_str("<mark>");
        escape(&text[start..end], &mut out);
        out.push_str("</mark>");
        pos = end;
    }
    (pos > 0).then(|| {
        escape(&text[pos..], &mut out);
        out
    })
}

/// Длина фрагмента текста в [`Snippet::preview`], символов.
const PREVIEW_CHARS: usize = 200;
/// Контекст перед первым совпадением во фрагменте, символов.
const PREVIEW_BEFORE: usize = 40;

/// [`SearchIndex`] по подстроке без учёта регистра (без морфологии). Письма добавляет тест
/// ([`FakeSearch::index`], [`SearchDoc::parse`]). `Relevance` — как `NewestFirst`.
#[derive(Debug, Clone, Default)]
pub struct FakeSearch(Arc<Mutex<SearchDocs>>);

/// Письма индекса по учётным записям.
type SearchDocs = HashMap<(TenantId, AccountId), HashMap<MessageId, SearchDoc>>;

impl FakeSearch {
    /// Добавляет или заменяет письмо (смена папки — повторный `index`).
    pub fn index(&self, scope: AccountScope, doc: SearchDoc) {
        lock(&self.0)
            .entry((scope.tenant, scope.account))
            .or_default()
            .insert(doc.message, doc);
    }

    /// Удаляет письмо из индекса.
    pub fn remove(&self, scope: AccountScope, message: MessageId) {
        if let Some(docs) = lock(&self.0).get_mut(&(scope.tenant, scope.account)) {
            docs.remove(&message);
        }
    }
}

#[async_trait]
impl SearchIndex for FakeSearch {
    async fn query(
        &self,
        scope: AccountScope,
        mailboxes: &[MailboxId],
        q: &TextQuery,
        sort: SearchSort,
        limit: usize,
    ) -> Result<Vec<MessageId>> {
        let st = lock(&self.0);
        let Some(docs) = st.get(&(scope.tenant, scope.account)) else {
            return Ok(Vec::new());
        };
        let mut hits: Vec<_> = docs
            .values()
            .filter(|d| mailboxes.is_empty() || mailboxes.contains(&d.mailbox))
            .filter(|d| d.matches(q))
            .map(|d| (d.received_at, d.message))
            .collect();
        hits.sort_unstable();
        if sort != SearchSort::OldestFirst {
            hits.reverse();
        }
        Ok(hits.into_iter().take(limit).map(|(_, id)| id).collect())
    }

    async fn snippet(
        &self,
        scope: AccountScope,
        message: MessageId,
        q: &TextQuery,
    ) -> Result<Snippet> {
        let st = lock(&self.0);
        let doc = st
            .get(&(scope.tenant, scope.account))
            .and_then(|d| d.get(&message))
            .ok_or(Error::NotFound)?;
        let mut subject_terms = Vec::new();
        positive_terms(
            q,
            &|f| matches!(f, TextField::Any | TextField::Subject),
            &mut subject_terms,
        );
        let mut body_terms = Vec::new();
        positive_terms(
            q,
            &|f| matches!(f, TextField::Any | TextField::Body),
            &mut body_terms,
        );
        let first = body_terms
            .iter()
            .filter_map(|t| find_all(&doc.body, t).first().map(|h| h.0))
            .min();
        let preview = first.and_then(|at| {
            let start = doc.body[..at]
                .char_indices()
                .rev()
                .nth(PREVIEW_BEFORE - 1)
                .map_or(0, |(i, _)| i);
            let window: String = doc.body[start..].chars().take(PREVIEW_CHARS).collect();
            highlight(&window, &body_terms)
        });
        Ok(Snippet {
            subject: highlight(&doc.subject, &subject_terms),
            preview,
        })
    }
}

// ---------- IndexManifestStore ----------

/// [`IndexManifestStore`] в памяти: версия — CAS, запрос перестройки увеличивает версию.
#[derive(Debug, Clone, Default)]
pub struct MemIndexManifests(Arc<Mutex<HashMap<(TenantId, AccountId), IndexManifest>>>);

#[async_trait]
impl IndexManifestStore for MemIndexManifests {
    async fn get_manifest(&self, scope: AccountScope) -> Result<IndexManifest> {
        Ok(lock(&self.0)
            .get(&(scope.tenant, scope.account))
            .cloned()
            .unwrap_or_default())
    }

    async fn put_manifest(
        &self,
        scope: AccountScope,
        expected: u64,
        update: ManifestUpdate,
    ) -> Result<u64> {
        let mut st = lock(&self.0);
        let m = st.entry((scope.tenant, scope.account)).or_default();
        if m.version != expected {
            return Err(Error::Conflict);
        }
        m.version += 1;
        m.schema = update.schema;
        m.segments = update.segments;
        if update.rebuilt {
            m.rebuild = None;
        }
        Ok(m.version)
    }

    async fn request_rebuild(&self, scope: AccountScope, request: RebuildRequest) -> Result<u64> {
        let mut st = lock(&self.0);
        let m = st.entry((scope.tenant, scope.account)).or_default();
        m.version += 1;
        m.rebuild = Some(request);
        Ok(m.version)
    }

    async fn pending_rebuilds(&self, schema: u32, limit: usize) -> Result<Vec<AccountScope>> {
        let st = lock(&self.0);
        // Сначала запрошенные (давние первыми), затем индексы со старой схемой.
        let mut pending: Vec<_> = st
            .iter()
            .filter_map(|(&(tenant, account), m)| {
                let key = match m.rebuild {
                    Some(r) => (0, Some(r.requested_at)),
                    None if m.version > 0 && m.schema < schema => (1, None),
                    None => return None,
                };
                Some((key, tenant, account))
            })
            .collect();
        pending.sort_unstable();
        Ok(pending
            .into_iter()
            .take(limit)
            .map(|(_, tenant, account)| AccountScope { tenant, account })
            .collect())
    }
}

// ---------- QueueStore ----------

#[derive(Debug)]
struct RcptState {
    rcpt: OutRcpt,
    attempts: u32,
    last_error: Option<String>,
    next_attempt: OffsetDateTime,
    done: bool,
    /// Выдан текущим захватом и ещё не завершён.
    claimed: bool,
}

#[derive(Debug)]
struct QueueEntry {
    msg: OutMessage,
    queued_at: OffsetDateTime,
    fencing: u64,
    lease_until: Option<OffsetDateTime>,
    rcpts: Vec<RcptState>,
}

impl QueueEntry {
    fn due(&self, now: OffsetDateTime) -> Option<OffsetDateTime> {
        self.rcpts
            .iter()
            .filter(|r| !r.done && r.next_attempt <= now)
            .map(|r| r.next_attempt)
            .min()
    }
}

/// [`QueueStore`] в памяти. Время захвата передаёт вызывающий; `queued_at` — по часам [`Clock`].
/// Когда все выданные захватом получатели завершены (`reschedule`/`complete`), лиза снимается.
#[derive(Debug, Clone)]
pub struct MemQueue {
    entries: Arc<Mutex<HashMap<QueueId, QueueEntry>>>,
    clock: Arc<dyn Clock>,
}

impl MemQueue {
    /// Очередь с часами `clock`.
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            entries: Arc::default(),
            clock,
        }
    }

    /// Число записей в очереди.
    pub fn len(&self) -> usize {
        lock(&self.entries).len()
    }

    /// Очередь пуста.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Объекты писем в очереди (проверка учёта ссылок).
    pub fn blobs(&self) -> Vec<BlobKey> {
        lock(&self.entries).values().map(|e| e.msg.blob).collect()
    }

    fn with_claim<R>(
        &self,
        claim: &QueueClaim,
        rcpt: &str,
        f: impl FnOnce(&mut RcptState) -> R,
    ) -> Result<(R, bool)> {
        let mut st = lock(&self.entries);
        let e = st
            .get_mut(&claim.id)
            .filter(|e| e.fencing == claim.fencing)
            .ok_or(Error::Conflict)?;
        let r = e
            .rcpts
            .iter_mut()
            .find(|r| r.rcpt.address == rcpt && !r.done)
            .ok_or(Error::NotFound)?;
        let out = f(r);
        r.claimed = false;
        if !e.rcpts.iter().any(|r| r.claimed) {
            e.lease_until = None;
        }
        let finished = e.rcpts.iter().all(|r| r.done);
        if finished {
            st.remove(&claim.id);
        }
        Ok((out, finished))
    }
}

#[async_trait]
impl QueueStore for MemQueue {
    async fn enqueue(&self, msg: OutMessage) -> Result<QueueId> {
        if msg.rcpts.is_empty() {
            return Err(Error::invalid("queue: no recipients"));
        }
        let mut seen = HashSet::new();
        if !msg.rcpts.iter().all(|r| seen.insert(r.address.as_str())) {
            return Err(Error::invalid("queue: duplicate recipient"));
        }
        let id = QueueId::new();
        let rcpts = msg
            .rcpts
            .iter()
            .map(|r| RcptState {
                rcpt: r.clone(),
                attempts: 0,
                last_error: None,
                next_attempt: msg.next_attempt,
                done: false,
                claimed: false,
            })
            .collect();
        let entry = QueueEntry {
            queued_at: self.clock.now(),
            fencing: 0,
            lease_until: None,
            rcpts,
            msg,
        };
        lock(&self.entries).insert(id, entry);
        Ok(id)
    }

    async fn claim(
        &self,
        now: OffsetDateTime,
        limit: usize,
        lease: Duration,
    ) -> Result<Vec<Claimed>> {
        let until = now.saturating_add(lease.try_into().unwrap_or(time::Duration::MAX));
        let mut st = lock(&self.entries);
        let mut ready: Vec<_> = st
            .iter()
            .filter(|(_, e)| e.lease_until.is_none_or(|u| u <= now))
            .filter_map(|(id, e)| Some((e.due(now)?, *id)))
            .collect();
        ready.sort_unstable();
        let mut out = Vec::new();
        for (_, id) in ready.into_iter().take(limit) {
            let Some(e) = st.get_mut(&id) else { continue };
            e.fencing += 1;
            e.lease_until = Some(until);
            let mut rcpts = Vec::new();
            for r in &mut e.rcpts {
                r.claimed = !r.done && r.next_attempt <= now;
                if r.claimed {
                    rcpts.push(QueuedRcpt {
                        rcpt: r.rcpt.clone(),
                        attempts: r.attempts,
                        last_error: r.last_error.clone(),
                    });
                }
            }
            out.push(Claimed {
                claim: QueueClaim {
                    id,
                    fencing: e.fencing,
                },
                until,
                queued_at: e.queued_at,
                tenant: e.msg.tenant,
                blob: e.msg.blob,
                size: e.msg.size,
                from: e.msg.from.clone(),
                dsn: e.msg.dsn.clone(),
                rcpts,
            });
        }
        Ok(out)
    }

    async fn renew(&self, claim: &QueueClaim, until: OffsetDateTime) -> Result<()> {
        let mut st = lock(&self.entries);
        let e = st
            .get_mut(&claim.id)
            .filter(|e| e.fencing == claim.fencing)
            .ok_or(Error::Conflict)?;
        e.lease_until = Some(until);
        Ok(())
    }

    async fn reschedule(
        &self,
        claim: &QueueClaim,
        rcpt: &str,
        at: OffsetDateTime,
        reason: &str,
    ) -> Result<()> {
        self.with_claim(claim, rcpt, |r| {
            r.next_attempt = at;
            r.attempts += 1;
            r.last_error = Some(reason.to_owned());
        })
        .map(|_| ())
    }

    async fn complete(&self, claim: &QueueClaim, rcpt: &str, status: RcptStatus) -> Result<bool> {
        self.with_claim(claim, rcpt, |r| {
            r.done = true;
            if let RcptStatus::Failed { reply } = status {
                r.last_error = Some(reply);
            }
        })
        .map(|(_, finished)| finished)
    }
}

// ---------- TrashStore, Quota ----------

/// [`TrashStore`] поверх [`MemMetadata`]: элементы — письма, перенесённые в корзину `to_trash`
/// и удалением папки; срок — [`MemMetadata::with_trash_retention`] на момент переноса.
/// Настройка `trash.purge` для `purge` — из [`MemPolicies`].
#[derive(Debug, Clone)]
pub struct MemTrash {
    meta: MemMetadata,
    policies: MemPolicies,
}

impl MemTrash {
    /// Корзина метаданных `meta` (разделяет с ними данные) с собственными политиками без заданных уровней
    /// (`trash.purge` — `hide`).
    pub fn new(meta: &MemMetadata) -> Self {
        Self {
            meta: meta.clone(),
            policies: MemPolicies::new(meta),
        }
    }

    /// Настройка `trash.purge` — из `policies`.
    pub fn with_policies(mut self, policies: MemPolicies) -> Self {
        self.policies = policies;
        self
    }

    /// Политики, по которым работает `purge`.
    pub fn policies(&self) -> &MemPolicies {
        &self.policies
    }

    /// Элементы корзины `scope`: видимые или скрытые, новые первыми, со смещения, не более `limit`.
    fn page(
        &self,
        scope: AccountScope,
        hidden: bool,
        offset: usize,
        limit: usize,
    ) -> Vec<TrashItem> {
        let mut rows: Vec<_> = self
            .meta
            .trash_rows(Some(scope))
            .into_iter()
            .filter(|r| r.hidden == hidden)
            .collect();
        rows.sort_unstable_by_key(|r| std::cmp::Reverse((r.deleted_at, r.message.id)));
        rows.into_iter()
            .skip(offset)
            .take(limit)
            .map(trash_item)
            .collect()
    }
}

fn trash_item(r: TrashRow) -> TrashItem {
    TrashItem {
        scope: r.scope,
        object: TrashObject::Message(r.message.id),
        origin: r.origin,
        size: r.message.size,
        received_at: r.message.received_at,
        deleted_at: r.deleted_at,
        expires_at: r.expires_at,
    }
}

fn trashed_message(object: TrashObject) -> Result<MessageId> {
    match object {
        TrashObject::Message(id) => Ok(id),
        _ => Err(Error::invalid("trash: unsupported object")),
    }
}

#[async_trait]
impl TrashStore for MemTrash {
    async fn list(
        &self,
        scope: AccountScope,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<TrashItem>> {
        Ok(self.page(scope, false, offset, limit))
    }

    async fn list_hidden(
        &self,
        scope: AccountScope,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<TrashItem>> {
        Ok(self.page(scope, true, offset, limit))
    }

    async fn restore(&self, scope: AccountScope, object: TrashObject) -> Result<Restored> {
        let id = trashed_message(object)?;
        let Some(row) = self
            .meta
            .trash_rows(Some(scope))
            .into_iter()
            .find(|r| r.message.id == id)
        else {
            return Err(Error::NotFound);
        };
        if row.hidden {
            return self.meta.restore_hidden(scope, id);
        }
        let inserted = self
            .meta
            .restore(scope, &[id])
            .await?
            .pop()
            .ok_or(Error::NotFound)?;
        let mailbox = MessageStore::get(&self.meta, scope, id)
            .await?
            .ok_or(Error::NotFound)?
            .mailbox;
        Ok(Restored::Message { mailbox, inserted })
    }

    async fn purge(
        &self,
        scope: AccountScope,
        ids: &[MessageId],
        hooks: &DecisionHooks,
    ) -> Result<PurgeOutcome> {
        let mode = self.policies.trash_purge(scope).await?.effective();
        let rows: HashMap<MessageId, TrashRow> = self
            .meta
            .trash_rows(Some(scope))
            .into_iter()
            .filter(|r| !r.hidden)
            .map(|r| (r.message.id, r))
            .collect();
        let mut seen = HashSet::new();
        let (mut hide, mut erase) = (Vec::new(), Vec::new());
        for id in ids {
            let Some(r) = rows.get(id).filter(|_| seen.insert(*id)) else {
                continue;
            };
            let allowed = match mode {
                TrashPurge::Hide => false,
                // Запрет (Legal Hold) и ошибка точки принятия решений — скрыть: стирание необратимо.
                TrashPurge::Erase => matches!(
                    hooks
                        .before_final_delete(&FinalDelete {
                            scope,
                            mailbox: r.origin,
                            message: *id,
                            received_at: r.message.received_at,
                        })
                        .await,
                    Ok(Decision::Allow)
                ),
            };
            if allowed {
                erase.push(*id);
            } else {
                hide.push(*id);
            }
        }
        self.meta.purge_trash(scope, &hide, &erase)
    }

    async fn expired(&self, now: OffsetDateTime, limit: usize) -> Result<Vec<TrashItem>> {
        let mut rows: Vec<_> = self
            .meta
            .trash_rows(None)
            .into_iter()
            .filter(|r| r.expires_at <= now)
            .collect();
        rows.sort_unstable_by_key(|r| (r.expires_at, r.message.id));
        Ok(rows.into_iter().take(limit).map(trash_item).collect())
    }

    async fn defer(
        &self,
        scope: AccountScope,
        object: TrashObject,
        until: OffsetDateTime,
    ) -> Result<()> {
        self.meta
            .defer_trash(scope, trashed_message(object)?, until)
    }
}

#[derive(Debug, Default)]
struct QuotaLimits {
    default: Option<u64>,
    accounts: HashMap<(TenantId, AccountId), Option<u64>>,
}

/// [`Quota`] поверх [`MemMetadata`]: использование — по письмам учётной записи (включая корзину),
/// лимит — заданный для учётной записи, иначе по умолчанию (`None` — без лимита).
#[derive(Debug, Clone)]
pub struct MemQuota {
    meta: MemMetadata,
    limits: Arc<Mutex<QuotaLimits>>,
}

impl MemQuota {
    /// Квоты учётных записей `meta` без лимита.
    pub fn new(meta: &MemMetadata) -> Self {
        Self {
            meta: meta.clone(),
            limits: Arc::default(),
        }
    }

    /// Лимит по умолчанию (значение конфигурации).
    pub fn set_default_limit(&self, limit: Option<u64>) {
        lock(&self.limits).default = limit;
    }

    /// Лимит учётной записи (политика ящика); `None` — без лимита.
    pub fn set_limit(&self, scope: AccountScope, limit: Option<u64>) {
        lock(&self.limits)
            .accounts
            .insert((scope.tenant, scope.account), limit);
    }
}

#[async_trait]
impl Quota for MemQuota {
    async fn usage(&self, scope: AccountScope) -> Result<QuotaUsage> {
        let mut usage = self.meta.quota(scope.tenant, scope.account).await?;
        let limits = lock(&self.limits);
        usage.limit_bytes = limits
            .accounts
            .get(&(scope.tenant, scope.account))
            .copied()
            .unwrap_or(limits.default);
        Ok(usage)
    }
}

// ---------- DecisionHook ----------

/// Вызов [`RecordingDecisionHook`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookCall {
    /// `before_final_delete`.
    FinalDelete(FinalDelete),
    /// `on_delivery`.
    Delivery(DeliveryInfo),
    /// `on_submission`.
    Submission(SubmissionInfo),
}

#[derive(Debug, Default)]
struct HookState {
    calls: Vec<HookCall>,
    final_delete: Decision,
    delivery: Decision,
    submission: Decision,
}

/// [`DecisionHook`], записывающий вызовы и отвечающий заданными решениями (по умолчанию `Allow`).
#[derive(Debug, Clone)]
pub struct RecordingDecisionHook {
    name: String,
    state: Arc<Mutex<HookState>>,
}

impl RecordingDecisionHook {
    /// Хук с именем `name`.
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            state: Arc::default(),
        }
    }

    /// Решение `before_final_delete` (Legal Hold — `Deny`).
    pub fn set_final_delete(&self, d: Decision) {
        lock(&self.state).final_delete = d;
    }

    /// Решение `on_delivery`.
    pub fn set_delivery(&self, d: Decision) {
        lock(&self.state).delivery = d;
    }

    /// Решение `on_submission`.
    pub fn set_submission(&self, d: Decision) {
        lock(&self.state).submission = d;
    }

    /// Вызовы в порядке поступления.
    pub fn calls(&self) -> Vec<HookCall> {
        lock(&self.state).calls.clone()
    }

    fn record(&self, call: HookCall) -> Decision {
        let mut st = lock(&self.state);
        let d = match &call {
            HookCall::FinalDelete(_) => st.final_delete.clone(),
            HookCall::Delivery(_) => st.delivery.clone(),
            HookCall::Submission(_) => st.submission.clone(),
        };
        st.calls.push(call);
        d
    }
}

#[async_trait]
impl DecisionHook for RecordingDecisionHook {
    fn name(&self) -> &str {
        &self.name
    }

    async fn before_final_delete(&self, op: &FinalDelete) -> Result<Decision> {
        Ok(self.record(HookCall::FinalDelete(op.clone())))
    }

    async fn on_delivery(&self, op: &DeliveryInfo) -> Result<Decision> {
        Ok(self.record(HookCall::Delivery(op.clone())))
    }

    async fn on_submission(&self, op: &SubmissionInfo) -> Result<Decision> {
        Ok(self.record(HookCall::Submission(op.clone())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ManualClock;
    use tp_contracts::ext::DecisionHooks;
    use tp_contracts::meta::{Flags, MessageMeta, NewMessage, SpecialUse};
    use tp_contracts::queue::DsnParams;
    use tp_contracts::search::{IndexSegment, RebuildReason};
    use tp_contracts::{BlobHash, Gen};

    const TLS: SaslContext = SaslContext {
        service: "imap",
        tls: true,
    };

    fn auth() -> (FakeAuthenticator, Principal) {
        let auth = FakeAuthenticator::default();
        let p = principal(TenantId::DEFAULT, "user@example.com");
        auth.add_password("User@Example.com", "secret", p.clone());
        auth.add_token("tok", p.clone());
        (auth, p)
    }

    #[tokio::test]
    async fn authenticator() {
        let (auth, p) = auth();
        let pass = |login: &str, secret: &str| Credentials::Password {
            login: login.into(),
            secret: secret.into(),
        };
        assert_eq!(
            auth.verify(pass("user@example.com", "secret")).await,
            Ok(p.clone())
        );
        assert_eq!(
            auth.verify(pass("user@example.com", "x")).await,
            Err(Error::Forbidden)
        );
        assert_eq!(
            auth.verify(Credentials::Bearer("tok".into())).await,
            Ok(p.clone())
        );
        auth.add_kerberos("user@EXAMPLE.COM", p.clone());
        let krb = |authzid: Option<&str>| Credentials::Gssapi {
            principal: "user@EXAMPLE.COM".into(),
            authzid: authzid.map(str::to_owned),
        };
        assert_eq!(auth.verify(krb(None)).await, Ok(p.clone()));
        assert_eq!(auth.verify(krb(Some("other"))).await, Err(Error::Forbidden));
        auth.set_failure(Some(Error::UNAVAILABLE));
        assert_eq!(auth.verify(krb(None)).await, Err(Error::UNAVAILABLE));
        auth.set_failure(None);
        auth.disable("user@example.com");
        assert_eq!(
            auth.verify(Credentials::Bearer("tok".into())).await,
            Err(Error::Forbidden)
        );
        assert_eq!(
            auth.verify(pass("user@example.com", "secret")).await,
            Err(Error::Forbidden)
        );
    }

    #[tokio::test]
    async fn sasl_plain() {
        let (auth, p) = auth();
        let sasl = FakeSasl::new(Arc::new(auth.clone()));
        assert!(sasl.mechanisms(false).is_empty());
        assert_eq!(
            sasl.mechanisms(true),
            [Mechanism::Plain, Mechanism::OauthBearer]
        );
        let plain = SaslContext { tls: false, ..TLS };
        assert!(matches!(
            sasl.start(Mechanism::Plain, &plain),
            Err(Error::Invalid(_))
        ));
        assert!(matches!(
            sasl.start(Mechanism::Login, &TLS),
            Err(Error::Invalid(_))
        ));

        let run = |msg: &'static [u8]| {
            let sasl = sasl.clone();
            async move {
                let mut s = sasl.start(Mechanism::Plain, &TLS).expect("start");
                s.step(Some(msg)).await
            }
        };
        assert_eq!(
            run(b"\0user@example.com\0secret").await,
            Ok(SaslStep::Done(p.clone()))
        );
        assert_eq!(
            run(b"user@example.com\0USER@example.com\0secret").await,
            Ok(SaslStep::Done(p.clone()))
        );
        assert_eq!(
            run(b"\0user@example.com\0wrong").await,
            Ok(SaslStep::Fail(SaslFailure::Authentication))
        );
        assert_eq!(
            run(b"boss@example.com\0user@example.com\0secret").await,
            Ok(SaslStep::Fail(SaslFailure::Authorization))
        );
        for bad in [&b"user\0secret"[..], b"\0\0secret", b"\0a\0b\0c", b""] {
            let mut s = sasl.start(Mechanism::Plain, &TLS).expect("start");
            assert!(
                matches!(s.step(Some(bad)).await, Err(Error::Invalid(_))),
                "{bad:?}"
            );
        }

        // Без начального ответа — пустой вызов; после завершения сеанс не используется.
        let mut s = sasl.start(Mechanism::Plain, &TLS).expect("start");
        assert_eq!(s.step(None).await, Ok(SaslStep::Challenge(Vec::new())));
        assert!(s.step(None).await.is_err());
        assert_eq!(
            s.step(Some(b"\0user@example.com\0secret")).await,
            Ok(SaslStep::Done(p))
        );
        assert!(s.step(Some(b"")).await.is_err());

        auth.set_failure(Some(Error::UNAVAILABLE));
        assert_eq!(
            run(b"\0user@example.com\0secret").await,
            Err(Error::UNAVAILABLE)
        );
    }

    #[tokio::test]
    async fn sasl_oauthbearer() {
        let (auth, p) = auth();
        let sasl = FakeSasl::new(Arc::new(auth));
        let start = || sasl.start(Mechanism::OauthBearer, &TLS).expect("start");
        let ok = b"n,a=user@example.com,\x01host=mail\x01port=993\x01auth=Bearer tok\x01\x01";
        assert_eq!(start().step(Some(ok)).await, Ok(SaslStep::Done(p.clone())));
        let mut s = start();
        assert_eq!(s.step(None).await, Ok(SaslStep::Challenge(Vec::new())));
        assert_eq!(
            s.step(Some(b"n,,\x01auth=bearer tok\x01\x01")).await,
            Ok(SaslStep::Done(p))
        );
        assert_eq!(
            start()
                .step(Some(b"n,a=other@example.com,\x01auth=Bearer tok\x01\x01"))
                .await,
            Ok(SaslStep::Fail(SaslFailure::Authorization))
        );

        // Неверный токен: вызов с ошибкой (RFC 7628, 3.2.2), клиент отвечает %x01 — отказ.
        let mut s = start();
        let SaslStep::Challenge(err) = s
            .step(Some(b"n,,\x01auth=Bearer bad\x01\x01"))
            .await
            .expect("step")
        else {
            panic!("challenge expected");
        };
        assert!(String::from_utf8_lossy(&err).contains("invalid_token"));
        assert_eq!(
            s.step(Some(b"\x01")).await,
            Ok(SaslStep::Fail(SaslFailure::Authentication))
        );
        for bad in [
            &b"n,,auth=Bearer tok"[..],
            b"p=x,,\x01auth=Bearer tok\x01\x01",
            b"n,,\x01\x01",
        ] {
            assert!(
                matches!(start().step(Some(bad)).await, Err(Error::Invalid(_))),
                "{bad:?}"
            );
        }

        let only_plain = sasl
            .clone()
            .with_mechanisms(&[Mechanism::Plain, Mechanism::Gssapi]);
        assert_eq!(only_plain.mechanisms(true), [Mechanism::Plain]);
        assert!(only_plain.start(Mechanism::OauthBearer, &TLS).is_err());
    }

    fn dir_account(id: &str) -> DirAccount {
        DirAccount {
            external_id: id.into(),
            login: format!("{id}@example.com"),
            display_name: id.into(),
            emails: vec![format!("{id}@example.com")],
            enabled: true,
            attributes: Default::default(),
        }
    }

    #[tokio::test]
    async fn directory_incremental() {
        let t = TenantId::DEFAULT;
        let dir = FakeDirectory::default();
        let empty = dir.accounts_since(t, None, 10).await.expect("empty");
        assert!(empty.changes.is_empty() && !empty.has_more);
        for id in ["a", "b", "c"] {
            dir.upsert_account(t, dir_account(id));
        }
        dir.remove_account(t, "b");
        let ids = |p: &DirPage<DirAccount>| -> Vec<String> {
            p.changes
                .iter()
                .map(|c| match c {
                    DirChange::Upsert(a) => a.external_id.clone(),
                    DirChange::Removed { external_id } => format!("-{external_id}"),
                })
                .collect()
        };
        // Полная выгрузка — без удалений; продолжение по курсору — с удалениями после него.
        let p1 = dir.accounts_since(t, None, 1).await.expect("p1");
        assert_eq!((ids(&p1), p1.has_more), (vec!["a".into()], true));
        let p2 = dir.accounts_since(t, Some(&p1.next), 1).await.expect("p2");
        assert_eq!((ids(&p2), p2.has_more), (vec!["c".into()], true));
        let p3 = dir.accounts_since(t, Some(&p2.next), 1).await.expect("p3");
        assert_eq!((ids(&p3), p3.has_more), (vec!["-b".into()], false));
        let none = dir
            .accounts_since(t, Some(&p3.next), 10)
            .await
            .expect("none");
        assert!(none.changes.is_empty() && none.next == p3.next);

        let mut a = dir_account("a");
        a.enabled = false;
        dir.upsert_account(t, a.clone());
        dir.remove_account(t, "c");
        let inc = dir
            .accounts_since(t, Some(&p3.next), 10)
            .await
            .expect("inc");
        assert_eq!(ids(&inc), ["a", "-c"]);
        assert_eq!(inc.changes[0], DirChange::Upsert(a));
        let full = dir.accounts_since(t, None, 10).await.expect("full");
        assert_eq!(ids(&full), ["a"]);

        dir.expire_cursors(t);
        assert_eq!(
            dir.accounts_since(t, Some(&p3.next), 10).await,
            Err(Error::Conflict)
        );
        let bogus = DirCursor("zzz".into());
        assert_eq!(
            dir.accounts_since(t, Some(&bogus), 10).await,
            Err(Error::Conflict)
        );
        let again = dir.accounts_since(t, None, 10).await.expect("again");
        assert!(dir.accounts_since(t, Some(&again.next), 10).await.is_ok());

        dir.upsert_group(
            t,
            DirGroup {
                external_id: "g".into(),
                name: "staff".into(),
                email: Some("staff@example.com".into()),
                members: vec!["a".into()],
            },
        );
        let g = dir.groups_since(t, None, 10).await.expect("groups");
        assert_eq!(g.changes.len(), 1);
        dir.remove_group(t, "g");
        let g2 = dir
            .groups_since(t, Some(&g.next), 10)
            .await
            .expect("groups");
        assert_eq!(
            g2.changes,
            [DirChange::Removed {
                external_id: "g".into()
            }]
        );

        let alias = DirAlias {
            address: "postmaster@example.com".into(),
            target: "a".into(),
        };
        dir.set_aliases(t, vec![alias.clone()]);
        dir.set_domains(
            t,
            vec![DirDomain {
                name: "example.com".into(),
                default: true,
            }],
        );
        dir.set_gal_attributes(t, vec!["title".into()]);
        assert_eq!(dir.aliases(t).await, Ok(vec![alias]));
        assert_eq!(dir.domains(t).await.expect("domains").len(), 1);
        assert_eq!(dir.gal_attributes(t).await, Ok(vec!["title".to_string()]));
        let other = TenantId::new();
        assert_eq!(dir.aliases(other).await, Ok(Vec::new()));
    }

    fn scope() -> AccountScope {
        AccountScope {
            tenant: TenantId::DEFAULT,
            account: AccountId::new(),
        }
    }

    #[tokio::test]
    async fn search_substring() {
        let s = scope();
        let (inbox, sent) = (MailboxId::new(), MailboxId::new());
        let t0 = OffsetDateTime::UNIX_EPOCH;
        let raw = b"From: Ivan Ivanov <ivan@example.com>\r\nTo: petr@example.com\r\n\
            Subject: =?UTF-8?B?0J/RgNC40LLQtdGCLCDQvNC40YA=?=\r\nList-Id: <dev.example.com>\r\n\
            Content-Type: text/plain; charset=utf-8\r\n\r\nHello <World> & friends\r\n";
        let a = SearchDoc::parse(MessageId::new(), inbox, t0, raw).expect("parse");
        assert_eq!(a.subject, "Привет, мир");
        assert!(a.from.contains("ivan@example.com") && a.to.contains("petr@"));
        let b = SearchDoc {
            message: MessageId::new(),
            mailbox: sent,
            received_at: Some(t0 + time::Duration::hours(1)),
            subject: "Отчёт".into(),
            body: "привет из отдела".into(),
            ..SearchDoc::default()
        };
        let search = FakeSearch::default();
        search.index(s, a.clone());
        search.index(s, b.clone());
        let q = |q: TextQuery, sort| {
            let search = search.clone();
            async move { search.query(s, &[], &q, sort, 10).await.expect("query") }
        };
        let term = TextQuery::term;
        let newest = SearchSort::NewestFirst;
        assert_eq!(
            q(term(TextField::Subject, "ПРИВЕТ"), newest).await,
            [a.message]
        );
        assert_eq!(
            q(term(TextField::Any, "привет"), newest).await,
            [b.message, a.message]
        );
        assert_eq!(
            q(term(TextField::Any, "привет"), SearchSort::OldestFirst).await,
            [a.message, b.message]
        );
        assert_eq!(
            q(term(TextField::Header("list-id".into()), ""), newest).await,
            [a.message]
        );
        assert_eq!(
            q(
                TextQuery::Not(Box::new(term(TextField::From, "ivanov"))),
                newest
            )
            .await,
            [b.message]
        );
        assert_eq!(q(TextQuery::And(vec![]), newest).await.len(), 2);
        assert!(q(TextQuery::Or(vec![]), newest).await.is_empty());
        let only_sent = search
            .query(s, &[sent], &term(TextField::Any, "привет"), newest, 10)
            .await
            .expect("query");
        assert_eq!(only_sent, [b.message]);
        let one = search
            .query(s, &[], &term(TextField::Any, "привет"), newest, 1)
            .await
            .expect("query");
        assert_eq!(one, [b.message]);

        let either = TextQuery::Or(vec![
            term(TextField::Any, "world"),
            term(TextField::Subject, "мир"),
        ]);
        let snip = search
            .snippet(s, a.message, &either)
            .await
            .expect("snippet");
        assert_eq!(snip.subject.as_deref(), Some("Привет, <mark>мир</mark>"));
        assert_eq!(
            snip.preview.as_deref().map(str::trim_end),
            Some("Hello &lt;<mark>World</mark>&gt; &amp; friends")
        );
        let none = search
            .snippet(s, b.message, &term(TextField::Subject, "нет"))
            .await;
        assert_eq!(none, Ok(Snippet::default()));
        search.remove(s, a.message);
        let all = TextQuery::And(vec![]);
        assert_eq!(
            search.snippet(s, a.message, &all).await,
            Err(Error::NotFound)
        );
        assert!(
            search
                .query(scope(), &[], &all, newest, 10)
                .await
                .expect("other")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn manifest_cas_and_rebuild() {
        let store = MemIndexManifests::default();
        let (a, b, c) = (scope(), scope(), scope());
        assert_eq!(store.get_manifest(a).await, Ok(IndexManifest::default()));
        let seg = IndexSegment {
            path: "idx/a/1".into(),
            size: 10,
            docs: 2,
        };
        let update = |schema, rebuilt| ManifestUpdate {
            schema,
            segments: vec![seg.clone()],
            rebuilt,
        };
        assert_eq!(store.put_manifest(a, 0, update(1, false)).await, Ok(1));
        assert_eq!(
            store.put_manifest(a, 0, update(1, false)).await,
            Err(Error::Conflict)
        );
        assert_eq!(store.put_manifest(b, 0, update(2, false)).await, Ok(1));
        let m = store.get_manifest(a).await.expect("get");
        assert_eq!((m.version, m.schema, m.docs()), (1, 1, 2));

        let req = |s: i64| RebuildRequest {
            requested_at: OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(s),
            reason: RebuildReason::Admin,
        };
        assert_eq!(store.request_rebuild(c, req(20)).await, Ok(1));
        assert_eq!(store.request_rebuild(b, req(10)).await, Ok(2));
        // Сначала давние запросы, затем старая схема; индекс текущей схемы без запроса — нет.
        assert_eq!(store.pending_rebuilds(2, 10).await, Ok(vec![b, c, a]));
        assert_eq!(store.pending_rebuilds(1, 10).await, Ok(vec![b, c]));
        assert_eq!(store.pending_rebuilds(2, 1).await, Ok(vec![b]));

        // Запрос во время перестройки — переключение получает `Conflict`.
        assert_eq!(
            store.put_manifest(b, 1, update(2, true)).await,
            Err(Error::Conflict)
        );
        assert_eq!(store.put_manifest(b, 2, update(2, false)).await, Ok(3));
        let kept = store.get_manifest(b).await.expect("b").rebuild;
        assert!(kept.is_some(), "без rebuilt запрос остаётся");
        assert_eq!(store.put_manifest(b, 3, update(2, true)).await, Ok(4));
        assert_eq!(store.get_manifest(b).await.expect("b").rebuild, None);
        assert_eq!(store.pending_rebuilds(2, 10).await, Ok(vec![c, a]));
    }

    fn out(rcpts: &[&str], at: OffsetDateTime) -> OutMessage {
        OutMessage {
            tenant: TenantId::DEFAULT,
            blob: BlobKey {
                tenant: TenantId::DEFAULT,
                hash: BlobHash([1; 32]),
                generation: Gen::INITIAL,
            },
            size: 100,
            from: "a@example.org".into(),
            dsn: DsnParams::default(),
            rcpts: rcpts.iter().map(|r| OutRcpt::new(*r)).collect(),
            next_attempt: at,
        }
    }

    #[tokio::test]
    async fn queue_claims_with_fencing() {
        let clock = ManualClock::default();
        let q = MemQueue::new(clock.shared());
        let t0 = clock.now();
        let sec = time::Duration::seconds;
        let lease = Duration::from_secs(300);
        assert!(matches!(
            q.enqueue(out(&[], t0)).await,
            Err(Error::Invalid(_))
        ));
        assert!(matches!(
            q.enqueue(out(&["x@a", "x@a"], t0)).await,
            Err(Error::Invalid(_))
        ));
        let id = q
            .enqueue(out(&["b@example.com", "c@example.com"], t0))
            .await
            .expect("enqueue");
        assert_eq!((q.len(), q.blobs().len()), (1, 1));

        assert!(
            q.claim(t0 - sec(1), 10, lease)
                .await
                .expect("early")
                .is_empty()
        );
        let c1 = q
            .claim(t0, 10, lease)
            .await
            .expect("claim")
            .pop()
            .expect("one");
        assert_eq!(
            (c1.claim.id, c1.claim.fencing, c1.until),
            (id, 1, t0 + sec(300))
        );
        assert_eq!((c1.queued_at, c1.rcpts.len()), (t0, 2));
        assert!(q.claim(t0, 10, lease).await.expect("leased").is_empty());

        // Временная неудача одному, доставка другому: лиза снята, повтор — по сроку.
        q.reschedule(&c1.claim, "b@example.com", t0 + sec(60), "451 later")
            .await
            .expect("reschedule");
        let delivered = RcptStatus::Delivered {
            reply: "250 ok".into(),
        };
        assert_eq!(
            q.complete(&c1.claim, "c@example.com", delivered.clone())
                .await,
            Ok(false)
        );
        assert_eq!(
            q.complete(&c1.claim, "c@example.com", delivered.clone())
                .await,
            Err(Error::NotFound)
        );
        assert_eq!(
            q.complete(&c1.claim, "nobody@x", delivered.clone()).await,
            Err(Error::NotFound)
        );
        assert!(
            q.claim(t0 + sec(30), 10, lease)
                .await
                .expect("not due")
                .is_empty()
        );
        let c2 = q
            .claim(t0 + sec(60), 10, lease)
            .await
            .expect("retry")
            .pop()
            .expect("one");
        assert_eq!(c2.claim.fencing, 2);
        assert_eq!(c2.rcpts.len(), 1);
        assert_eq!(
            (c2.rcpts[0].attempts, c2.rcpts[0].last_error.as_deref()),
            (1, Some("451 later"))
        );
        assert_eq!(q.renew(&c1.claim, t0).await, Err(Error::Conflict));
        assert_eq!(
            q.reschedule(&c1.claim, "b@example.com", t0, "x").await,
            Err(Error::Conflict)
        );
        q.renew(&c2.claim, t0 + sec(600)).await.expect("renew");
        assert!(
            q.claim(t0 + sec(400), 10, lease)
                .await
                .expect("renewed")
                .is_empty()
        );

        // Узел пропал: после лизы запись берёт другой, без увеличения `attempts`.
        let c3 = q
            .claim(t0 + sec(600), 10, lease)
            .await
            .expect("takeover")
            .pop()
            .expect("one");
        assert_eq!((c3.claim.fencing, c3.rcpts[0].attempts), (3, 1));
        assert_eq!(
            q.complete(&c2.claim, "b@example.com", delivered).await,
            Err(Error::Conflict)
        );
        let failed = RcptStatus::Failed {
            reply: "550 no".into(),
        };
        assert_eq!(
            q.complete(&c3.claim, "b@example.com", failed).await,
            Ok(true)
        );
        assert!(q.is_empty());
        assert_eq!(q.renew(&c3.claim, t0).await, Err(Error::Conflict));
    }

    #[tokio::test]
    async fn queue_claim_order_and_limit() {
        let clock = ManualClock::default();
        let q = MemQueue::new(clock.shared());
        let t0 = clock.now();
        let late = q
            .enqueue(out(&["a@x"], t0 + time::Duration::seconds(5)))
            .await
            .expect("late");
        let early = q.enqueue(out(&["a@x"], t0)).await.expect("early");
        let now = t0 + time::Duration::seconds(10);
        let first = q
            .claim(now, 1, Duration::from_secs(60))
            .await
            .expect("claim");
        assert_eq!(
            first.iter().map(|c| c.claim.id).collect::<Vec<_>>(),
            [early]
        );
        let rest = q
            .claim(now, 10, Duration::from_secs(60))
            .await
            .expect("claim");
        assert_eq!(rest.iter().map(|c| c.claim.id).collect::<Vec<_>>(), [late]);
    }

    fn new_message(n: u8, size: u64) -> NewMessage {
        NewMessage {
            skeleton: BlobKey {
                tenant: TenantId::DEFAULT,
                hash: BlobHash([n; 32]),
                generation: Gen::INITIAL,
            },
            parts: Vec::new(),
            size,
            received_at: OffsetDateTime::UNIX_EPOCH,
            flags: Flags::new(),
            meta: MessageMeta::default(),
        }
    }

    #[tokio::test]
    async fn trash_over_metadata() {
        let clock = ManualClock::default();
        let day = Duration::from_secs(24 * 3600);
        let meta = MemMetadata::new(clock.shared()).with_trash_retention(day * 10);
        let s = meta.account("user@example.com").await.expect("account");
        let inbox = meta.mailbox_by_role(s, SpecialUse::Inbox).expect("inbox");
        let m1 = meta
            .insert(s, inbox, new_message(1, 10))
            .await
            .expect("m1")
            .id;
        let m2 = meta
            .insert(s, inbox, new_message(2, 20))
            .await
            .expect("m2")
            .id;
        let trash = MemTrash::new(&meta);
        assert_eq!(trash.list(s, 0, 10).await, Ok(Vec::new()));

        let t1 = clock.now();
        meta.to_trash(s, &[m1]).await.expect("trash m1");
        clock.advance(Duration::from_secs(3600));
        // Политика изменилась: действует на удалённые после изменения.
        let meta = meta.with_trash_retention(day);
        meta.to_trash(s, &[m2]).await.expect("trash m2");
        let t2 = clock.now();

        let items = trash.list(s, 0, 10).await.expect("list");
        let msgs: Vec<_> = items.iter().map(|i| i.object).collect();
        assert_eq!(msgs, [TrashObject::Message(m2), TrashObject::Message(m1)]);
        assert_eq!(
            (items[1].origin, items[1].size, items[1].deleted_at),
            (inbox, 10, t1)
        );
        assert_eq!(items[1].expires_at, t1 + day * 10);
        assert_eq!(items[0].expires_at, t2 + day);
        assert_eq!(trash.list(s, 1, 10).await.expect("offset").len(), 1);
        assert_eq!(trash.list(s, 0, 1).await.expect("limit").len(), 1);

        assert!(trash.expired(t2, 10).await.expect("none").is_empty());
        let all = trash.expired(t1 + day * 11, 10).await.expect("expired");
        assert_eq!(all.iter().map(|i| i.object).collect::<Vec<_>>(), msgs);
        assert_eq!(
            trash.expired(t1 + day * 11, 1).await.expect("limit").len(),
            1
        );

        // Legal Hold: хук запретил — отложить.
        let hold = t1 + day * 100;
        trash
            .defer(s, TrashObject::Message(m1), hold)
            .await
            .expect("defer");
        let left = trash.expired(t1 + day * 11, 10).await.expect("expired");
        assert_eq!(
            left.iter().map(|i| i.object).collect::<Vec<_>>(),
            [TrashObject::Message(m2)]
        );

        let Restored::Message { mailbox, inserted } = trash
            .restore(s, TrashObject::Message(m2))
            .await
            .expect("restore")
        else {
            panic!("message expected");
        };
        assert_eq!((mailbox, inserted.id), (inbox, m2));
        // Восстановлено между `expired` и `expunge_final` (CTR-18): письмо остаётся.
        let ms = meta.expunge_final(s, &[m2]).await.expect("expunge");
        assert_eq!(ms, inserted.modseq, "ничего не изменилось");
        let kept = MessageStore::get(&meta, s, m2).await.expect("get");
        assert_eq!(kept.map(|m| m.mailbox), Some(inbox));
        assert_eq!(
            trash.restore(s, TrashObject::Message(m2)).await,
            Err(Error::NotFound)
        );
        assert_eq!(
            trash.defer(s, TrashObject::Message(m2), hold).await,
            Err(Error::NotFound)
        );
        assert_eq!(trash.list(s, 0, 10).await.expect("list").len(), 1);
    }

    #[tokio::test]
    async fn hidden_trash_changes_and_expiry() {
        use tp_contracts::events::CoreEventKind;
        use tp_contracts::meta::ChangeLog;

        let clock = ManualClock::default();
        let day = Duration::from_secs(24 * 3600);
        let meta = MemMetadata::new(clock.shared()).with_trash_retention(day);
        let s = meta.account("user@example.com").await.expect("account");
        let inbox = meta.mailbox_by_role(s, SpecialUse::Inbox).expect("inbox");
        let bin = meta.mailbox_by_role(s, SpecialUse::Trash).expect("trash");
        let m = meta
            .insert(s, inbox, new_message(1, 10))
            .await
            .expect("m")
            .id;
        let trashed = meta.to_trash(s, &[m]).await.expect("to trash");
        let trash = MemTrash::new(&meta);
        let out = trash
            .purge(s, &[m], &DecisionHooks::default())
            .await
            .expect("hide");
        assert_eq!(out.hidden, [m]);
        assert!(out.modseq > trashed);

        // Пользователь видит удаление из корзины; событие ядра — `Trashed`.
        let ch = meta
            .message_changes(s, Some(bin), trashed, 100)
            .await
            .expect("changes");
        assert_eq!(ch.destroyed.iter().map(|c| c.id).collect::<Vec<_>>(), [m]);
        assert_eq!(ch.new_state, out.modseq);
        let last = meta.event_log().all().pop().expect("event").event;
        assert_eq!(
            (last.mailbox, last.kind),
            (Some(bin), CoreEventKind::Trashed { message: m })
        );
        let status = tp_contracts::meta::MailboxStore::status(&meta, s, bin)
            .await
            .expect("status");
        assert_eq!((status.messages, status.size), (0, 0));

        // Срок корзины истекает и для скрытых: отсрочка и окончательное удаление.
        let later = clock.now() + day * 2;
        let expired = trash.expired(later, 10).await.expect("expired");
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].object, TrashObject::Message(m));
        trash
            .defer(s, TrashObject::Message(m), later + day)
            .await
            .expect("defer hidden");
        assert!(trash.expired(later, 10).await.expect("none").is_empty());
        let ms = meta.expunge_final(s, &[m]).await.expect("expunge");
        assert_eq!(
            ms, out.modseq,
            "скрытое удаляется без изменения для пользователя"
        );
        assert!(
            trash
                .list_hidden(s, 0, 10)
                .await
                .expect("hidden")
                .is_empty()
        );
        let last = meta.event_log().all().pop().expect("event").event;
        assert_eq!(last.kind, CoreEventKind::Purged { message: m });
        assert_eq!(
            trash.restore(s, TrashObject::Message(m)).await,
            Err(Error::NotFound)
        );
    }

    #[tokio::test]
    async fn quota_limits() {
        let meta = MemMetadata::new(ManualClock::default().shared());
        let s = meta.account("user@example.com").await.expect("account");
        let inbox = meta.mailbox_by_role(s, SpecialUse::Inbox).expect("inbox");
        meta.insert(s, inbox, new_message(1, 60))
            .await
            .expect("insert");
        let quota = MemQuota::new(&meta);
        let u = quota.usage(s).await.expect("usage");
        assert_eq!((u.used_bytes, u.messages, u.limit_bytes), (60, 1, None));
        quota.set_default_limit(Some(100));
        assert_eq!(quota.check(s, 40).await, Ok(()));
        assert_eq!(quota.check(s, 41).await, Err(Error::QuotaExceeded));
        quota.set_limit(s, None);
        assert_eq!(quota.check(s, 1000).await, Ok(()));
        quota.set_limit(s, Some(50));
        assert_eq!(quota.check(s, 0).await, Err(Error::QuotaExceeded));
        assert_eq!(quota.usage(scope()).await, Err(Error::NotFound));
    }

    #[tokio::test]
    async fn recording_hook() {
        let hook = RecordingDecisionHook::new("hold");
        let hooks = DecisionHooks::new(vec![Arc::new(hook.clone())]);
        let op = FinalDelete {
            scope: scope(),
            mailbox: MailboxId::new(),
            message: MessageId::new(),
            received_at: OffsetDateTime::UNIX_EPOCH,
        };
        assert_eq!(hooks.before_final_delete(&op).await, Ok(Decision::Allow));
        hook.set_final_delete(Decision::Deny("legal hold".into()));
        assert_eq!(
            hooks.before_final_delete(&op).await,
            Ok(Decision::Deny("legal hold".into()))
        );
        let sub = SubmissionInfo {
            principal: principal(TenantId::DEFAULT, "u"),
            mail_from: "u@example.com".into(),
            rcpt_to: vec!["x@example.org".into()],
            size: 1,
        };
        hook.set_submission(Decision::Deny("limit".into()));
        assert_eq!(
            hooks.on_submission(&sub).await,
            Ok(Decision::Deny("limit".into()))
        );
        assert_eq!(hook.name(), "hold");
        assert_eq!(
            hook.calls(),
            [
                HookCall::FinalDelete(op.clone()),
                HookCall::FinalDelete(op),
                HookCall::Submission(sub)
            ]
        );
    }
}
