//! POP3 (RFC 1939) для устаревших устройств: `STLS` (RFC 2595), `AUTH` (SASL, RFC 5034), `CAPA`, `TOP`, `UIDL`
//! (RFC 2449). Выключен по умолчанию ([`Pop3Config::enabled`](crate::config::Pop3Config)).
//!
//! - Почтовый ящик POP3 — «Входящие» учётной записи; список писем фиксируется при входе.
//! - `USER`/`PASS` и механизмы SASL с открытым паролем — только по TLS (как `LOGINDISABLED` в IMAP).
//! - `DELE` помечает письмо; при `QUIT` помеченные переносятся в корзину (`MailStore::delete_to_trash`,
//!   SRS 4.7), как `EXPUNGE` в IMAP. Обрыв соединения без `QUIT` ничего не удаляет (RFC 1939, 6).
//! - `UIDL` — идентификатор письма (`MessageId`): не меняется между сессиями.
//! - Эксклюзивная блокировка ящика (RFC 1939, 8) не берётся: параллельные сессии IMAP и JMAP допустимы,
//!   письмо, удалённое другой сессией, — `-ERR` на `RETR`.

use std::fmt;
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use futures::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinSet;
use tp_contracts::auth::{Credentials, Principal};
use tp_contracts::meta::{AccountScope, MessageQuery, SpecialUse};
use tp_contracts::sasl::{Mechanism, SaslContext, SaslStep};
use tp_contracts::{Error, MessageId, Result};
use tp_platform::{Conn, Listener};

use crate::session::{SessionCtx, Transport, io_error};

/// Письмо ящика POP3.
#[derive(Debug, Clone)]
struct Item {
    id: MessageId,
    size: u64,
    deleted: bool,
}

/// Ящик POP3 после входа.
#[derive(Debug)]
struct Maildrop {
    scope: AccountScope,
    items: Vec<Item>,
}

impl Maildrop {
    /// Письмо по номеру (с 1), не помеченное к удалению.
    fn get(&self, n: &str) -> Option<(usize, &Item)> {
        let n: usize = n.parse().ok()?;
        let i = n.checked_sub(1)?;
        self.items.get(i).filter(|m| !m.deleted).map(|m| (n, m))
    }

    fn live(&self) -> impl Iterator<Item = (usize, &Item)> {
        self.items
            .iter()
            .enumerate()
            .filter(|(_, m)| !m.deleted)
            .map(|(i, m)| (i + 1, m))
    }
}

/// Добавляет письмо в ответ многострочной команды: точка в начале строки удваивается, в конце — CRLF.
#[derive(Debug)]
struct DotStuff {
    line_start: bool,
    last: [u8; 2],
}

impl DotStuff {
    fn new() -> Self {
        Self {
            line_start: true,
            last: *b"\r\n",
        }
    }

    fn push(&mut self, data: &[u8], out: &mut Vec<u8>) {
        for &b in data {
            if self.line_start && b == b'.' {
                out.push(b'.');
            }
            out.push(b);
            self.line_start = b == b'\n';
            self.last = [self.last[1], b];
        }
    }

    fn finish(self, out: &mut Vec<u8>) {
        if self.last != *b"\r\n" {
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(b".\r\n");
    }
}

/// Сессия POP3 одного соединения.
pub struct Pop3Session<T: Transport> {
    ctx: Arc<SessionCtx>,
    conn: Option<T>,
    buf: Vec<u8>,
    out: Vec<u8>,
    user: Option<String>,
    drop: Option<Maildrop>,
    errors: u32,
    auth_failures: u32,
    close: bool,
}

impl<T: Transport> fmt::Debug for Pop3Session<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pop3Session")
            .field("authenticated", &self.drop.is_some())
            .finish_non_exhaustive()
    }
}

/// Следующая строка: `Ok(None)` — нужны ещё байты; `Err(())` — строка длиннее лимита.
fn split_line(buf: &mut Vec<u8>, max: usize) -> std::result::Result<Option<Vec<u8>>, ()> {
    match buf.iter().position(|&b| b == b'\n') {
        Some(end) => {
            let mut line: Vec<u8> = buf.drain(..=end).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.len() > max {
                return Err(());
            }
            Ok(Some(line))
        }
        None if buf.len() > max => Err(()),
        None => Ok(None),
    }
}

impl<T: Transport> Pop3Session<T> {
    /// Сессия соединения.
    pub fn new(ctx: Arc<SessionCtx>, conn: T) -> Self {
        Self {
            ctx,
            conn: Some(conn),
            buf: Vec::new(),
            out: Vec::new(),
            user: None,
            drop: None,
            errors: 0,
            auth_failures: 0,
            close: false,
        }
    }

    /// Ведёт сессию до `QUIT`, закрытия клиентом, таймаута или остановки узла.
    pub async fn run(mut self) -> Result<()> {
        self.reply(true, "The Post POP3 ready");
        while !self.close {
            let Some(line) = self.read_line().await? else {
                break;
            };
            self.command(&line).await?;
        }
        self.flush().await
    }

    fn tls(&self) -> bool {
        self.conn.as_ref().is_some_and(Transport::tls)
    }

    fn reply(&mut self, ok: bool, text: &str) {
        let status = if ok { "+OK" } else { "-ERR" };
        let text: String = text.chars().filter(|c| !c.is_control()).collect();
        self.out
            .extend_from_slice(format!("{status} {text}\r\n").as_bytes());
    }

    fn error(&mut self, text: &str) {
        self.reply(false, text);
        self.errors += 1;
        if self.errors >= self.ctx.config.pop3.max_errors {
            self.close = true;
        }
    }

    async fn flush(&mut self) -> Result<()> {
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

    async fn maybe_flush(&mut self) -> Result<()> {
        if self.out.len() >= self.ctx.config.limits.write_buffer {
            self.flush().await?;
        }
        Ok(())
    }

    /// Следующая строка клиента; `None` — сессия закончена.
    async fn read_line(&mut self) -> Result<Option<Vec<u8>>> {
        let cfg = &self.ctx.config.pop3;
        let max = cfg.max_line;
        let timeout = if self.drop.is_some() {
            cfg.autologout
        } else {
            cfg.login_timeout
        };
        let mut chunk = vec![0u8; 4096];
        loop {
            match split_line(&mut self.buf, max) {
                Ok(Some(line)) => return Ok(Some(line)),
                Ok(None) => {}
                Err(()) => {
                    self.reply(false, "Line too long");
                    return self.end().await;
                }
            }
            self.flush().await?;
            let shutdown = self.ctx.shutdown.clone();
            let Some(conn) = self.conn.as_mut() else {
                return Ok(None);
            };
            let io = conn.io();
            let read = tokio::select! {
                biased;
                _ = shutdown.cancelled() => None,
                r = tokio::time::timeout(timeout, io.read(&mut chunk)) => Some(r),
            };
            match read {
                None => {
                    self.reply(false, "[SYS/TEMP] Server shutting down");
                    return self.end().await;
                }
                Some(Err(_)) => {
                    self.reply(false, "Autologout: idle for too long");
                    return self.end().await;
                }
                Some(Ok(Ok(0))) => return Ok(None),
                Some(Ok(Ok(n))) => self.buf.extend_from_slice(&chunk[..n]),
                Some(Ok(Err(e))) => return Err(io_error(e)),
            }
        }
    }

    async fn end(&mut self) -> Result<Option<Vec<u8>>> {
        self.close = true;
        self.flush().await?;
        Ok(None)
    }

    async fn command(&mut self, line: &[u8]) -> Result<()> {
        let line = String::from_utf8_lossy(line).into_owned();
        let (verb, rest) = line.split_once(' ').unwrap_or((line.as_str(), ""));
        let verb = verb.to_ascii_uppercase();
        let args: Vec<&str> = rest.split(' ').filter(|a| !a.is_empty()).collect();
        match (verb.as_str(), self.drop.is_some()) {
            ("CAPA", _) => self.capa(),
            ("QUIT", _) => self.quit().await?,
            ("NOOP", true) => self.reply(true, ""),
            ("STLS", false) => self.stls().await?,
            ("USER", false) => self.user_cmd(&args),
            ("PASS", false) => self.pass(rest).await?,
            ("AUTH", false) => self.auth(&args).await?,
            ("STAT", true) => self.stat(),
            ("LIST", true) => self.list(&args, false),
            ("UIDL", true) => self.list(&args, true),
            ("RETR", true) => self.retr(&args, None).await?,
            ("TOP", true) => match args.get(1).and_then(|k| k.parse::<usize>().ok()) {
                Some(k) => self.retr(&args, Some(k)).await?,
                None => self.error("Usage: TOP msg n"),
            },
            ("DELE", true) => self.dele(&args),
            ("RSET", true) => self.rset(),
            _ => self.error("Unknown command or wrong state"),
        }
        Ok(())
    }

    // ---------- Состояние AUTHORIZATION ----------

    fn capa(&mut self) {
        let mut lines = vec!["TOP", "UIDL", "RESP-CODES", "AUTH-RESP-CODE", "PIPELINING"]
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if self.drop.is_none() {
            let tls = self.tls();
            if tls {
                lines.push("USER".into());
            } else if self.conn.as_ref().is_some_and(Transport::can_starttls) {
                lines.push("STLS".into());
            }
            let mechs: Vec<String> = self
                .ctx
                .deps
                .sasl
                .mechanisms(tls)
                .iter()
                .map(|m| m.name().to_owned())
                .collect();
            if !mechs.is_empty() {
                lines.push(format!("SASL {}", mechs.join(" ")));
            }
        }
        lines.push("IMPLEMENTATION The-Post".into());
        self.reply(true, "Capability list follows");
        for l in lines {
            self.out.extend_from_slice(l.as_bytes());
            self.out.extend_from_slice(b"\r\n");
        }
        self.out.extend_from_slice(b".\r\n");
    }

    async fn stls(&mut self) -> Result<()> {
        if self.tls() || !self.conn.as_ref().is_some_and(Transport::can_starttls) {
            self.error("STLS not available");
            return Ok(());
        }
        self.reply(true, "Begin TLS negotiation");
        self.flush().await?;
        let Some(conn) = self.conn.take() else {
            return Ok(());
        };
        self.conn = Some(conn.starttls().await?);
        // Пришедшее открытым текстом после STLS отбрасывается.
        self.buf.clear();
        self.user = None;
        Ok(())
    }

    fn user_cmd(&mut self, args: &[&str]) {
        if !self.tls() {
            self.error("[AUTH] USER requires TLS, use STLS");
            return;
        }
        match args.first() {
            Some(u) => {
                self.user = Some((*u).to_owned());
                self.reply(true, "Send password");
            }
            None => self.error("Usage: USER name"),
        }
    }

    async fn pass(&mut self, secret: &str) -> Result<()> {
        let Some(login) = self.user.take().filter(|_| self.tls()) else {
            self.error("USER first");
            return Ok(());
        };
        let creds = Credentials::Password {
            login,
            secret: secret.to_owned(),
        };
        match self.ctx.deps.auth.verify(creds).await {
            Ok(p) => self.logged_in(p).await,
            Err(Error::Forbidden) => {
                self.auth_failed();
                Ok(())
            }
            Err(e) => {
                self.unavailable(&e);
                Ok(())
            }
        }
    }

    /// `AUTH [mechanism [initial-response]]` (RFC 5034); без механизма — список механизмов.
    async fn auth(&mut self, args: &[&str]) -> Result<()> {
        let tls = self.tls();
        let offered = self.ctx.deps.sasl.mechanisms(tls);
        let Some(name) = args.first() else {
            self.reply(true, "Supported mechanisms");
            for m in &offered {
                self.out.extend_from_slice(m.name().as_bytes());
                self.out.extend_from_slice(b"\r\n");
            }
            self.out.extend_from_slice(b".\r\n");
            return Ok(());
        };
        let Some(mech) = name
            .parse::<Mechanism>()
            .ok()
            .filter(|m| offered.contains(m))
        else {
            self.error("Unsupported authentication mechanism");
            return Ok(());
        };
        let mut response = match args.get(1) {
            None => None,
            Some(&"=") => Some(Vec::new()),
            Some(ir) => match STANDARD.decode(ir) {
                Ok(v) => Some(v),
                Err(_) => {
                    self.error("Invalid base64");
                    return Ok(());
                }
            },
        };
        let ctx = SaslContext {
            service: "pop",
            tls,
        };
        let mut sasl = match self.ctx.deps.sasl.start(mech, &ctx) {
            Ok(s) => s,
            Err(e) => {
                self.unavailable(&e);
                return Ok(());
            }
        };
        loop {
            let step = match sasl.step(response.as_deref()).await {
                Ok(step) => step,
                Err(Error::Invalid(_)) => {
                    self.auth_failed();
                    return Ok(());
                }
                Err(e) => {
                    self.unavailable(&e);
                    return Ok(());
                }
            };
            match step {
                SaslStep::Challenge(c) => {
                    self.out
                        .extend_from_slice(format!("+ {}\r\n", STANDARD.encode(c)).as_bytes());
                    let Some(line) = self.read_line().await? else {
                        return Ok(());
                    };
                    if line == b"*" {
                        self.error("Authentication cancelled");
                        return Ok(());
                    }
                    match STANDARD.decode(&line) {
                        Ok(v) => response = Some(v),
                        Err(_) => {
                            self.error("Invalid base64");
                            return Ok(());
                        }
                    }
                }
                SaslStep::Done(p) => return self.logged_in(p).await,
                SaslStep::Fail(_) => {
                    self.auth_failed();
                    return Ok(());
                }
            }
        }
    }

    fn auth_failed(&mut self) {
        self.reply(false, "[AUTH] Authentication failed");
        self.auth_failures += 1;
        if self.auth_failures >= self.ctx.config.pop3.max_auth_failures {
            self.close = true;
        }
    }

    fn unavailable(&mut self, e: &Error) {
        tracing::warn!(error = %e, "pop3: вход недоступен");
        self.reply(false, "[SYS/TEMP] Authentication temporarily unavailable");
    }

    /// После входа — список писем «Входящих».
    async fn logged_in(&mut self, p: Principal) -> Result<()> {
        let scope = AccountScope {
            tenant: p.tenant,
            account: p.account,
        };
        let deps = &self.ctx.deps;
        let inbox = match deps.mailboxes.list(scope).await {
            Ok(list) => list.into_iter().find(|m| m.role == Some(SpecialUse::Inbox)),
            Err(e) => {
                self.unavailable(&e);
                return Ok(());
            }
        };
        let Some(inbox) = inbox else {
            self.reply(false, "[SYS/PERM] No inbox");
            return Ok(());
        };
        let limit = self.ctx.config.pop3.max_messages;
        let list = match deps
            .messages
            .list(scope, inbox.id, MessageQuery::All, limit.saturating_add(1))
            .await
        {
            Ok(list) => list,
            Err(e) => {
                self.unavailable(&e);
                return Ok(());
            }
        };
        if list.len() > limit {
            self.reply(false, "[SYS/PERM] Mailbox is too large for POP3");
            return Ok(());
        }
        tracing::debug!(account = %p.account, "pop3: вход выполнен");
        let items: Vec<Item> = list
            .into_iter()
            .map(|m| Item {
                id: m.id,
                size: m.size,
                deleted: false,
            })
            .collect();
        let n = items.len();
        self.drop = Some(Maildrop { scope, items });
        self.reply(true, &format!("Logged in, {n} messages"));
        Ok(())
    }

    // ---------- Состояние TRANSACTION ----------

    fn stat(&mut self) {
        let Some(d) = self.drop.as_ref() else { return };
        let (n, size) = d
            .live()
            .fold((0usize, 0u64), |(n, s), (_, m)| (n + 1, s + m.size));
        self.reply(true, &format!("{n} {size}"));
    }

    /// `LIST` (размеры) или `UIDL` (идентификаторы).
    fn list(&mut self, args: &[&str], uidl: bool) {
        let Some(d) = self.drop.as_ref() else { return };
        let item = |n: usize, m: &Item| {
            if uidl {
                format!("{n} {}", m.id)
            } else {
                format!("{n} {}", m.size)
            }
        };
        if let Some(arg) = args.first() {
            match d.get(arg) {
                Some((n, m)) => {
                    let line = item(n, m);
                    self.reply(true, &line);
                }
                None => self.error("No such message"),
            }
            return;
        }
        let lines: Vec<String> = d.live().map(|(n, m)| item(n, m)).collect();
        self.reply(true, &format!("{} messages", lines.len()));
        for l in lines {
            self.out.extend_from_slice(l.as_bytes());
            self.out.extend_from_slice(b"\r\n");
        }
        self.out.extend_from_slice(b".\r\n");
    }

    /// `RETR n` или `TOP n k` (`top` — строк тела).
    async fn retr(&mut self, args: &[&str], top: Option<usize>) -> Result<()> {
        let Some(d) = self.drop.as_ref() else {
            return Ok(());
        };
        let Some((_, m)) = args.first().and_then(|a| d.get(a)) else {
            self.error("No such message");
            return Ok(());
        };
        let (scope, id, size) = (d.scope, m.id, m.size);
        let mut stream = match self.ctx.deps.store.fetch_raw(scope, id, None).await {
            Ok(s) => s,
            Err(Error::NotFound) => {
                self.error("Message was deleted by another session");
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        // Первая часть — до `+OK`: письмо, удалённое другой сессией, — `-ERR`, а не обрыв ответа.
        let first = match stream.next().await {
            Some(Ok(chunk)) => Some(chunk),
            Some(Err(Error::NotFound)) => {
                self.error("Message was deleted by another session");
                return Ok(());
            }
            Some(Err(e)) => return Err(e),
            None => None,
        };
        self.reply(true, &format!("{size} octets"));
        let mut stuff = DotStuff::new();
        // TOP: заголовок целиком, затем `k` строк тела.
        let mut body_lines: Option<usize> = None;
        let mut header_tail = [0u8; 4];
        let mut done = false;
        let mut chunk = first;
        while let Some(data) = chunk.take() {
            match top {
                None => stuff.push(&data, &mut self.out),
                Some(k) => {
                    let mut end = data.len();
                    for (i, &b) in data.iter().enumerate() {
                        match body_lines.as_mut() {
                            None => {
                                header_tail = [header_tail[1], header_tail[2], header_tail[3], b];
                                if header_tail == *b"\r\n\r\n" || header_tail[2..] == b"\n\n"[..] {
                                    body_lines = Some(0);
                                    if k == 0 {
                                        end = i + 1;
                                        done = true;
                                        break;
                                    }
                                }
                            }
                            Some(lines) if b == b'\n' => {
                                *lines += 1;
                                if *lines >= k {
                                    end = i + 1;
                                    done = true;
                                    break;
                                }
                            }
                            Some(_) => {}
                        }
                    }
                    stuff.push(&data[..end], &mut self.out);
                }
            }
            self.maybe_flush().await?;
            if done {
                break;
            }
            chunk = match stream.next().await {
                Some(Ok(c)) => Some(c),
                Some(Err(e)) => return Err(e),
                None => None,
            };
        }
        stuff.finish(&mut self.out);
        Ok(())
    }

    fn dele(&mut self, args: &[&str]) {
        let Some(d) = self.drop.as_mut() else { return };
        let n = args.first().and_then(|a| a.parse::<usize>().ok());
        match n.and_then(|n| d.items.get_mut(n.checked_sub(1)?)) {
            Some(m) if !m.deleted => {
                m.deleted = true;
                self.reply(true, "Marked for deletion");
            }
            Some(_) => self.error("Message already deleted"),
            None => self.error("No such message"),
        }
    }

    fn rset(&mut self) {
        let Some(d) = self.drop.as_mut() else { return };
        for m in &mut d.items {
            m.deleted = false;
        }
        let n = d.items.len();
        self.reply(true, &format!("{n} messages"));
    }

    /// `QUIT`: в состоянии TRANSACTION — удаление помеченных (UPDATE, RFC 1939, 6).
    async fn quit(&mut self) -> Result<()> {
        self.close = true;
        let Some(d) = self.drop.take() else {
            self.reply(true, "Bye");
            return Ok(());
        };
        let ids: Vec<MessageId> = d.items.iter().filter(|m| m.deleted).map(|m| m.id).collect();
        if !ids.is_empty()
            && let Err(e) = self.ctx.deps.store.delete_to_trash(d.scope, &ids).await
        {
            tracing::warn!(error = %e, "pop3: письма не удалены");
            self.reply(false, "[SYS/TEMP] Some messages were not deleted");
            return Ok(());
        }
        self.reply(true, "Bye");
        Ok(())
    }
}

/// Сервер POP3 одного слушателя.
#[derive(Debug, Clone)]
pub struct Pop3Server {
    ctx: Arc<SessionCtx>,
}

impl Pop3Server {
    /// Сервер с контекстом сессий (секция `imap.pop3`).
    pub fn new(ctx: SessionCtx) -> Self {
        Self { ctx: Arc::new(ctx) }
    }

    /// Принимает соединения до остановки узла (POP3 выключен — сразу возвращает `Ok`), затем ждёт сессии
    /// не дольше `imap.timeouts.drain` и прерывает оставшиеся.
    pub async fn serve(&self, listener: Listener) -> Result<()> {
        if !self.ctx.config.pop3.enabled {
            return Ok(());
        }
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
                            tracing::warn!(error = %e, "pop3: ошибка приёма соединения");
                            continue;
                        }
                    };
                    let full = sessions.len() >= self.ctx.config.pop3.max_connections;
                    let ctx = self.ctx.clone();
                    let task = async move {
                        let conn = match incoming.establish().await {
                            Ok(conn) => conn,
                            Err(e) => {
                                tracing::debug!(error = %e, "pop3: соединение не установлено");
                                return;
                            }
                        };
                        if full {
                            reject(conn).await;
                            return;
                        }
                        if let Err(e) = Pop3Session::new(ctx, conn).run().await {
                            tracing::debug!(error = %e, "pop3: сессия прервана");
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
        if tokio::time::timeout(drain, async {
            while sessions.join_next().await.is_some() {}
        })
        .await
        .is_err()
        {
            sessions.abort_all();
        }
        Ok(())
    }
}

async fn reject(mut conn: Conn) {
    let _ = conn
        .stream
        .write_all(b"-ERR [SYS/TEMP] Too many connections\r\n")
        .await;
    let _ = conn.stream.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dot_stuffing() {
        let mut out = Vec::new();
        let mut s = DotStuff::new();
        s.push(b".a\r\nb\r", &mut out);
        s.push(b"\n.c", &mut out);
        s.finish(&mut out);
        assert_eq!(out, b"..a\r\nb\r\n..c\r\n.\r\n");
        let mut out = Vec::new();
        let mut s = DotStuff::new();
        s.push(b"x\r\n", &mut out);
        s.finish(&mut out);
        assert_eq!(out, b"x\r\n.\r\n");
    }

    #[test]
    fn lines() {
        let mut buf = b"USER a\r\nPASS".to_vec();
        assert_eq!(split_line(&mut buf, 100), Ok(Some(b"USER a".to_vec())));
        assert_eq!(split_line(&mut buf, 100), Ok(None));
        buf.extend_from_slice(&[b'x'; 200]);
        assert_eq!(split_line(&mut buf, 100), Err(()));
    }
}
