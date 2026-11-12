//! Сеанс ManageSieve: чтение команд, приветствие, `STARTTLS`, вход SASL (SIEVE-08); команды скриптов — [`super::scripts`].

use std::fmt;
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tp_contracts::Error;
use tp_contracts::meta::AccountScope;
use tp_contracts::sasl::{Mechanism, SaslContext, SaslFailure, SaslStep};
use tp_contracts::tls::Profile;

use super::proto::{Arg, Literal, put_string, split_literal, tokenize};
use super::scripts;
use super::{SessionCtx, Transport};
use crate::compile::EXTENSIONS;

/// Имя службы SASL (RFC 5804 §2.1: GSSAPI `sieve`).
const SERVICE: &str = "sieve";

/// Аргументов в команде вместе с именем (RFC 5804: не больше двух); лишние не хранятся.
const MAX_ARGS: usize = 3;

/// Завершение сеанса.
#[derive(Debug)]
pub(super) enum Stop {
    /// Соединение закрыто или ошибка ввода-вывода.
    Closed,
    /// Сообщить `BYE` и закрыть.
    Bye(&'static str),
}

/// Команда не разобрана; поток при этом прочитан до конца команды.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reject {
    Syntax,
    /// Литерал больше `sieve.managesieve.max_literal`.
    TooBig,
}

/// Сеанс одного соединения.
pub struct Session<T: Transport> {
    pub(super) ctx: Arc<SessionCtx>,
    conn: Option<T>,
    buf: Vec<u8>,
    pub(super) out: Vec<u8>,
    /// Вошедшая учётная запись.
    pub(super) account: Option<AccountScope>,
    auth_failures: u32,
}

impl<T: Transport> fmt::Debug for Session<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("authenticated", &self.account.is_some())
            .finish_non_exhaustive()
    }
}

impl<T: Transport> Session<T> {
    /// Сеанс поверх установленного соединения.
    pub fn new(ctx: Arc<SessionCtx>, conn: T) -> Self {
        Self {
            ctx,
            conn: Some(conn),
            buf: Vec::new(),
            out: Vec::new(),
            account: None,
            auth_failures: 0,
        }
    }

    /// Ведёт сеанс до `LOGOUT`, закрытия соединения, таймаута бездействия или остановки узла.
    pub async fn run(mut self) {
        self.capabilities();
        self.ok("ManageSieve ready");
        let stop = loop {
            match self.step().await {
                Ok(true) => {}
                Ok(false) => break None,
                Err(stop) => break Some(stop),
            }
        };
        if let Some(Stop::Bye(text)) = stop {
            self.out.extend_from_slice(b"BYE ");
            put_string(&mut self.out, text);
            self.out.extend_from_slice(b"\r\n");
        }
        if !matches!(stop, Some(Stop::Closed)) {
            let _ = self.flush().await;
        }
        if let Some(mut conn) = self.conn.take() {
            let _ = conn.io().shutdown().await;
        }
    }

    fn tls(&self) -> bool {
        self.conn.as_ref().is_some_and(Transport::tls)
    }

    /// Вход разрешён: канал защищён или узел в `profile: dev`.
    fn secure(&self) -> bool {
        self.tls() || self.ctx.profile == Profile::Dev
    }

    // ---------- Ответы ----------

    fn respond(&mut self, kind: &str, code: Option<&str>, text: &str) {
        self.out.extend_from_slice(kind.as_bytes());
        if let Some(code) = code {
            self.out.extend_from_slice(format!(" ({code})").as_bytes());
        }
        if !text.is_empty() {
            self.out.push(b' ');
            put_string(&mut self.out, text);
        }
        self.out.extend_from_slice(b"\r\n");
    }

    pub(super) fn ok(&mut self, text: &str) {
        self.respond("OK", None, text);
    }

    pub(super) fn no(&mut self, text: &str) {
        self.respond("NO", None, text);
    }

    /// `NO` с кодом ответа RFC 5804 §1.3.
    pub(super) fn no_code(&mut self, code: &str, text: &str) {
        self.respond("NO", Some(code), text);
    }

    /// Сбой контракта: `NO (TRYLATER)`, подробности — в журнале.
    pub(super) fn unavailable(&mut self, e: &Error) {
        tracing::warn!(error = %e, "managesieve: команда не выполнена");
        self.no_code("TRYLATER", "Temporary failure, try again later");
    }

    /// Возможности (RFC 5804 §1.7): в приветствии, после `STARTTLS` и по `CAPABILITY`.
    fn capabilities(&mut self) {
        let mechanisms = if self.secure() {
            let names: Vec<&str> = self
                .ctx
                .deps
                .sasl
                .mechanisms(true)
                .into_iter()
                .map(Mechanism::name)
                .collect();
            names.join(" ")
        } else {
            String::new()
        };
        let mut cap = |name: &str, value: Option<&str>| {
            put_string(&mut self.out, name);
            if let Some(value) = value {
                self.out.push(b' ');
                put_string(&mut self.out, value);
            }
            self.out.extend_from_slice(b"\r\n");
        };
        cap("IMPLEMENTATION", Some("The Post"));
        cap("SASL", Some(&mechanisms));
        cap("SIEVE", Some(&EXTENSIONS.join(" ")));
        cap(
            "MAXREDIRECTS",
            Some(&self.ctx.config.max_redirects.to_string()),
        );
        cap("UNAUTHENTICATE", None);
        cap("VERSION", Some("1.0"));
        if self.conn.as_ref().is_some_and(Transport::can_starttls) {
            put_string(&mut self.out, "STARTTLS");
            self.out.extend_from_slice(b"\r\n");
        }
    }

    // ---------- Чтение ----------

    async fn flush(&mut self) -> Result<(), Stop> {
        if self.out.is_empty() {
            return Ok(());
        }
        let conn = self.conn.as_mut().ok_or(Stop::Closed)?;
        let io = conn.io();
        let sent = async {
            io.write_all(&self.out).await?;
            io.flush().await
        };
        let done = tokio::time::timeout(self.ctx.config.managesieve.idle_timeout, sent).await;
        self.out.clear();
        match done {
            Ok(Ok(())) => Ok(()),
            _ => Err(Stop::Closed),
        }
    }

    /// Отправляет накопленные ответы и дочитывает данные клиента.
    async fn fill(&mut self) -> Result<(), Stop> {
        self.flush().await?;
        let conn = self.conn.as_mut().ok_or(Stop::Closed)?;
        let idle = self.ctx.config.managesieve.idle_timeout;
        tokio::select! {
            _ = self.ctx.shutdown.cancelled() => Err(Stop::Bye("Server is shutting down")),
            read = tokio::time::timeout(idle, conn.io().read_buf(&mut self.buf)) => match read {
                Err(_) => Err(Stop::Bye("Idle timeout")),
                Ok(Ok(n)) if n > 0 => Ok(()),
                Ok(_) => Err(Stop::Closed),
            },
        }
    }

    /// Строка без CRLF; длиннее `sieve.managesieve.max_line` — `BYE`.
    async fn read_line(&mut self) -> Result<Vec<u8>, Stop> {
        let max = self.ctx.config.managesieve.max_line;
        loop {
            match self.buf.iter().position(|&b| b == b'\n') {
                Some(end) if end <= max => {
                    let mut line: Vec<u8> = self.buf.drain(..=end).collect();
                    line.pop();
                    if line.last() == Some(&b'\r') {
                        line.pop();
                    }
                    return Ok(line);
                }
                None if self.buf.len() <= max => self.fill().await?,
                _ => return Err(Stop::Bye("Line too long")),
            }
        }
    }

    async fn read_exact(&mut self, n: usize) -> Result<Vec<u8>, Stop> {
        while self.buf.len() < n {
            self.fill().await?;
        }
        Ok(self.buf.drain(..n).collect())
    }

    /// Пропускает `n` байт литерала, не накапливая их.
    async fn discard(&mut self, mut n: u64) -> Result<(), Stop> {
        loop {
            let take = usize::try_from(n).map_or(self.buf.len(), |n| n.min(self.buf.len()));
            self.buf.drain(..take);
            n -= take as u64;
            if n == 0 {
                return Ok(());
            }
            self.fill().await?;
        }
    }

    /// Команда целиком: строка с литералами `{n+}` (данные литерала клиент шлёт без подтверждения, поэтому слишком
    /// большой литерал пропускается, а не обрывает разбор).
    async fn read_command(&mut self) -> Result<Result<Vec<Arg>, Reject>, Stop> {
        let max_literal = self.ctx.config.managesieve.max_literal as u64;
        let mut args = Vec::new();
        let mut reject = None;
        loop {
            let line = self.read_line().await?;
            let (head, literal) = split_literal(&line);
            if !tokenize(head, &mut args) {
                reject.get_or_insert(Reject::Syntax);
            }
            let size = match literal {
                Literal::None => break,
                Literal::Bad => return Err(Stop::Bye("Invalid literal")),
                Literal::Size(n) => n,
            };
            if size > max_literal {
                self.discard(size).await?;
                reject = Some(Reject::TooBig);
            } else if args.len() >= MAX_ARGS {
                self.discard(size).await?;
                reject.get_or_insert(Reject::Syntax);
            } else {
                // size ≤ max_literal: помещается в usize.
                let data = self.read_exact(size as usize).await?;
                args.push(Arg::Str(data));
            }
            if args.len() > MAX_ARGS {
                args.truncate(MAX_ARGS);
                reject.get_or_insert(Reject::Syntax);
            }
        }
        Ok(match reject {
            Some(r) => Err(r),
            None => Ok(args),
        })
    }

    fn rejected(&mut self, reject: Reject) {
        match reject {
            Reject::Syntax => self.no("Syntax error"),
            Reject::TooBig => self.no_code("QUOTA/MAXSIZE", "Literal is too large"),
        }
    }

    // ---------- Команды ----------

    /// Одна команда; `false` — сеанс завершён (`LOGOUT`).
    async fn step(&mut self) -> Result<bool, Stop> {
        let args = match self.read_command().await? {
            Ok(args) => args,
            Err(reject) => {
                self.rejected(reject);
                return Ok(true);
            }
        };
        let mut args = args.into_iter();
        let Some(Arg::Atom(name)) = args.next() else {
            self.no("Command expected");
            return Ok(true);
        };
        let name = name.to_ascii_uppercase();
        let args: Vec<Arg> = args.collect();
        match (name.as_str(), args.as_slice()) {
            ("CAPABILITY", []) => {
                self.capabilities();
                self.ok("Capability completed");
            }
            ("NOOP", []) => self.ok("NOOP completed"),
            ("NOOP", [Arg::Str(tag)]) => match std::str::from_utf8(tag) {
                Ok(tag) => {
                    self.out.extend_from_slice(b"OK (TAG ");
                    put_string(&mut self.out, tag);
                    self.out.extend_from_slice(b") \"Done\"\r\n");
                }
                Err(_) => self.no("Invalid UTF-8"),
            },
            ("LOGOUT", []) => {
                self.ok("Logout completed");
                return Ok(false);
            }
            ("STARTTLS", []) => self.starttls().await?,
            ("AUTHENTICATE", [Arg::Str(mech)]) => self.authenticate(mech, None).await?,
            ("AUTHENTICATE", [Arg::Str(mech), Arg::Str(initial)]) => {
                self.authenticate(mech, Some(initial)).await?;
            }
            ("UNAUTHENTICATE", []) => {
                if self.account.take().is_some() {
                    self.ok("Unauthenticated");
                } else {
                    self.no("Not authenticated");
                }
            }
            (
                "CAPABILITY" | "NOOP" | "LOGOUT" | "STARTTLS" | "AUTHENTICATE" | "UNAUTHENTICATE",
                _,
            ) => self.no("Invalid arguments"),
            (name, args) if scripts::is_command(name) => self.script_command(name, args).await,
            _ => self.no("Unknown command"),
        }
        Ok(true)
    }

    async fn starttls(&mut self) -> Result<(), Stop> {
        if self.account.is_some() {
            self.no("Already authenticated");
            return Ok(());
        }
        if !self.conn.as_ref().is_some_and(Transport::can_starttls) {
            self.no("STARTTLS is not available");
            return Ok(());
        }
        self.ok("Begin TLS negotiation now");
        self.flush().await?;
        let conn = self.conn.take().ok_or(Stop::Closed)?;
        match conn.starttls().await {
            Ok(conn) => self.conn = Some(conn),
            Err(e) => {
                tracing::debug!(error = %e, "managesieve: рукопожатие TLS не удалось");
                return Err(Stop::Closed);
            }
        }
        // Данные, пришедшие открытым текстом до рукопожатия, не исполняются.
        self.buf.clear();
        self.capabilities();
        self.ok("TLS negotiation successful");
        Ok(())
    }

    /// `AUTHENTICATE "механизм" ["начальный ответ"]` (RFC 5804 §2.1): ответы и вызовы — строки base64, отмена — `"*"`.
    async fn authenticate(&mut self, mech: &[u8], initial: Option<&[u8]>) -> Result<(), Stop> {
        if self.account.is_some() {
            self.no("Already authenticated");
            return Ok(());
        }
        if !self.secure() {
            self.no_code("ENCRYPT-NEEDED", "TLS is required to authenticate");
            return Ok(());
        }
        // В `profile: dev` вход без TLS разрешён: контракту SASL канал объявляется защищённым.
        let ctx = SaslContext {
            service: SERVICE,
            tls: true,
        };
        let Some(mech) = std::str::from_utf8(mech)
            .ok()
            .and_then(|m| m.parse::<Mechanism>().ok())
            .filter(|m| self.ctx.deps.sasl.mechanisms(true).contains(m))
        else {
            self.no("Unsupported authentication mechanism");
            return Ok(());
        };
        let mut response = match initial.map(|r| STANDARD.decode(r)) {
            None => None,
            Some(Ok(r)) => Some(r),
            Some(Err(_)) => {
                self.no("Invalid base64");
                return Ok(());
            }
        };
        let mut sasl = match self.ctx.deps.sasl.start(mech, &ctx) {
            Ok(sasl) => sasl,
            Err(e) => {
                self.unavailable(&e);
                return Ok(());
            }
        };
        loop {
            let failure = match sasl.step(response.as_deref()).await {
                Ok(SaslStep::Done(principal)) => {
                    self.account = Some(AccountScope {
                        tenant: principal.tenant,
                        account: principal.account,
                    });
                    self.ok("Authenticated");
                    return Ok(());
                }
                Ok(SaslStep::Challenge(challenge)) => {
                    put_string(&mut self.out, &STANDARD.encode(challenge));
                    self.out.extend_from_slice(b"\r\n");
                    let answer = match self.read_command().await? {
                        Ok(args) => args,
                        Err(reject) => {
                            self.rejected(reject);
                            return Ok(());
                        }
                    };
                    let [Arg::Str(answer)] = answer.as_slice() else {
                        self.no("SASL response expected");
                        return Ok(());
                    };
                    if answer == b"*" {
                        self.no("Authentication cancelled");
                        return Ok(());
                    }
                    match STANDARD.decode(answer) {
                        Ok(r) => response = Some(r),
                        Err(_) => {
                            self.no("Invalid base64");
                            return Ok(());
                        }
                    }
                    continue;
                }
                Ok(SaslStep::Fail(failure)) => failure,
                Err(Error::Invalid(_)) => SaslFailure::Authentication,
                Err(e) => {
                    self.unavailable(&e);
                    return Ok(());
                }
            };
            self.auth_failures += 1;
            self.no(match failure {
                SaslFailure::Authentication => "Authentication failed",
                SaslFailure::Authorization => "Authorization failed",
                SaslFailure::Expired => "Credentials expired",
            });
            if self.auth_failures >= self.ctx.config.managesieve.max_auth_failures {
                return Err(Stop::Bye("Too many authentication failures"));
            }
            return Ok(());
        }
    }
}
