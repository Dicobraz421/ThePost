//! Встроенный клиент ACME для профиля SMB (SRS 6.2): выпуск сертификата по HTTP-01 (ответ на проверку отдаёт
//! служебный HTTP — [`challenge_router`]), хранение цепочки и ключа на диске в файлах секции `tls`,
//! продление за `renew_before_days` до истечения, перечитывание [`TlsCerts`] без перезапуска.
//!
//! Учётная запись ACME сохраняется в `state_dir/account.yaml`, список доменов последнего выпуска — в
//! `state_dir/domains` (смена доменов вызывает перевыпуск). Срок действия читается из сертификата на диске.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use instant_acme::{
    Account, AccountBuilder, AccountCredentials, AuthorizationStatus, BodyWrapper, BytesResponse,
    ChallengeType, HttpClient, Identifier, NewAccount, NewOrder, OrderStatus, RetryPolicy,
};
use schemars::JsonSchema;
use serde::Deserialize;
use tp_contracts::Error;
use tp_contracts::role::Shutdown;

use crate::tls::{TlsCerts, TlsConfig};

/// Секция `acme`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AcmeConfig {
    /// Встроенный клиент ACME включён (SMB). Enterprise — файлы или секреты Kubernetes (cert-manager).
    pub enabled: bool,
    /// Адрес каталога ACME.
    pub directory_url: String,
    /// Контакты учётной записи (`mailto:admin@example.org`).
    pub contact: Vec<String>,
    /// Имена в сертификате (первое — основное). Обязательно при `enabled`.
    pub domains: Vec<String>,
    /// Каталог состояния: учётная запись ACME и список доменов последнего выпуска.
    pub state_dir: PathBuf,
    /// За сколько дней до истечения продлевать сертификат.
    pub renew_before_days: u32,
    /// Период проверки срока действия, с.
    pub check_interval_secs: u64,
    /// Пауза перед повтором после неудачного выпуска, с.
    pub retry_interval_secs: u64,
    /// Время ожидания проверки и выпуска на стороне ACME, с.
    pub order_timeout_secs: u64,
}

impl Default for AcmeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            directory_url: instant_acme::LetsEncrypt::Production.url().to_owned(),
            contact: Vec::new(),
            domains: Vec::new(),
            state_dir: PathBuf::from("/var/lib/thepost/acme"),
            renew_before_days: 30,
            check_interval_secs: 12 * 3600,
            retry_interval_secs: 3600,
            order_timeout_secs: 300,
        }
    }
}

impl AcmeConfig {
    fn renew_before(&self) -> Duration {
        Duration::from_secs(u64::from(self.renew_before_days) * 86_400)
    }
}

#[derive(Debug, thiserror::Error)]
enum AcmeError {
    #[error("acme: {0}")]
    Acme(#[from] instant_acme::Error),
    #[error("acme: {0}")]
    Protocol(String),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("acme state: {0}")]
    State(#[from] serde_yaml_ng::Error),
    #[error("acme.domains is empty")]
    NoDomains,
    #[error("acme: storage task failed")]
    Join,
}

impl From<AcmeError> for Error {
    fn from(e: AcmeError) -> Self {
        match e {
            AcmeError::Acme(_) | AcmeError::Protocol(_) => {
                // Подробности — в журнале: `Unavailable` сообщения не несёт.
                tracing::warn!(error = %e, "acme request failed");
                Error::UNAVAILABLE
            }
            AcmeError::NoDomains => Error::Invalid(e.to_string()),
            _ => Error::Internal(e.to_string()),
        }
    }
}

/// Ответы на проверки HTTP-01: токен → key authorization. Клонируется; заполняет [`Acme`], отдаёт служебный HTTP.
#[derive(Debug, Clone, Default)]
pub struct Http01Challenges(Arc<RwLock<HashMap<String, String>>>);

impl Http01Challenges {
    /// Ответ на токен.
    pub fn get(&self, token: &str) -> Option<String> {
        self.0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(token)
            .cloned()
    }

    fn insert(&self, token: String, key_authorization: String) {
        self.0
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(token, key_authorization);
    }

    fn remove(&self, token: &str) {
        self.0
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(token);
    }
}

/// Маршрут `/.well-known/acme-challenge/{token}` для служебного HTTP: `service_http::router(..).merge(..)`.
pub fn challenge_router(challenges: Http01Challenges) -> Router {
    Router::new()
        .route(
            "/.well-known/acme-challenge/{token}",
            get(challenge_handler),
        )
        .with_state(challenges)
}

async fn challenge_handler(
    State(challenges): State<Http01Challenges>,
    UrlPath(token): UrlPath<String>,
) -> Response {
    match challenges.get(&token) {
        Some(answer) => answer.into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Клиент ACME узла.
pub struct Acme {
    cfg: AcmeConfig,
    tls: TlsConfig,
    challenges: Http01Challenges,
    certs: Option<Arc<TlsCerts>>,
    http: Option<Arc<dyn HttpClient>>,
}

impl fmt::Debug for Acme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Acme")
            .field("directory_url", &self.cfg.directory_url)
            .field("domains", &self.cfg.domains)
            .finish_non_exhaustive()
    }
}

impl Acme {
    /// Клиент по секции `acme`; сертификат и ключ пишутся в файлы секции `tls`, затем перечитывается `certs`.
    pub fn new(
        cfg: AcmeConfig,
        tls: TlsConfig,
        challenges: Http01Challenges,
        certs: Option<Arc<TlsCerts>>,
    ) -> Self {
        Self {
            cfg,
            tls,
            challenges,
            certs,
            http: None,
        }
    }

    /// HTTP-клиент для обращений к ACME вместо встроенного (свои корневые сертификаты, прокси, тесты).
    pub fn with_http(mut self, http: Arc<dyn HttpClient>) -> Self {
        self.http = Some(http);
        self
    }

    /// Выпускает сертификат, если его нет, домены изменились или до истечения меньше `renew_before_days`.
    /// `true` — сертификат выпущен.
    pub async fn renew_if_needed(&self) -> tp_contracts::Result<bool> {
        self.renew_if_needed_at(SystemTime::now()).await
    }

    /// [`Self::renew_if_needed`] на момент `now`.
    pub async fn renew_if_needed_at(&self, now: SystemTime) -> tp_contracts::Result<bool> {
        if !self.needs_renewal_at(now).await? {
            return Ok(false);
        }
        self.issue().await?;
        Ok(true)
    }

    /// Нужен ли выпуск на момент `now`.
    pub async fn needs_renewal_at(&self, now: SystemTime) -> tp_contracts::Result<bool> {
        if self.cfg.domains.is_empty() {
            return Err(AcmeError::NoDomains.into());
        }
        let cert_file = self.tls.cert_file.clone();
        let domains_file = self.domains_file();
        let (not_after, issued_for) = tokio::task::spawn_blocking(move || {
            let not_after = std::fs::read_to_string(&cert_file)
                .ok()
                .and_then(|pem| pem_not_after(&pem));
            let issued_for = std::fs::read_to_string(&domains_file).ok();
            (not_after, issued_for)
        })
        .await
        .map_err(|_| AcmeError::Join)?;
        let Some(not_after) = not_after else {
            return Ok(true);
        };
        if issued_for.as_deref() != Some(self.domains_text().as_str()) {
            return Ok(true);
        }
        Ok(now + self.cfg.renew_before() >= not_after)
    }

    /// Выпускает сертификат безусловно: заказ, проверки HTTP-01, финализация, запись файлов, перечитывание TLS.
    pub async fn issue(&self) -> tp_contracts::Result<()> {
        Ok(self.try_issue().await?)
    }

    /// Проверяет сертификат каждые `check_interval_secs` (после ошибки — через `retry_interval_secs`) до сигнала остановки.
    pub async fn run(self, shutdown: Shutdown) {
        if !self.cfg.enabled {
            return;
        }
        loop {
            let wait = match self.renew_if_needed().await {
                Ok(issued) => {
                    if issued {
                        tracing::info!(domains = ?self.cfg.domains, "acme certificate issued");
                    }
                    self.cfg.check_interval_secs
                }
                Err(e) => {
                    tracing::warn!(error = %e, "acme renewal failed");
                    self.cfg.retry_interval_secs
                }
            };
            tokio::select! {
                () = shutdown.triggered() => return,
                () = tokio::time::sleep(Duration::from_secs(wait.max(1))) => {}
            }
        }
    }

    async fn try_issue(&self) -> Result<(), AcmeError> {
        if self.cfg.domains.is_empty() {
            return Err(AcmeError::NoDomains);
        }
        let account = self.account().await?;
        let identifiers: Vec<Identifier> = self
            .cfg
            .domains
            .iter()
            .map(|d| Identifier::Dns(d.clone()))
            .collect();
        let mut order = account.new_order(&NewOrder::new(&identifiers)).await?;

        let mut tokens = Vec::new();
        let result = async {
            let mut authorizations = order.authorizations();
            while let Some(authz) = authorizations.next().await {
                let mut authz = authz?;
                match authz.status {
                    AuthorizationStatus::Valid => continue,
                    AuthorizationStatus::Pending => {}
                    status => {
                        return Err(AcmeError::Protocol(format!("authorization is {status:?}")));
                    }
                }
                let mut challenge = authz.challenge(ChallengeType::Http01).ok_or_else(|| {
                    AcmeError::Protocol("no http-01 challenge offered".to_owned())
                })?;
                let token = challenge.token.clone();
                self.challenges.insert(
                    token.clone(),
                    challenge.key_authorization().as_str().to_owned(),
                );
                tokens.push(token);
                challenge.set_ready().await?;
            }
            let retry =
                RetryPolicy::new().timeout(Duration::from_secs(self.cfg.order_timeout_secs));
            let status = order.poll_ready(&retry).await?;
            if status != OrderStatus::Ready {
                return Err(AcmeError::Protocol(format!("order is {status:?}")));
            }
            let key_pem = order.finalize().await?;
            let chain_pem = order.poll_certificate(&retry).await?;
            Ok((key_pem, chain_pem))
        }
        .await;
        for token in &tokens {
            self.challenges.remove(token);
        }
        let (key_pem, chain_pem) = result?;
        check_pair(&chain_pem, &key_pem)?;

        let tls = self.tls.clone();
        let domains_file = self.domains_file();
        let domains = self.domains_text();
        tokio::task::spawn_blocking(move || {
            write_private(&tls.key_file, key_pem.as_bytes())?;
            write_atomic(&tls.cert_file, chain_pem.as_bytes(), false)?;
            write_atomic(&domains_file, domains.as_bytes(), false)
        })
        .await
        .map_err(|_| AcmeError::Join)??;
        if let Some(certs) = &self.certs {
            certs
                .reload()
                .await
                .map_err(|e| AcmeError::Protocol(format!("issued certificate rejected: {e}")))?;
        }
        Ok(())
    }

    /// Учётная запись из `account.yaml` или новая (сохраняется).
    async fn account(&self) -> Result<Account, AcmeError> {
        let path = self.cfg.state_dir.join("account.yaml");
        let read = path.clone();
        let stored = tokio::task::spawn_blocking(move || match std::fs::read_to_string(&read) {
            Ok(text) => Ok(Some(text)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(AcmeError::Io { path: read, source }),
        })
        .await
        .map_err(|_| AcmeError::Join)??;
        if let Some(text) = stored {
            let credentials: AccountCredentials = serde_yaml_ng::from_str(&text)?;
            return Ok(self.builder()?.from_credentials(credentials).await?);
        }
        let contact: Vec<&str> = self.cfg.contact.iter().map(String::as_str).collect();
        let (account, credentials) = self
            .builder()?
            .create(
                &NewAccount {
                    contact: &contact,
                    terms_of_service_agreed: true,
                    only_return_existing: false,
                },
                self.cfg.directory_url.clone(),
                None,
            )
            .await?;
        let text = serde_yaml_ng::to_string(&credentials)?;
        tokio::task::spawn_blocking(move || write_private(&path, text.as_bytes()))
            .await
            .map_err(|_| AcmeError::Join)??;
        Ok(account)
    }

    fn builder(&self) -> Result<AccountBuilder, AcmeError> {
        Ok(match &self.http {
            Some(http) => Account::builder_with_http(Box::new(SharedHttp(http.clone()))),
            None => Account::builder()?,
        })
    }

    fn domains_file(&self) -> PathBuf {
        self.cfg.state_dir.join("domains")
    }

    fn domains_text(&self) -> String {
        self.cfg.domains.join("\n")
    }
}

/// Разделяемый HTTP-клиент: `AccountBuilder` забирает клиент во владение.
struct SharedHttp(Arc<dyn HttpClient>);

impl HttpClient for SharedHttp {
    fn request(
        &self,
        req: axum::http::Request<BodyWrapper<axum::body::Bytes>>,
    ) -> Pin<Box<dyn Future<Output = Result<BytesResponse, instant_acme::Error>> + Send>> {
        self.0.request(req)
    }
}

/// Проверяет до записи на диск: цепочка читается, срок известен, ключ соответствует сертификату.
fn check_pair(chain_pem: &str, key_pem: &str) -> Result<(), AcmeError> {
    let bad = |what: &str| AcmeError::Protocol(format!("issued certificate rejected: {what}"));
    if pem_not_after(chain_pem).is_none() {
        return Err(bad("chain is not readable"));
    }
    let chain = rustls_pemfile::certs(&mut chain_pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| bad("chain is not readable"))?;
    let key = rustls_pemfile::private_key(&mut key_pem.as_bytes())
        .ok()
        .flatten()
        .ok_or_else(|| bad("key is not readable"))?;
    crate::tls::server_config(chain, key, crate::tls::TlsVersion::Tls13)
        .map_err(|e| bad(&e.to_string()))?;
    Ok(())
}

/// Закрытые данные (ключ, учётная запись): права 0600.
fn write_private(path: &Path, data: &[u8]) -> Result<(), AcmeError> {
    write_atomic(path, data, true)
}

/// Запись через временный файл и rename: читатель видит старую или новую версию целиком.
fn write_atomic(path: &Path, data: &[u8], private: bool) -> Result<(), AcmeError> {
    let io = |source| AcmeError::Io {
        path: path.to_owned(),
        source,
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = private;
    let mut file = options.open(&tmp).map_err(io)?;
    file.write_all(data).map_err(io)?;
    file.sync_all().map_err(io)?;
    std::fs::rename(&tmp, path).map_err(io)
}

/// `notAfter` первого сертификата PEM-цепочки.
fn pem_not_after(pem: &str) -> Option<SystemTime> {
    let der = rustls_pemfile::certs(&mut pem.as_bytes()).next()?.ok()?;
    cert_not_after(&der)
}

/// `notAfter` сертификата X.509 (DER): Certificate → TBSCertificate → [version] serial signature issuer validity.
fn cert_not_after(der: &[u8]) -> Option<SystemTime> {
    let (_, cert, _) = der_tlv(der).filter(|(tag, _, _)| *tag == 0x30)?;
    let (_, tbs, _) = der_tlv(cert).filter(|(tag, _, _)| *tag == 0x30)?;
    let mut rest = tbs;
    if rest.first() == Some(&0xa0) {
        rest = der_tlv(rest)?.2;
    }
    for _ in 0..3 {
        rest = der_tlv(rest)?.2; // serialNumber, signature, issuer
    }
    let (_, validity, _) = der_tlv(rest).filter(|(tag, _, _)| *tag == 0x30)?;
    let after_not_before = der_tlv(validity)?.2;
    let (tag, time, _) = der_tlv(after_not_before)?;
    der_time(tag, time)
}

/// Элемент DER: (тег, содержимое, остаток).
fn der_tlv(b: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let tag = *b.first()?;
    let first = usize::from(*b.get(1)?);
    let (len, head) = if first < 0x80 {
        (first, 2)
    } else {
        let n = first & 0x7f;
        if n == 0 || n > 4 {
            return None;
        }
        let len = b
            .get(2..2 + n)?
            .iter()
            .fold(0usize, |acc, x| (acc << 8) | usize::from(*x));
        (len, 2 + n)
    };
    let end = head.checked_add(len)?;
    Some((tag, b.get(head..end)?, b.get(end..)?))
}

/// UTCTime (`YYMMDDHHMMSSZ`) или GeneralizedTime (`YYYYMMDDHHMMSSZ`) → время.
fn der_time(tag: u8, value: &[u8]) -> Option<SystemTime> {
    let s = std::str::from_utf8(value).ok()?.strip_suffix('Z')?;
    let (year, rest) = match tag {
        0x17 if s.len() == 12 => {
            let yy: i64 = s.get(..2)?.parse().ok()?;
            (if yy < 50 { 2000 + yy } else { 1900 + yy }, s.get(2..)?)
        }
        0x18 if s.len() == 14 => (s.get(..4)?.parse().ok()?, s.get(4..)?),
        _ => return None,
    };
    let num = |r: std::ops::Range<usize>| rest.get(r)?.parse::<i64>().ok();
    let (month, day) = (num(0..2)?, num(2..4)?);
    let (hour, min, sec) = (num(4..6)?, num(6..8)?, num(8..10)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || min > 59 || sec > 60 {
        return None;
    }
    let secs = days_from_civil(year, month, day) * 86_400 + hour * 3600 + min * 60 + sec;
    Some(UNIX_EPOCH + Duration::from_secs(u64::try_from(secs).ok()?))
}

/// Дней от 1970-01-01 до даты григорианского календаря (H. Hinnant, `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll, Waker};

    use axum::body::Bytes;
    use axum::body::HttpBody as _;
    use axum::http::{Method, Request};

    use super::*;
    use crate::tls::tests::{CERT_A, CERT_B, KEY_A, TempDir, der};

    const BASE: &str = "https://acme.test";
    const TOKEN: &str = "tok-ABC_123";
    const DOMAIN: &str = "mail.example.org";

    /// Мок каталога ACME (RFC 8555) без сети: маршруты по методу и пути. Ответ на проверку HTTP-01 берётся
    /// из [`Http01Challenges`] — как его забрал бы сервер ACME; сертификат выпускается на ключ из CSR.
    struct MockAcme {
        challenges: Http01Challenges,
        /// Вместо выпуска по CSR отдать эту цепочку.
        fixed_chain: Option<String>,
        state: Mutex<MockState>,
        nonces: AtomicUsize,
    }

    #[derive(Default)]
    struct MockState {
        accounts: usize,
        orders: usize,
        validated: bool,
        chain: Option<String>,
    }

    type Reply = (u16, Option<String>, String);

    impl MockAcme {
        fn respond(&self, method: &Method, path: &str, body: &[u8]) -> Reply {
            let mut st = self.state.lock().expect("mock state");
            let order = |st: &MockState| {
                let status = match (st.validated, st.chain.is_some()) {
                    (_, true) => "valid",
                    (true, false) => "ready",
                    _ => "pending",
                };
                format!(
                    r#"{{"status":"{status}","authorizations":["{BASE}/authz/1"],"finalize":"{BASE}/order/1/finalize","certificate":"{BASE}/cert/1"}}"#
                )
            };
            let problem = |kind: &str| format!(r#"{{"type":"urn:ietf:params:acme:error:{kind}"}}"#);
            match (method.as_str(), path) {
                ("GET", "/directory") => (
                    200,
                    None,
                    format!(
                        r#"{{"newNonce":"{BASE}/nonce","newAccount":"{BASE}/account","newOrder":"{BASE}/order/new"}}"#
                    ),
                ),
                ("HEAD", "/nonce") => (200, None, String::new()),
                ("POST", "/account") => {
                    st.accounts += 1;
                    (
                        201,
                        Some("/acct/1".to_owned()),
                        r#"{"status":"valid"}"#.to_owned(),
                    )
                }
                ("POST", "/order/new") => {
                    st.orders += 1;
                    st.validated = false;
                    st.chain = None;
                    (201, Some("/order/1".to_owned()), order(&st))
                }
                ("POST", "/authz/1") => {
                    let status = if st.validated { "valid" } else { "pending" };
                    (
                        200,
                        None,
                        format!(
                            r#"{{"identifier":{{"type":"dns","value":"{DOMAIN}"}},"status":"{status}","challenges":[{{"type":"dns-01","url":"{BASE}/chall/2","token":"x","status":"pending"}},{{"type":"http-01","url":"{BASE}/chall/1","token":"{TOKEN}","status":"pending"}}]}}"#
                        ),
                    )
                }
                ("POST", "/chall/1") => match self.challenges.get(TOKEN) {
                    // Ключ авторизации — «токен.отпечаток ключа учётной записи».
                    Some(answer) if answer.starts_with(&format!("{TOKEN}.")) => {
                        st.validated = true;
                        (
                            200,
                            None,
                            format!(
                                r#"{{"type":"http-01","url":"{BASE}/chall/1","token":"{TOKEN}","status":"valid"}}"#
                            ),
                        )
                    }
                    _ => (403, None, problem("unauthorized")),
                },
                ("POST", "/order/1") => (200, None, order(&st)),
                ("POST", "/order/1/finalize") => {
                    if !st.validated {
                        return (403, None, problem("orderNotReady"));
                    }
                    let chain = match &self.fixed_chain {
                        Some(chain) => chain.clone(),
                        None => cert_for_csr(body),
                    };
                    st.chain = Some(chain);
                    (200, None, order(&st))
                }
                ("POST", "/cert/1") => (200, None, st.chain.clone().unwrap_or_default()),
                _ => (404, None, problem("malformed")),
            }
        }
    }

    impl HttpClient for MockAcme {
        fn request(
            &self,
            req: Request<BodyWrapper<Bytes>>,
        ) -> Pin<Box<dyn Future<Output = Result<BytesResponse, instant_acme::Error>> + Send>>
        {
            let (head, mut body) = req.into_parts();
            // Тело запроса — готовые байты: кадр доступен при первом опросе.
            let body = match Pin::new(&mut body).poll_frame(&mut Context::from_waker(Waker::noop()))
            {
                Poll::Ready(Some(frame)) => frame.expect("frame").into_data().unwrap_or_default(),
                Poll::Ready(None) => Bytes::new(),
                Poll::Pending => panic!("request body is not ready"),
            };
            let (status, location, reply) = self.respond(&head.method, head.uri.path(), &body);
            let nonce = self.nonces.fetch_add(1, Ordering::SeqCst);
            let mut rsp = axum::http::Response::builder()
                .status(status)
                .header("replay-nonce", format!("nonce-{nonce}"));
            if let Some(location) = location {
                rsp = rsp.header("location", format!("{BASE}{location}"));
            }
            let (parts, ()) = rsp.body(()).expect("response").into_parts();
            Box::pin(async move {
                Ok(BytesResponse {
                    parts,
                    body: Box::new(Bytes::from(reply)),
                })
            })
        }
    }

    /// Сертификат на открытый ключ из CSR запроса finalize (JWS → payload → `csr`).
    /// Подпись фиктивная: сервер TLS свою цепочку не проверяет, rustls сверяет только ключ.
    fn cert_for_csr(jws: &[u8]) -> String {
        let jws: HashMap<String, String> = serde_yaml_ng::from_slice(jws).expect("jws json");
        let payload = b64url_decode(&jws["payload"]);
        let payload: HashMap<String, String> =
            serde_yaml_ng::from_slice(&payload).expect("payload json");
        let csr = b64url_decode(&payload["csr"]);
        // CertificationRequest → CertificationRequestInfo → version, subject, subjectPKInfo.
        let (_, req, _) = der_tlv(&csr).expect("csr");
        let (_, info, _) = der_tlv(req).expect("info");
        let after_version = der_tlv(info).expect("version").2;
        let spki_and_rest = der_tlv(after_version).expect("subject").2;
        let rest = der_tlv(spki_and_rest).expect("spki").2;
        let spki = &spki_and_rest[..spki_and_rest.len() - rest.len()];

        let alg = tlv(
            0x30,
            &[0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02],
        );
        let cn = [
            &[0x06, 0x03, 0x55, 0x04, 0x03][..],
            &tlv(0x0c, DOMAIN.as_bytes()),
        ]
        .concat();
        let name = tlv(0x30, &tlv(0x31, &tlv(0x30, &cn)));
        let validity = tlv(
            0x30,
            &[tlv(0x17, b"260101000000Z"), tlv(0x18, b"21000101000000Z")].concat(),
        );
        let tbs = tlv(
            0x30,
            &[
                tlv(0xa0, &tlv(0x02, &[2])),
                tlv(0x02, &[1]),
                alg.clone(),
                name.clone(),
                validity,
                name,
                spki.to_vec(),
            ]
            .concat(),
        );
        let sig = tlv(0x03, &[0, 0x30, 0x06, 0x02, 0x01, 0x01, 0x02, 0x01, 0x01]);
        let cert = tlv(0x30, &[tbs, alg, sig].concat());
        let b64 = b64_encode(&cert);
        let lines: Vec<&str> = b64
            .as_bytes()
            .chunks(64)
            .map(|c| std::str::from_utf8(c).expect("ascii"))
            .collect();
        format!(
            "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
            lines.join("\n")
        )
    }

    fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        let len = content.len();
        if len < 0x80 {
            out.push(u8::try_from(len).expect("short"));
        } else {
            let bytes = len.to_be_bytes();
            let skip = bytes.iter().take_while(|b| **b == 0).count();
            out.push(0x80 | u8::try_from(bytes.len() - skip).expect("len"));
            out.extend_from_slice(&bytes[skip..]);
        }
        out.extend_from_slice(content);
        out
    }

    const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    fn b64_encode(data: &[u8]) -> String {
        let mut out = String::new();
        for chunk in data.chunks(3) {
            let n = chunk.iter().fold(0u32, |acc, b| (acc << 8) | u32::from(*b))
                << (8 * (3 - chunk.len()));
            for i in 0..4 {
                if i <= chunk.len() {
                    out.push(char::from(B64[((n >> (18 - 6 * i)) & 63) as usize]));
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    fn b64url_decode(s: &str) -> Vec<u8> {
        let mut bits = 0u32;
        let mut n = 0;
        let mut out = Vec::new();
        for c in s.bytes() {
            let v = match c {
                b'A'..=b'Z' => c - b'A',
                b'a'..=b'z' => c - b'a' + 26,
                b'0'..=b'9' => c - b'0' + 52,
                b'-' => 62,
                b'_' => 63,
                _ => continue,
            };
            bits = (bits << 6) | u32::from(v);
            n += 6;
            if n >= 8 {
                n -= 8;
                out.push(((bits >> n) & 0xff) as u8);
            }
        }
        out
    }

    fn setup(dir: &Path, fixed_chain: Option<&str>) -> (Acme, Arc<MockAcme>, TlsConfig) {
        let challenges = Http01Challenges::default();
        let mock = Arc::new(MockAcme {
            challenges: challenges.clone(),
            fixed_chain: fixed_chain.map(str::to_owned),
            state: Mutex::new(MockState::default()),
            nonces: AtomicUsize::new(0),
        });
        let tls = TlsConfig {
            cert_file: dir.join("tls/fullchain.pem"),
            key_file: dir.join("tls/privkey.pem"),
            ..TlsConfig::default()
        };
        let cfg = AcmeConfig {
            enabled: true,
            directory_url: format!("{BASE}/directory"),
            contact: vec!["mailto:admin@example.org".to_owned()],
            domains: vec![DOMAIN.to_owned()],
            state_dir: dir.join("acme"),
            ..AcmeConfig::default()
        };
        let acme = Acme::new(cfg, tls.clone(), challenges, None).with_http(mock.clone());
        (acme, mock, tls)
    }

    #[tokio::test]
    async fn issues_and_renews_with_mock_directory() {
        let dir = TempDir::new("acme-issue");
        let (acme, mock, tls) = setup(&dir.0, None);
        let now = SystemTime::now();
        assert!(
            acme.needs_renewal_at(now).await.expect("check"),
            "no certificate yet"
        );
        assert!(acme.renew_if_needed_at(now).await.expect("issue"));

        let chain = std::fs::read_to_string(&tls.cert_file).expect("chain");
        let key = std::fs::read_to_string(&tls.key_file).expect("key");
        assert!(key.contains("PRIVATE KEY"));
        check_pair(&chain, &key).expect("key matches certificate");
        assert!(dir.0.join("acme/account.yaml").exists());
        assert_eq!(
            std::fs::read_to_string(dir.0.join("acme/domains")).expect("domains"),
            DOMAIN
        );
        assert_eq!(
            acme.challenges.get(TOKEN),
            None,
            "answer removed after order"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&tls.key_file)
                .expect("meta")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        // Действует до 2100 года: продление — за 30 дней до истечения, учётная запись переиспользуется.
        let not_after = pem_not_after(&chain).expect("not after");
        assert!(!acme.renew_if_needed_at(now).await.expect("fresh"));
        let day = Duration::from_secs(86_400);
        assert!(
            !acme
                .needs_renewal_at(not_after - 31 * day)
                .await
                .expect("check")
        );
        assert!(
            acme.renew_if_needed_at(not_after - 29 * day)
                .await
                .expect("renew")
        );
        let st = mock.state.lock().expect("state");
        assert_eq!((st.accounts, st.orders), (1, 2));
    }

    #[tokio::test]
    async fn issued_certificate_is_reloaded_into_tls() {
        let dir = TempDir::new("acme-reload");
        let (acme, _mock, tls) = setup(&dir.0, None);
        std::fs::create_dir_all(dir.0.join("tls")).expect("dir");
        std::fs::write(&tls.cert_file, CERT_A).expect("cert");
        std::fs::write(&tls.key_file, KEY_A).expect("key");
        let certs = Arc::new(TlsCerts::load(&tls).await.expect("certs"));
        let before = certs.server_config();
        let acme = Acme {
            certs: Some(certs.clone()),
            ..acme
        };
        // Сертификат есть, но выпущен не этим клиентом (нет `domains`) → перевыпуск.
        assert!(acme.renew_if_needed().await.expect("issue"));
        assert!(
            !Arc::ptr_eq(&before, &certs.server_config()),
            "tls reloaded"
        );
        let chain = std::fs::read_to_string(&tls.cert_file).expect("chain");
        assert_ne!(der(&chain), der(CERT_A));

        let acme = Acme {
            cfg: AcmeConfig {
                domains: vec![DOMAIN.to_owned(), "imap.example.org".to_owned()],
                ..acme.cfg.clone()
            },
            ..acme
        };
        assert!(
            acme.needs_renewal_at(SystemTime::now())
                .await
                .expect("domains changed")
        );
    }

    #[tokio::test]
    async fn failures_leave_files_untouched() {
        let dir = TempDir::new("acme-fail");
        // Цепочка не соответствует выпущенному ключу.
        let (acme, mock, tls) = setup(&dir.0, Some(CERT_B));
        assert!(acme.issue().await.expect_err("mismatch").is_retryable());
        assert!(!tls.cert_file.exists() && !tls.key_file.exists());
        assert!(mock.state.lock().expect("state").chain.is_some());

        // Ответ на проверку не опубликован: сервер ACME отклоняет проверку, до финализации не доходит.
        let (acme, mock, tls) = setup(&dir.0, None);
        let acme = Acme {
            challenges: Http01Challenges::default(),
            ..acme
        };
        assert!(acme.issue().await.expect_err("validation").is_retryable());
        assert!(!tls.cert_file.exists());
        assert!(mock.state.lock().expect("state").chain.is_none());

        let acme = Acme {
            cfg: AcmeConfig {
                domains: Vec::new(),
                ..acme.cfg.clone()
            },
            ..acme
        };
        assert!(matches!(acme.issue().await, Err(Error::Invalid(_))));
    }

    #[tokio::test]
    async fn challenge_is_served_over_service_http() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let challenges = Http01Challenges::default();
        challenges.insert("tok".to_owned(), "tok.thumb".to_owned());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server =
            tokio::spawn(axum::serve(listener, challenge_router(challenges)).into_future());
        for (path, expect) in [
            ("/.well-known/acme-challenge/tok", "HTTP/1.1 200"),
            ("/.well-known/acme-challenge/other", "HTTP/1.1 404"),
        ] {
            let mut s = tokio::net::TcpStream::connect(addr).await.expect("connect");
            let req = format!("GET {path} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n");
            s.write_all(req.as_bytes()).await.expect("write");
            let mut resp = String::new();
            s.read_to_string(&mut resp).await.expect("read");
            assert!(resp.starts_with(expect), "{resp}");
            if expect.ends_with("200") {
                assert!(resp.ends_with("tok.thumb"), "{resp}");
            }
        }
        server.abort();
    }

    #[test]
    fn certificate_expiry_is_parsed() {
        // cert-a: notAfter 2126-10-06 11:35:19 UTC (GeneralizedTime).
        let not_after = pem_not_after(CERT_A).expect("parse");
        let secs = not_after
            .duration_since(UNIX_EPOCH)
            .expect("epoch")
            .as_secs();
        let days = u64::try_from(days_from_civil(2126, 10, 6)).expect("days");
        assert_eq!(secs, days * 86_400 + 11 * 3600 + 35 * 60 + 19);
        let days = u64::try_from(days_from_civil(2049, 12, 31)).expect("days");
        assert_eq!(
            der_time(0x17, b"491231235959Z"),
            Some(UNIX_EPOCH + Duration::from_secs(days * 86_400 + 86_399))
        );
        assert_eq!(der_time(0x17, b"700101000000Z"), Some(UNIX_EPOCH));
        assert_eq!(der_time(0x17, b"701301000000Z"), None);
        assert_eq!(der_time(0x18, b"2026103011Z"), None);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(pem_not_after("not a pem"), None);
        assert_eq!(cert_not_after(&[0x30, 0x82, 0xff]), None);
    }

    #[test]
    fn config_from_yaml() {
        let cfg: AcmeConfig = serde_yaml_ng::from_str(
            "enabled: true\ndomains: [mail.example.org]\ncontact: [\"mailto:a@example.org\"]\n",
        )
        .expect("parse");
        assert!(cfg.enabled);
        assert_eq!(cfg.renew_before_days, 30);
        assert!(
            cfg.directory_url
                .starts_with("https://acme-v02.api.letsencrypt.org")
        );
        assert!(serde_yaml_ng::from_str::<AcmeConfig>("renew: 1").is_err());
    }
}
