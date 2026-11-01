//! Конечный автомат сессии SMTP (RFC 5321) поверх `smtp-proto`: разбор команд — `RequestReceiver`,
//! данных — `DataReceiver` (снятие точек, защита от SMTP smuggling) и `BdatReceiver` (RFC 3030),
//! ответ `EHLO` — `EhloResponse`.
//!
//! Ответы на конвейер команд (PIPELINING, RFC 2920) копятся и отправляются одной записью, когда входной
//! буфер исчерпан. После `STARTTLS` непрочитанный остаток буфера отбрасывается: команды, пришедшие открытым
//! текстом до рукопожатия, не исполняются после него (RFC 3207, 4.2).

use std::io;
use std::time::Instant;

use bytes::Bytes;
use smtp_proto::request::receiver::{
    BdatReceiver, DataReceiver, DummyDataReceiver, RequestReceiver,
};
use smtp_proto::{
    EXT_8BIT_MIME, EXT_CHUNKING, EXT_ENHANCED_STATUS_CODES, EXT_PIPELINING, EXT_SIZE,
    EXT_SMTP_UTF8, EXT_START_TLS, EhloResponse, MAIL_BODY_BINARYMIME, MAIL_SMTPUTF8, MailFrom,
    Request,
};
use time::OffsetDateTime;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tp_contracts::auth::Principal;
use tp_platform::ConnInfo;

use super::{Envelope, Mode, Reply, SessionCtx, is_positive, next_id, reply};
use crate::limits::SendLimits;

/// Чем закончилась работа сессии над потоком.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// Клиент завершил сессию (`QUIT`).
    Quit,
    /// Клиенту ответили `220` на `STARTTLS`: вызывающий выполняет рукопожатие и продолжает
    /// [`Session::serve`] над защищённым потоком без приветствия.
    StartTls,
    /// Соединение закрыто клиентом, по таймауту или лимиту ошибок.
    Closed,
}

/// Что разбирает сессия.
pub(super) enum State {
    /// Команды.
    Command(RequestReceiver),
    /// Данные `DATA` до `<CRLF>.<CRLF>`.
    Data(DataReceiver),
    /// Блок `BDAT`.
    Bdat(BdatReceiver),
    /// Пропуск данных, которые не будут приняты (превышен размер, нет транзакции); затем — ответ.
    Discard(DummyDataReceiver, Reply),
    /// Ответ клиента в обмене SASL (`AUTH`).
    Auth(super::auth::Exchange),
}

impl State {
    pub(super) fn command() -> Self {
        State::Command(RequestReceiver::default())
    }
}

/// Транзакция: от `MAIL` до конца данных или `RSET`.
pub(super) struct Tx {
    pub(super) mail_from: String,
    pub(super) rcpts: Vec<String>,
    pub(super) smtputf8: bool,
    pub(super) started_at: OffsetDateTime,
    /// Данные письма (`DATA` или сумма блоков `BDAT`).
    pub(super) body: Vec<u8>,
    /// Транзакция получает данные блоками `BDAT`: `DATA` в ней запрещён (RFC 3030, 2).
    pub(super) bdat: bool,
}

/// Сессия SMTP одного соединения.
pub struct Session {
    pub(super) ctx: SessionCtx,
    pub(super) conn: ConnInfo,
    /// Слушатель может выполнить STARTTLS (есть сертификаты, канал ещё открыт).
    pub(super) starttls: bool,
    pub(super) helo: Option<String>,
    pub(super) esmtp: bool,
    pub(super) principal: Option<Principal>,
    pub(super) tx: Option<Tx>,
    pub(super) errors: u32,
    pub(super) auth_failures: u32,
    /// Сессия завершается после отправки накопленных ответов.
    pub(super) exit: Option<Exit>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("mode", &self.ctx.mode)
            .field("conn", &self.conn)
            .field("authenticated", &self.principal.is_some())
            .finish_non_exhaustive()
    }
}

impl Session {
    /// Сессия соединения `conn`; `starttls` — слушатель может предложить STARTTLS.
    pub fn new(ctx: SessionCtx, conn: ConnInfo, starttls: bool) -> Self {
        Self {
            ctx,
            conn,
            starttls: starttls && !conn.tls,
            helo: None,
            esmtp: false,
            principal: None,
            tx: None,
            errors: 0,
            auth_failures: 0,
            exit: None,
        }
    }

    /// Аутентифицированный отправитель.
    pub fn principal(&self) -> Option<&Principal> {
        self.principal.as_ref()
    }

    /// Канал защищён: сессия забывает всё, что узнала открытым текстом (RFC 3207, 4.2), и ждёт нового `EHLO`.
    pub fn tls_established(&mut self) {
        self.conn.tls = true;
        self.starttls = false;
        self.helo = None;
        self.esmtp = false;
        self.principal = None;
        self.tx = None;
        self.exit = None;
    }

    /// Ведёт сессию над `io` до `QUIT`, `STARTTLS` или закрытия; `greet` — отправить приветствие `220`.
    pub async fn serve<S>(&mut self, io: &mut S, greet: bool) -> io::Result<Exit>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send,
    {
        let mut out = Vec::new();
        if greet {
            out.extend_from_slice(
                format!("220 {} ESMTP The Post\r\n", self.ctx.config.hostname).as_bytes(),
            );
        }
        let mut buf = vec![0u8; 16 * 1024];
        let mut state = State::command();
        loop {
            if !out.is_empty() {
                io.write_all(&out).await?;
                io.flush().await?;
                out.clear();
            }
            if let Some(exit) = self.exit {
                return Ok(exit);
            }
            let wait = match state {
                State::Command(_) | State::Auth(_) => self.ctx.config.timeouts.command,
                _ => self.ctx.config.timeouts.data,
            };
            let n = match tokio::time::timeout(wait, io.read(&mut buf)).await {
                Ok(n) => n?,
                Err(_) => {
                    push(
                        &mut out,
                        &reply(421, [4, 4, 2], "Timeout, closing connection"),
                    );
                    io.write_all(&out).await?;
                    return Ok(Exit::Closed);
                }
            };
            if n == 0 {
                return Ok(Exit::Closed);
            }
            let mut bytes = buf[..n].iter();
            while self.exit.is_none() && !bytes.as_slice().is_empty() {
                state = match state {
                    State::Command(mut rx) => {
                        let before = bytes.len();
                        match rx.ingest(&mut bytes) {
                            Ok(req) => {
                                let req = req.into_owned();
                                self.command(req, &mut out)
                                    .await
                                    .unwrap_or(State::Command(rx))
                            }
                            Err(smtp_proto::Error::NeedsMoreData { .. }) => State::Command(rx),
                            Err(e) => {
                                self.protocol_error(&e, &mut out);
                                if bytes.len() == before {
                                    // Разборщик не продвинулся: отбросить остаток, чтобы не зациклиться.
                                    bytes = [].iter();
                                }
                                State::Command(rx)
                            }
                        }
                    }
                    State::Data(mut rx) => self
                        .data_bytes(&mut rx, &mut bytes, &mut out)
                        .await
                        .unwrap_or(State::Data(rx)),
                    State::Bdat(mut rx) => self
                        .bdat_bytes(&mut rx, &mut bytes, &mut out)
                        .await
                        .unwrap_or(State::Bdat(rx)),
                    State::Auth(ex) => self.auth_bytes(ex, &mut bytes, &mut out).await,
                    State::Discard(mut rx, r) => {
                        if rx.ingest(&mut bytes) {
                            push(&mut out, &r);
                            State::command()
                        } else {
                            State::Discard(rx, r)
                        }
                    }
                };
            }
        }
    }

    /// Ошибка разбора команды.
    fn protocol_error(&mut self, e: &smtp_proto::Error, out: &mut Vec<u8>) {
        use smtp_proto::Error as E;
        let r = match e {
            E::UnknownCommand => reply(500, [5, 5, 1], "Unrecognized command"),
            E::InvalidSenderAddress => reply(501, [5, 1, 7], "Invalid sender address"),
            E::InvalidRecipientAddress => reply(501, [5, 1, 3], "Invalid recipient address"),
            E::SyntaxError { syntax } => reply(501, [5, 5, 4], format!("Syntax: {syntax}")),
            E::InvalidParameter { param } => {
                reply(501, [5, 5, 4], format!("Invalid parameter {param}"))
            }
            E::UnsupportedParameter { param } => {
                reply(555, [5, 5, 4], format!("Unsupported parameter {param}"))
            }
            E::ResponseTooLong => reply(500, [5, 5, 6], "Line too long"),
            E::NeedsMoreData { .. } | E::InvalidResponse { .. } => {
                reply(500, [5, 5, 2], "Syntax error")
            }
        };
        self.fail(r, out);
    }

    /// Отказ на ошибку клиента; сверх `limits.max_errors` — закрытие соединения.
    pub(super) fn fail(&mut self, r: Reply, out: &mut Vec<u8>) {
        self.errors += 1;
        push(out, &r);
        if self.errors >= self.ctx.config.limits.max_errors {
            push(
                out,
                &reply(421, [4, 7, 0], "Too many errors, closing connection"),
            );
            self.exit = Some(Exit::Closed);
        }
    }

    /// Исполняет команду; `Some` — новое состояние разбора.
    async fn command(&mut self, req: Request<String>, out: &mut Vec<u8>) -> Option<State> {
        match req {
            Request::Ehlo { host } => self.ehlo(host, true, out),
            Request::Helo { host } => self.ehlo(host, false, out),
            Request::Mail { from } => self.mail(from, out),
            Request::Rcpt { to } => self.rcpt(to.address, out).await,
            Request::Data => return self.data(out),
            Request::Bdat {
                chunk_size,
                is_last,
            } => return self.bdat(chunk_size, is_last, out).await,
            Request::Rset => {
                self.tx = None;
                push(out, &reply(250, [2, 0, 0], "OK"));
            }
            Request::Noop { .. } => push(out, &reply(250, [2, 0, 0], "OK")),
            Request::Quit => {
                push(out, &reply(221, [2, 0, 0], "Bye"));
                self.exit = Some(Exit::Quit);
            }
            Request::Vrfy { .. } => push(
                out,
                &reply(252, [2, 5, 0], "Cannot VRFY user, but will accept message"),
            ),
            Request::Help { .. } => push(out, &reply(214, [2, 0, 0], "See RFC 5321")),
            Request::StartTls => self.starttls(out),
            Request::Auth {
                mechanism,
                initial_response,
            } => return self.auth(mechanism, initial_response, out).await,
            Request::Expn { .. }
            | Request::Lhlo { .. }
            | Request::Etrn { .. }
            | Request::Atrn { .. }
            | Request::Burl { .. } => {
                self.fail(reply(502, [5, 5, 1], "Command not implemented"), out)
            }
        }
        None
    }

    fn ehlo(&mut self, host: String, esmtp: bool, out: &mut Vec<u8>) {
        self.helo = Some(host);
        self.esmtp = esmtp;
        self.tx = None;
        let cfg = &self.ctx.config;
        if !esmtp {
            push(out, &reply(250, [2, 0, 0], cfg.hostname.clone()));
            return;
        }
        let mut ehlo = EhloResponse::new(cfg.hostname.as_str());
        ehlo.capabilities = EXT_PIPELINING
            | EXT_SIZE
            | EXT_8BIT_MIME
            | EXT_SMTP_UTF8
            | EXT_ENHANCED_STATUS_CODES
            | EXT_CHUNKING;
        ehlo.size = cfg.limits.max_message_size;
        if self.starttls {
            ehlo.capabilities |= EXT_START_TLS;
        }
        self.advertise_auth(&mut ehlo);
        // Запись в Vec не завершается ошибкой.
        let _ = ehlo.write(&mut *out);
    }

    fn mail(&mut self, from: MailFrom<String>, out: &mut Vec<u8>) {
        if self.helo.is_none() {
            return self.fail(reply(503, [5, 5, 1], "Send EHLO first"), out);
        }
        if self.tx.is_some() {
            return self.fail(reply(503, [5, 5, 1], "Nested MAIL command"), out);
        }
        if self.ctx.mode == Mode::Submission && self.principal.is_none() {
            return self.fail(reply(530, [5, 7, 0], "Authentication required"), out);
        }
        if from.flags & MAIL_BODY_BINARYMIME != 0 {
            return self.fail(reply(555, [5, 5, 4], "BODY=BINARYMIME not supported"), out);
        }
        let smtputf8 = from.flags & MAIL_SMTPUTF8 != 0;
        if smtputf8 && !self.esmtp {
            return self.fail(reply(555, [5, 5, 4], "SMTPUTF8 requires EHLO"), out);
        }
        if !smtputf8 && !from.address.is_ascii() {
            return self.fail(
                reply(553, [5, 6, 7], "Non-ASCII address requires SMTPUTF8"),
                out,
            );
        }
        if from.size > self.ctx.config.limits.max_message_size {
            return self.fail(
                reply(552, [5, 3, 4], "Message size exceeds fixed limit"),
                out,
            );
        }
        if let Some(r) = self.mail_rate_limit() {
            push(out, &r);
            return;
        }
        if let Some((l, account)) = self.send_account()
            && !l.allow_message_at(&self.ctx.config.limits, account, Instant::now())
        {
            self.limited("messages");
            push(
                out,
                &reply(
                    451,
                    [4, 7, 1],
                    "Hourly message limit exceeded, try again later",
                ),
            );
            return;
        }
        self.tx = Some(Tx {
            mail_from: from.address,
            rcpts: Vec::new(),
            smtputf8,
            started_at: OffsetDateTime::now_utc(),
            body: Vec::new(),
            bdat: false,
        });
        push(out, &reply(250, [2, 1, 0], "OK"));
    }

    async fn rcpt(&mut self, address: String, out: &mut Vec<u8>) {
        let Some(tx) = &self.tx else {
            return self.fail(reply(503, [5, 5, 1], "Need MAIL command"), out);
        };
        if !tx.smtputf8 && !address.is_ascii() {
            return self.fail(
                reply(553, [5, 6, 7], "Non-ASCII address requires SMTPUTF8"),
                out,
            );
        }
        if tx.rcpts.iter().any(|r| r.eq_ignore_ascii_case(&address)) {
            return push(out, &reply(250, [2, 1, 5], "OK"));
        }
        if tx.rcpts.len() >= self.ctx.config.limits.max_recipients {
            return push(out, &reply(452, [4, 5, 3], "Too many recipients"));
        }
        if let Some((l, account)) = self.send_account()
            && !l.allow_rcpts_at(
                &self.ctx.config.limits,
                account,
                tx.rcpts.len() + 1,
                Instant::now(),
            )
        {
            self.limited("recipients");
            return push(
                out,
                &reply(
                    452,
                    [4, 5, 3],
                    "Hourly recipient limit exceeded, try again later",
                ),
            );
        }
        let env = self.envelope(tx);
        let r = self.ctx.handler.rcpt(&env, &address).await;
        if is_positive(&r) {
            if let Some(tx) = &mut self.tx {
                tx.rcpts.push(address);
            }
            push(out, &r);
        } else {
            self.fail(r, out);
        }
    }

    fn data(&mut self, out: &mut Vec<u8>) -> Option<State> {
        let r = match &self.tx {
            None => reply(503, [5, 5, 1], "Need MAIL command"),
            Some(tx) if tx.bdat => reply(503, [5, 5, 1], "DATA not allowed after BDAT"),
            Some(tx) if tx.rcpts.is_empty() => reply(554, [5, 5, 1], "No valid recipients"),
            Some(_) => {
                push_line(out, 354, "Start mail input; end with <CRLF>.<CRLF>");
                return Some(State::Data(DataReceiver::new()));
            }
        };
        self.fail(r, out);
        None
    }

    async fn data_bytes(
        &mut self,
        rx: &mut DataReceiver,
        bytes: &mut std::slice::Iter<'_, u8>,
        out: &mut Vec<u8>,
    ) -> Option<State> {
        let max = self.ctx.config.limits.max_message_size;
        let tx = self.tx.as_mut()?;
        if rx.ingest(bytes, &mut tx.body) {
            if tx.body.len() > max {
                self.tx = None;
                push(
                    out,
                    &reply(552, [5, 3, 4], "Message size exceeds fixed limit"),
                );
            } else {
                let r = self.finish().await;
                push(out, &r);
            }
            return Some(State::command());
        }
        if tx.body.len() > max {
            // Остаток письма читается и отбрасывается, ответ — после `<CRLF>.<CRLF>`.
            self.tx = None;
            let r = reply(552, [5, 3, 4], "Message size exceeds fixed limit");
            return Some(State::Discard(DummyDataReceiver::new_data(rx), r));
        }
        None
    }

    async fn bdat(&mut self, size: usize, last: bool, out: &mut Vec<u8>) -> Option<State> {
        let max = self.ctx.config.limits.max_message_size;
        let refuse = match &mut self.tx {
            None => Some(reply(503, [5, 5, 1], "Need MAIL command")),
            Some(tx) if tx.rcpts.is_empty() => Some(reply(554, [5, 5, 1], "No valid recipients")),
            Some(tx) if tx.body.len().saturating_add(size) > max => {
                Some(reply(552, [5, 3, 4], "Message size exceeds fixed limit"))
            }
            Some(tx) => {
                tx.bdat = true;
                tx.body.reserve(size);
                None
            }
        };
        if let Some(r) = refuse {
            // Блок всё равно передаётся клиентом: прочитать и отбросить (RFC 3030, 2).
            self.tx = None;
            self.errors += 1;
            if size == 0 {
                push(out, &r);
                return None;
            }
            return Some(State::Discard(DummyDataReceiver::new_bdat(size), r));
        }
        if size == 0 {
            self.bdat_done(last, out).await;
            return None;
        }
        Some(State::Bdat(BdatReceiver::new(size, last)))
    }

    async fn bdat_bytes(
        &mut self,
        rx: &mut BdatReceiver,
        bytes: &mut std::slice::Iter<'_, u8>,
        out: &mut Vec<u8>,
    ) -> Option<State> {
        let tx = self.tx.as_mut()?;
        if rx.ingest(bytes, &mut tx.body) {
            self.bdat_done(rx.is_last, out).await;
            return Some(State::command());
        }
        None
    }

    async fn bdat_done(&mut self, last: bool, out: &mut Vec<u8>) {
        if last {
            let r = self.finish().await;
            push(out, &r);
        } else {
            let size = self.tx.as_ref().map_or(0, |t| t.body.len());
            push(
                out,
                &reply(250, [2, 0, 0], format!("{size} octets received")),
            );
        }
    }

    /// Письмо получено: передать обработчику и закрыть транзакцию.
    async fn finish(&mut self) -> Reply {
        let Some(tx) = self.tx.take() else {
            return reply(503, [5, 5, 1], "Need MAIL command");
        };
        let env = self.envelope(&tx);
        let r = self.ctx.handler.data(&env, Bytes::from(tx.body)).await;
        let accepted = is_positive(&r);
        if let Some(m) = &self.ctx.metrics {
            m.message(env.mode, accepted);
        }
        if accepted && let Some((l, account)) = self.send_account() {
            l.record_at(
                &self.ctx.config.limits,
                account,
                env.rcpts.len(),
                Instant::now(),
            );
        }
        tracing::info!(
            id = %env.id,
            role = env.mode.name(),
            client = %env.conn.client,
            rcpts = env.rcpts.len(),
            code = r.code,
            "smtp: письмо"
        );
        r
    }

    /// Лимиты отправки и учётная запись (submission после AUTH).
    fn send_account(&self) -> Option<(&SendLimits, &str)> {
        let limits = self.ctx.send_limits.as_deref()?;
        let p = self.principal.as_ref()?;
        Some((limits, p.login.as_str()))
    }

    fn limited(&self, kind: &'static str) {
        if let Some(m) = &self.ctx.metrics {
            m.limited(kind);
        }
    }

    fn starttls(&mut self, out: &mut Vec<u8>) {
        if self.conn.tls {
            return self.fail(reply(503, [5, 5, 1], "Already in TLS"), out);
        }
        if !self.starttls {
            return self.fail(reply(454, [4, 7, 0], "TLS not available"), out);
        }
        push(out, &reply(220, [2, 0, 0], "Ready to start TLS"));
        self.exit = Some(Exit::StartTls);
    }

    pub(super) fn envelope(&self, tx: &Tx) -> Envelope {
        Envelope {
            id: next_id(),
            mode: self.ctx.mode,
            conn: self.conn,
            helo: self.helo.clone().unwrap_or_default(),
            esmtp: self.esmtp,
            principal: self.principal.clone(),
            mail_from: tx.mail_from.clone(),
            rcpts: tx.rcpts.clone(),
            smtputf8: tx.smtputf8,
            started_at: tx.started_at,
        }
    }
}

/// Записывает ответ в буфер отправки.
pub(super) fn push(out: &mut Vec<u8>, r: &Reply) {
    // Запись в Vec не завершается ошибкой.
    let _ = r.write(&mut *out);
}

/// Промежуточный ответ без расширенного кода (`354`, `334`: RFC 2034 их не применяет).
pub(super) fn push_line(out: &mut Vec<u8>, code: u16, text: &str) {
    out.extend_from_slice(format!("{code} {text}\r\n").as_bytes());
}
