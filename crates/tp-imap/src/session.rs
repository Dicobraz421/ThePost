//! Сессия IMAP4rev2 (RFC 9051): состояния, `CAPABILITY`, `STARTTLS`, `LOGIN`, `AUTHENTICATE` (SASL, SASL-IR),
//! `ENABLE`, `ID`, `NOOP`, `LOGOUT`; остановка узла с `BYE`.
//!
//! Сессия работает над любым [`Transport`] (соединение `tp-platform` или поток в тестах). Ответы копятся
//! и отправляются, когда входной буфер исчерпан (конвейер команд) или превышен
//! [`Limits::write_buffer`](crate::config::Limits). Пароль открытым текстом без TLS не принимается:
//! `LOGINDISABLED`, механизмы SASL — только [`SaslServer::mechanisms`] с учётом TLS. После `STARTTLS`
//! непрочитанный остаток буфера отбрасывается: команды, пришедшие открытым текстом до рукопожатия,
//! не исполняются после него.
//!
//! Остановка узла ([`SessionCtx::shutdown`]): сессия, ожидающая команду, отвечает `* BYE` и закрывается;
//! [`Server::serve`] перестаёт принимать соединения и ждёт сессии не дольше
//! [`Timeouts::drain`](crate::config::Timeouts).

use std::borrow::Cow;
use std::fmt;
use std::future::Future;
use std::num::NonZeroU32;
use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tp_contracts::auth::{Authenticator, Credentials, Principal};
use tp_contracts::mailstore::MailStore;
use tp_contracts::meta::{AccountScope, MailboxStore, MessageStore};
use tp_contracts::sasl::{Mechanism, SaslContext, SaslFailure, SaslServer, SaslStep};
use tp_contracts::search::SearchIndex;
use tp_contracts::{Error, MailboxId, MessageId, Result, Uid, UidValidity};
use tp_platform::{Conn, Listener};

use crate::config::ImapConfig;
use crate::proto::types::auth::AuthMechanism;
use crate::proto::types::command::{CommandBody, FetchModifier, SelectParameter};
use crate::proto::types::core::{AString, Atom, IString, NString, Tag, Vec1};
use crate::proto::types::extensions::enable::CapabilityEnable;
use crate::proto::types::fetch::MessageDataItem;
use crate::proto::types::response::{
    Capability, Code, CommandContinuationRequest, Data, Greeting, GreetingKind, Response, Status,
    StatusBody, StatusKind,
};
use crate::proto::types::secret::Secret;
use crate::proto::{self, Body, Command, Expect, Input, Reader};
use crate::search::SearchCmd;

/// Канал сессии: поток байтов, признак TLS, апгрейд STARTTLS.
pub trait Transport: Send + Sized + 'static {
    /// Поток данных.
    type Io: AsyncRead + AsyncWrite + Unpin + Send;

    /// Поток данных.
    fn io(&mut self) -> &mut Self::Io;

    /// Канал защищён TLS.
    fn tls(&self) -> bool;

    /// Можно предложить STARTTLS.
    fn can_starttls(&self) -> bool;

    /// Рукопожатие TLS после ответа на `STARTTLS`.
    fn starttls(self) -> impl Future<Output = Result<Self>> + Send;
}

impl Transport for Conn {
    type Io = tp_platform::listen::Stream;

    fn io(&mut self) -> &mut Self::Io {
        &mut self.stream
    }

    fn tls(&self) -> bool {
        self.info.tls
    }

    fn can_starttls(&self) -> bool {
        Conn::can_starttls(self)
    }

    fn starttls(self) -> impl Future<Output = Result<Self>> + Send {
        Conn::starttls(self)
    }
}

/// Реализации контрактов, с которыми работает протокол.
#[derive(Clone)]
pub struct Deps {
    /// Механизмы SASL (`AUTHENTICATE`).
    pub sasl: Arc<dyn SaslServer>,
    /// Проверка пароля (`LOGIN`).
    pub auth: Arc<dyn Authenticator>,
    /// Папки.
    pub mailboxes: Arc<dyn MailboxStore>,
    /// Письма (метаданные).
    pub messages: Arc<dyn MessageStore>,
    /// Почтовое хранилище (содержимое, флаги).
    pub store: Arc<dyn MailStore>,
    /// Полнотекстовый поиск (`SEARCH BODY`, `TEXT`).
    pub search: Arc<dyn SearchIndex>,
}

impl fmt::Debug for Deps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Deps").finish_non_exhaustive()
    }
}

/// Общее для сессий слушателя.
#[derive(Debug, Clone)]
pub struct SessionCtx {
    /// Секция `imap` на момент соединения.
    pub config: Arc<ImapConfig>,
    /// Зависимости.
    pub deps: Deps,
    /// Остановка узла: сессии отвечают `BYE`.
    pub shutdown: CancellationToken,
}

/// Выбранная папка (`SELECT`/`EXAMINE`): снимок писем для номеров последовательности.
#[derive(Debug, Clone)]
pub(crate) struct Selected {
    /// Папка.
    pub mailbox: MailboxId,
    /// `EXAMINE`: только чтение.
    pub read_only: bool,
    /// UIDVALIDITY на момент выбора.
    #[allow(dead_code)]
    pub uid_validity: UidValidity,
    /// Письма по возрастанию UID; номер последовательности — индекс + 1.
    pub messages: Vec<(Uid, MessageId)>,
}

/// Сессия IMAP одного соединения.
pub struct Session<T: Transport> {
    pub(crate) ctx: Arc<SessionCtx>,
    conn: Option<T>,
    reader: Reader,
    pub(crate) out: Vec<u8>,
    pub(crate) principal: Option<Principal>,
    pub(crate) selected: Option<Selected>,
    /// Клиент включил IMAP4rev2 (`ENABLE IMAP4rev2`): имена папок в UTF-8, без `RECENT`.
    pub(crate) rev2: bool,
    /// CONDSTORE включён (`ENABLE CONDSTORE` или команда с `MODSEQ`, RFC 7162, 3.1): `MODSEQ` в ответах
    /// `FETCH` и `HIGHESTMODSEQ` при выборе папки.
    pub(crate) condstore: bool,
    errors: u32,
    auth_failures: u32,
    /// Сессия завершается после отправки накопленных ответов.
    close: bool,
}

impl<T: Transport> fmt::Debug for Session<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("authenticated", &self.principal.is_some())
            .field("selected", &self.selected.as_ref().map(|s| s.mailbox))
            .field("rev2", &self.rev2)
            .finish_non_exhaustive()
    }
}

fn cap(name: &'static str) -> Capability<'static> {
    Capability::from(Atom::unvalidated(name))
}

/// Расширения, не зависящие от состояния.
const EXTENSIONS: &[&str] = &[
    "IMAP4rev1",
    "IMAP4rev2",
    "LITERAL+",
    "ENABLE",
    "ID",
    "SASL-IR",
    "UNSELECT",
    "CHILDREN",
    "LIST-EXTENDED",
    "LIST-STATUS",
    "SPECIAL-USE",
    "CREATE-SPECIAL-USE",
    "STATUS=SIZE",
    "BINARY",
    "CONDSTORE",
    "ESEARCH",
    "UIDPLUS",
    "MOVE",
];

fn reader(config: &ImapConfig) -> Reader {
    let l = &config.limits;
    Reader::new(l.max_message_size.saturating_add(l.max_command_size))
}

impl<T: Transport> Session<T> {
    /// Сессия соединения.
    pub fn new(ctx: Arc<SessionCtx>, conn: T) -> Self {
        let reader = reader(&ctx.config);
        Self {
            ctx,
            conn: Some(conn),
            reader,
            out: Vec::new(),
            principal: None,
            selected: None,
            rev2: false,
            condstore: false,
            errors: 0,
            auth_failures: 0,
            close: false,
        }
    }

    /// Ведёт сессию до `LOGOUT`, закрытия клиентом, таймаута или остановки узла.
    pub async fn run(mut self) -> Result<()> {
        let greeting = Greeting {
            kind: GreetingKind::Ok,
            code: Some(Code::Capability(self.capabilities())),
            text: proto::text("The Post IMAP4rev2 ready"),
        };
        self.out.extend(proto::encode_greeting(&greeting));
        while !self.close {
            let Some(input) = self.read_input(Expect::Command).await? else {
                break;
            };
            self.handle(input).await?;
        }
        self.flush().await
    }

    // ---------- Ввод-вывод ----------

    fn tls(&self) -> bool {
        self.conn.as_ref().is_some_and(Transport::tls)
    }

    pub(crate) async fn flush(&mut self) -> Result<()> {
        if self.out.is_empty() {
            return Ok(());
        }
        let Some(conn) = self.conn.as_mut() else {
            self.out.clear();
            return Ok(());
        };
        let io = conn.io();
        io.write_all(&self.out).await.map_err(io_error)?;
        io.flush().await.map_err(io_error)?;
        self.out.clear();
        Ok(())
    }

    /// Отправляет накопленное, если буфер ответа велик (`FETCH` многих писем).
    pub(crate) async fn maybe_flush(&mut self) -> Result<()> {
        if self.out.len() >= self.ctx.config.limits.write_buffer {
            self.flush().await?;
        }
        Ok(())
    }

    /// Следующее сообщение клиента; `None` — сессия закончена (клиент закрыл соединение, таймаут,
    /// остановка узла — `BYE` уже отправлен).
    pub(crate) async fn read_input(&mut self, expect: Expect) -> Result<Option<Input>> {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            while let Some(input) = self.reader.next(expect) {
                match input {
                    Input::Literal { tag, length, sync } => {
                        if !self.literal(tag, length, sync) {
                            return Ok(None);
                        }
                    }
                    other => return Ok(Some(other)),
                }
            }
            self.flush().await?;
            let timeout = if self.principal.is_some() {
                self.ctx.config.timeouts.autologout
            } else {
                self.ctx.config.timeouts.login
            };
            let shutdown = self.ctx.shutdown.clone();
            let Some(conn) = self.conn.as_mut() else {
                return Ok(None);
            };
            let io = conn.io();
            let read = tokio::select! {
                biased;
                _ = shutdown.cancelled() => None,
                r = tokio::time::timeout(timeout, io.read(&mut buf)) => Some(r),
            };
            match read {
                None => {
                    self.bye(Some(proto::code("UNAVAILABLE")), "Server shutting down");
                    return self.end().await;
                }
                Some(Err(_)) => {
                    self.bye(None, "Autologout: idle for too long");
                    return self.end().await;
                }
                Some(Ok(Ok(0))) => return Ok(None),
                Some(Ok(Ok(n))) => self.reader.push(&buf[..n]),
                Some(Ok(Err(e))) => return Err(io_error(e)),
            }
        }
    }

    async fn end(&mut self) -> Result<Option<Input>> {
        self.close = true;
        self.flush().await?;
        Ok(None)
    }

    /// Объявлен литерал: `+` для синхронизирующего; больше лимита — отказ (синхронизирующий клиент
    /// не пришлёт) или `BYE` (LITERAL+ уже в пути). `false` — сессия закрывается.
    fn literal(&mut self, tag: Option<Tag<'static>>, length: u32, sync: bool) -> bool {
        if length > self.ctx.config.limits.max_message_size {
            if !sync {
                self.bye(Some(Code::TooBig), "Literal too large");
                self.close = true;
                return false;
            }
            self.reader.reject_literal();
            let code = Some(Code::TooBig);
            match tag {
                Some(tag) => self.no(&tag, code, "Literal too large"),
                None => self.untagged(StatusKind::Bad, code, "Literal too large"),
            }
            return true;
        }
        if sync {
            self.push(&Response::CommandContinuationRequest(
                CommandContinuationRequest::basic(None, "Ready for literal data")
                    .unwrap_or(CommandContinuationRequest::Base64(Cow::Borrowed(b""))),
            ));
        }
        true
    }

    // ---------- Ответы ----------

    pub(crate) fn push(&mut self, response: &Response<'_>) {
        self.out.extend(proto::encode(response));
    }

    pub(crate) fn data(&mut self, data: Data<'static>) {
        self.push(&Response::Data(data));
    }

    /// Ответ `* n FETCH (…)` (см. [`proto::fetch_line`]).
    pub(crate) fn fetch_data(
        &mut self,
        seq: NonZeroU32,
        items: Vec1<MessageDataItem<'static>>,
        origins: &[(usize, u32)],
    ) {
        self.out.extend(proto::fetch_line(seq, items, origins));
    }

    fn tagged(
        &mut self,
        tag: &Tag<'static>,
        kind: StatusKind,
        code: Option<Code<'static>>,
        text: &str,
    ) {
        let r = proto::tagged(tag, kind, code, text);
        self.push(&r);
    }

    pub(crate) fn ok(&mut self, tag: &Tag<'static>, code: Option<Code<'static>>, text: &str) {
        self.tagged(tag, StatusKind::Ok, code, text);
    }

    pub(crate) fn no(&mut self, tag: &Tag<'static>, code: Option<Code<'static>>, text: &str) {
        self.tagged(tag, StatusKind::No, code, text);
    }

    /// `BAD`: ошибочная команда; превышен лимит ошибок — `BYE`.
    pub(crate) fn bad(&mut self, tag: Option<&Tag<'static>>, text: &str) {
        match tag {
            Some(tag) => self.tagged(tag, StatusKind::Bad, None, text),
            None => self.untagged(StatusKind::Bad, None, text),
        }
        self.errors += 1;
        if self.errors >= self.ctx.config.limits.max_errors {
            self.bye(None, "Too many invalid commands");
            self.close = true;
        }
    }

    pub(crate) fn untagged(&mut self, kind: StatusKind, code: Option<Code<'static>>, text: &str) {
        self.push(&Response::Status(Status::Untagged(StatusBody {
            kind,
            code,
            text: proto::text(text),
        })));
    }

    fn bye(&mut self, code: Option<Code<'static>>, text: &str) {
        self.push(&Response::Status(Status::Bye(
            crate::proto::types::response::Bye {
                code,
                text: proto::text(text),
            },
        )));
    }

    /// Ответ на ошибку хранилища.
    pub(crate) fn store_error(&mut self, tag: &Tag<'static>, e: &Error) {
        let (code, text) = match e {
            Error::NotFound => ("NONEXISTENT", "No such mailbox or message"),
            Error::Conflict => ("ALREADYEXISTS", "Already exists"),
            Error::Forbidden => ("NOPERM", "Permission denied"),
            Error::QuotaExceeded => ("OVERQUOTA", "Quota exceeded"),
            Error::Invalid(_) => ("CANNOT", "Operation not possible"),
            Error::Unavailable { .. } => ("UNAVAILABLE", "Temporary failure, try again"),
            _ => ("SERVERBUG", "Internal error"),
        };
        if matches!(e, Error::Internal(_)) {
            tracing::warn!(error = %e, "imap: ошибка хранилища");
        }
        self.no(tag, Some(proto::code(code)), text);
    }

    pub(crate) fn capabilities(&self) -> Vec1<Capability<'static>> {
        let mut caps: Vec<Capability<'static>> = EXTENSIONS.iter().map(|c| cap(c)).collect();
        if self.principal.is_none() {
            let conn = self.conn.as_ref();
            if conn.is_some_and(Transport::can_starttls) {
                caps.push(Capability::StartTls);
            }
            let tls = self.tls();
            if !tls {
                caps.push(Capability::LoginDisabled);
            }
            for m in self.ctx.deps.sasl.mechanisms(tls) {
                if let Ok(m) = AuthMechanism::try_from(m.name()) {
                    caps.push(Capability::Auth(m));
                }
            }
        }
        Vec1::try_from(caps).unwrap_or_else(|_| Vec1::from(Capability::Imap4Rev1))
    }

    /// Контекст операций хранилища; `None` — не выполнен вход.
    pub(crate) fn scope(&self) -> Option<AccountScope> {
        self.principal.as_ref().map(|p| AccountScope {
            tenant: p.tenant,
            account: p.account,
        })
    }

    // ---------- Команды ----------

    async fn handle(&mut self, input: Input) -> Result<()> {
        match input {
            Input::Command(cmd) => self.command(cmd).await,
            Input::Bad { tag, reason } => {
                self.bad(tag.as_ref(), reason);
                Ok(())
            }
            Input::TooLong { tag } => {
                match tag {
                    Some(tag) => self.tagged(&tag, StatusKind::Bad, Some(Code::TooBig), "Too long"),
                    None => self.untagged(StatusKind::Bad, Some(Code::TooBig), "Too long"),
                }
                Ok(())
            }
            Input::AuthData(_) | Input::IdleDone | Input::Literal { .. } => {
                self.bad(None, "Unexpected data");
                Ok(())
            }
        }
    }

    async fn command(&mut self, cmd: Command) -> Result<()> {
        let Command { tag, body } = cmd;
        let body = match body {
            Body::Std(body) => body,
            other => return self.authenticated_command(tag, other).await,
        };
        match body {
            CommandBody::Capability => {
                let caps = self.capabilities();
                self.data(Data::Capability(caps));
                self.ok(&tag, None, "CAPABILITY completed");
            }
            CommandBody::Noop => self.ok(&tag, None, "NOOP completed"),
            CommandBody::Logout => {
                self.bye(None, "Logging out");
                self.ok(&tag, None, "LOGOUT completed");
                self.close = true;
            }
            CommandBody::StartTLS => self.starttls(&tag).await?,
            CommandBody::Login { username, password } => {
                self.login(&tag, username, password).await;
            }
            CommandBody::Authenticate {
                mechanism,
                initial_response,
            } => {
                let ir = initial_response.map(|r| r.declassify().to_vec());
                self.authenticate(&tag, &mechanism, ir).await?;
            }
            CommandBody::Id { .. } => {
                let param = |k: &'static str, v: &'static str| -> Option<(IString, NString)> {
                    Some((IString::try_from(k).ok()?, NString::try_from(v).ok()?))
                };
                let parameters = [
                    param("name", "The Post"),
                    param("version", env!("CARGO_PKG_VERSION")),
                ]
                .into_iter()
                .collect();
                self.data(Data::Id { parameters });
                self.ok(&tag, None, "ID completed");
            }
            other => return self.authenticated_command(tag, Body::Std(other)).await,
        }
        Ok(())
    }

    /// Команды состояний «аутентифицирован» и «папка выбрана».
    async fn authenticated_command(&mut self, tag: Tag<'static>, body: Body) -> Result<()> {
        if self.principal.is_none() {
            self.bad(Some(&tag), "Not authenticated");
            return Ok(());
        }
        let done = match body {
            Body::Std(CommandBody::Enable { capabilities }) => {
                self.enable(&tag, capabilities);
                Ok(())
            }
            Body::List(cmd) => self.list(&tag, cmd).await,
            Body::Status { mailbox, items } => self.status(&tag, &mailbox, &items).await,
            Body::Create { mailbox, uses } => self.create(&tag, &mailbox, &uses).await,
            Body::ESearch {
                charset,
                criteria,
                uid,
                ret,
            } => {
                if self.selected.is_none() {
                    self.bad(Some(&tag), "No mailbox selected");
                    return Ok(());
                }
                let cmd = SearchCmd {
                    charset,
                    criteria,
                    uid,
                    ret: Some(ret),
                };
                self.search(&tag, cmd).await
            }
            Body::Std(body) => match body {
                CommandBody::Select {
                    mailbox,
                    parameters,
                } => {
                    self.select_params(&parameters);
                    self.select(&tag, &proto::mailbox_bytes(&mailbox), false)
                        .await
                }
                CommandBody::Examine {
                    mailbox,
                    parameters,
                } => {
                    self.select_params(&parameters);
                    self.select(&tag, &proto::mailbox_bytes(&mailbox), true)
                        .await
                }
                CommandBody::Delete { mailbox } => {
                    self.delete(&tag, &proto::mailbox_bytes(&mailbox)).await
                }
                CommandBody::Rename { from, to } => {
                    let (from, to) = (proto::mailbox_bytes(&from), proto::mailbox_bytes(&to));
                    self.rename(&tag, &from, &to).await
                }
                CommandBody::Append {
                    mailbox,
                    flags,
                    date,
                    message,
                } => {
                    self.append(&tag, &mailbox, &flags, date.as_ref(), &message)
                        .await
                }
                CommandBody::Subscribe { mailbox } => {
                    self.subscribe(&tag, &proto::mailbox_bytes(&mailbox), true)
                        .await
                }
                CommandBody::Unsubscribe { mailbox } => {
                    self.subscribe(&tag, &proto::mailbox_bytes(&mailbox), false)
                        .await
                }
                body => {
                    self.selected_command(&tag, body).await;
                    Ok(())
                }
            },
        };
        if let Err(e) = done {
            self.store_error(&tag, &e);
        }
        Ok(())
    }

    /// Команды состояния «папка выбрана».
    async fn selected_command(&mut self, tag: &Tag<'static>, body: CommandBody<'static>) {
        if self.selected.is_none() {
            self.bad(Some(tag), "No mailbox selected");
            return;
        }
        let done = match body {
            CommandBody::Check => {
                self.ok(tag, None, "CHECK completed");
                Ok(())
            }
            CommandBody::Unselect => {
                self.unselect(tag);
                Ok(())
            }
            CommandBody::Fetch {
                sequence_set,
                macro_or_item_names,
                uid,
                modifiers,
            } => {
                let changed_since = modifiers.iter().find_map(|m| match m {
                    FetchModifier::ChangedSince(n) => Some(n.get()),
                    FetchModifier::Vanished => None,
                });
                self.fetch(tag, &sequence_set, macro_or_item_names, uid, changed_since)
                    .await
            }
            body @ CommandBody::Store { .. } => self.store(tag, body).await,
            CommandBody::Copy {
                sequence_set,
                mailbox,
                uid,
            } => {
                self.copy_or_move(tag, &sequence_set, &mailbox, uid, false)
                    .await
            }
            CommandBody::Move {
                sequence_set,
                mailbox,
                uid,
            } => {
                self.copy_or_move(tag, &sequence_set, &mailbox, uid, true)
                    .await
            }
            CommandBody::Expunge => self.expunge(tag, None).await,
            CommandBody::ExpungeUid { sequence_set } => {
                self.expunge(tag, Some(&sequence_set)).await
            }
            CommandBody::Close => self.close(tag).await,
            CommandBody::Search {
                charset,
                criteria,
                uid,
            } => {
                let cmd = SearchCmd {
                    charset,
                    criteria,
                    uid,
                    ret: None,
                };
                self.search(tag, cmd).await
            }
            _ => {
                self.bad(Some(tag), "Command not supported");
                Ok(())
            }
        };
        if let Err(e) = done {
            self.store_error(tag, &e);
        }
    }

    async fn starttls(&mut self, tag: &Tag<'static>) -> Result<()> {
        let available =
            self.principal.is_none() && self.conn.as_ref().is_some_and(Transport::can_starttls);
        if !available {
            self.bad(Some(tag), "STARTTLS not available");
            return Ok(());
        }
        self.ok(tag, None, "Begin TLS negotiation now");
        self.flush().await?;
        let Some(conn) = self.conn.take() else {
            return Ok(());
        };
        let conn = conn.starttls().await?;
        self.conn = Some(conn);
        // Всё, что пришло открытым текстом после STARTTLS, отбрасывается.
        self.reader = reader(&self.ctx.config);
        Ok(())
    }

    async fn login(
        &mut self,
        tag: &Tag<'static>,
        username: AString<'static>,
        password: Secret<AString<'static>>,
    ) {
        if self.principal.is_some() {
            self.bad(Some(tag), "Already authenticated");
            return;
        }
        if !self.tls() {
            self.no(
                tag,
                Some(proto::code("PRIVACYREQUIRED")),
                "LOGIN requires TLS, use STARTTLS",
            );
            return;
        }
        let login = String::from_utf8(username.as_ref().to_vec());
        let secret = String::from_utf8(password.declassify().as_ref().to_vec());
        let (Ok(login), Ok(secret)) = (login, secret) else {
            self.bad(Some(tag), "Invalid UTF-8 in credentials");
            return;
        };
        let creds = Credentials::Password { login, secret };
        match self.ctx.deps.auth.verify(creds).await {
            Ok(p) => self.logged_in(tag, p),
            Err(Error::Forbidden) => self.auth_failed(tag, SaslFailure::Authentication),
            Err(e) => self.auth_unavailable(tag, &e),
        }
    }

    async fn authenticate(
        &mut self,
        tag: &Tag<'static>,
        mechanism: &AuthMechanism<'static>,
        mut response: Option<Vec<u8>>,
    ) -> Result<()> {
        if self.principal.is_some() {
            self.bad(Some(tag), "Already authenticated");
            return Ok(());
        }
        let tls = self.tls();
        let offered = self.ctx.deps.sasl.mechanisms(tls);
        let mech = mechanism.as_ref().parse::<Mechanism>().ok();
        let Some(mech) = mech.filter(|m| offered.contains(m)) else {
            let plain_text = mech.is_some_and(Mechanism::plaintext) && !tls;
            let code = if plain_text {
                "PRIVACYREQUIRED"
            } else {
                "CANNOT"
            };
            self.no(
                tag,
                Some(proto::code(code)),
                "Unsupported authentication mechanism",
            );
            return Ok(());
        };
        let ctx = SaslContext {
            service: "imap",
            tls,
        };
        let mut sasl = match self.ctx.deps.sasl.start(mech, &ctx) {
            Ok(s) => s,
            Err(e) => {
                self.auth_unavailable(tag, &e);
                return Ok(());
            }
        };
        loop {
            let step = match sasl.step(response.as_deref()).await {
                Ok(step) => step,
                Err(Error::Invalid(_)) => {
                    self.bad(Some(tag), "Malformed SASL message");
                    return Ok(());
                }
                Err(e) => {
                    self.auth_unavailable(tag, &e);
                    return Ok(());
                }
            };
            match step {
                SaslStep::Challenge(challenge) => {
                    self.push(&Response::CommandContinuationRequest(
                        CommandContinuationRequest::Base64(Cow::Owned(challenge)),
                    ));
                    match self.read_input(Expect::AuthData).await? {
                        None => return Ok(()),
                        Some(Input::AuthData(Some(data))) => response = Some(data),
                        Some(Input::AuthData(None)) => {
                            self.bad(Some(tag), "Authentication cancelled");
                            return Ok(());
                        }
                        Some(_) => {
                            self.bad(Some(tag), "Invalid SASL response");
                            return Ok(());
                        }
                    }
                }
                SaslStep::Done(p) => {
                    self.logged_in(tag, p);
                    return Ok(());
                }
                SaslStep::Fail(f) => {
                    self.auth_failed(tag, f);
                    return Ok(());
                }
            }
        }
    }

    fn logged_in(&mut self, tag: &Tag<'static>, principal: Principal) {
        tracing::debug!(account = %principal.account, "imap: вход выполнен");
        self.principal = Some(principal);
        let caps = self.capabilities();
        self.ok(tag, Some(Code::Capability(caps)), "Logged in");
    }

    fn auth_failed(&mut self, tag: &Tag<'static>, failure: SaslFailure) {
        let code = match failure {
            SaslFailure::Authentication => "AUTHENTICATIONFAILED",
            SaslFailure::Authorization => "AUTHORIZATIONFAILED",
            SaslFailure::Expired => "EXPIRED",
        };
        self.no(tag, Some(proto::code(code)), "Authentication failed");
        self.auth_failures += 1;
        if self.auth_failures >= self.ctx.config.limits.max_auth_failures {
            self.bye(None, "Too many authentication failures");
            self.close = true;
        }
    }

    fn auth_unavailable(&mut self, tag: &Tag<'static>, e: &Error) {
        tracing::warn!(error = %e, "imap: аутентификация недоступна");
        self.no(
            tag,
            Some(proto::code("UNAVAILABLE")),
            "Authentication temporarily unavailable",
        );
    }

    /// Параметры `SELECT`/`EXAMINE`: `CONDSTORE` включает CONDSTORE (RFC 7162, 3.1.8).
    fn select_params(&mut self, parameters: &[SelectParameter]) {
        if parameters
            .iter()
            .any(|p| matches!(p, SelectParameter::CondStore))
        {
            self.condstore = true;
        }
    }

    fn enable(&mut self, tag: &Tag<'static>, capabilities: Vec1<CapabilityEnable<'static>>) {
        if self.selected.is_some() {
            self.bad(Some(tag), "ENABLE is not allowed in selected state");
            return;
        }
        let mut enabled = Vec::new();
        for c in capabilities.into_iter() {
            let name = c.to_string();
            if name.eq_ignore_ascii_case("IMAP4rev2") {
                self.rev2 = true;
                enabled.push(c);
            } else if name.eq_ignore_ascii_case("CONDSTORE") {
                self.condstore = true;
                enabled.push(c);
            }
        }
        self.data(Data::Enabled {
            capabilities: enabled,
        });
        self.ok(tag, None, "ENABLE completed");
    }
}

fn io_error(e: std::io::Error) -> Error {
    tracing::debug!(error = %e, "imap: ошибка ввода-вывода");
    Error::UNAVAILABLE
}

/// Сервер IMAP одного слушателя.
#[derive(Debug, Clone)]
pub struct Server {
    ctx: Arc<SessionCtx>,
}

impl Server {
    /// Сервер с контекстом сессий.
    pub fn new(ctx: SessionCtx) -> Self {
        Self { ctx: Arc::new(ctx) }
    }

    /// Принимает соединения до остановки узла, затем ждёт сессии не дольше `timeouts.drain`
    /// (сессии получают `BYE`) и прерывает оставшиеся.
    pub async fn serve(&self, listener: Listener) -> Result<()> {
        let mut sessions = JoinSet::new();
        let shutdown = self.ctx.shutdown.clone();
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                Some(_) = sessions.join_next(), if !sessions.is_empty() => {}
                accepted = listener.accept() => {
                    let incoming = match accepted {
                        Ok(incoming) => incoming,
                        Err(e) => {
                            tracing::warn!(error = %e, "imap: ошибка приёма соединения");
                            continue;
                        }
                    };
                    let full = sessions.len() >= self.ctx.config.limits.max_connections;
                    let ctx = self.ctx.clone();
                    let task = async move {
                        let peer = incoming.peer();
                        let conn = match incoming.establish().await {
                            Ok(conn) => conn,
                            Err(e) => {
                                tracing::debug!(%peer, error = %e, "imap: соединение не установлено");
                                return;
                            }
                        };
                        if full {
                            reject(conn).await;
                            return;
                        }
                        let client = conn.info.client;
                        if let Err(e) = Session::new(ctx, conn).run().await {
                            tracing::debug!(%client, error = %e, "imap: сессия прервана");
                        }
                    };
                    if full {
                        tokio::spawn(task);
                    } else {
                        sessions.spawn(task);
                    }
                }
            }
        }
        let drain = self.ctx.config.timeouts.drain;
        let finished = tokio::time::timeout(drain, async {
            while sessions.join_next().await.is_some() {}
        })
        .await;
        if finished.is_err() {
            tracing::info!(
                left = sessions.len(),
                "imap: срок остановки истёк, сессии прерваны"
            );
            sessions.abort_all();
        }
        Ok(())
    }
}

/// Соединение сверх лимита: `BYE` и закрытие.
async fn reject(mut conn: Conn) {
    let bye = Response::Status(Status::Bye(crate::proto::types::response::Bye {
        code: Some(proto::code("UNAVAILABLE")),
        text: proto::text("Too many connections"),
    }));
    let _ = conn.stream.write_all(&proto::encode(&bye)).await;
    let _ = conn.stream.shutdown().await;
}
