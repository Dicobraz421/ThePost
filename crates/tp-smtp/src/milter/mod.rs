//! Milter-клиент (SRS 4.1): антиспам и антивирус по протоколу Milter v6 (Rspamd, clamav-milter и др.).
//!
//! [`Milters`] — цепочка фильтров секции `smtp.milter`; письмо проходит фильтры по порядку после приёма
//! (`DATA`/`BDAT`), до доставки и постановки в очередь. Каждому фильтру — отдельное соединение: согласование
//! (`O`), макросы (`D`), `C`, `H`, `M`, `R`, `T`, заголовки (`L`), `N`, тело (`B`), `E`, `Q`; шаги, от
//! которых фильтр отказался при согласовании (`SMFIP_NO*`, `SMFIP_NR_*`), не отправляются или идут без ответа.
//!
//! Действия фильтра:
//! - `continue`/`accept` — следующий фильтр (после `accept` этот фильтр письмо больше не видит);
//! - `reject`, `replycode` (5xx) — отказ `550`/код фильтра; `tempfail`, `replycode` (4xx) — `451`/код фильтра;
//! - `discard` — письмо принято (`250`) и никому не доставляется;
//! - добавление, вставка и изменение заголовков (`h`, `i`, `m`) — применяются к письму, следующий фильтр
//!   видит изменённое письмо;
//! - `quarantine` — smtp-in доставляет письмо в «Спам», submission отклоняет (`550 5.7.1`): исходящее письмо
//!   в карантине наружу не уходит.
//!
//! Отказ фильтра на шаге до конца тела (`MAIL`, `RCPT`, заголовки) отклоняет всё письмо: проверка идёт после
//! приёма, ответить отказом отдельному получателю уже нельзя. Другие изменения (тело, получатели, отправитель)
//! не предлагаются при согласовании.
//!
//! Таймауты — `connect_timeout`, `command_timeout` (ответ на команду), `end_timeout` (ответ на конец тела;
//! `progress` фильтра продлевает ожидание). Недоступность фильтра (соединение, таймаут, нарушение протокола) —
//! по `on_failure`: `accept` — письмо идёт дальше с заголовком `X-ThePost-Milter: <имя>; unavailable`,
//! `tempfail` — `451 4.7.1`.
//!
//! Готовый крейт клиента (`miltr-client`, MIT) не подошёл: не разбирает ответ `accept` (`SMFIR_ACCEPT`),
//! не умеет отправлять макросы и по описанию автора — тестовый клиент; поэтому здесь минимальная реализация
//! протокола ([`proto`]).

use std::fmt;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

use crate::config::{MilterConfig, MilterFilter, MilterOnFailure};
use crate::server::{Envelope, Mode, Reply, reply};

pub mod proto;

use proto::{Packet, ProtoError};

/// Заголовок письма, пропущенного без недоступного фильтра.
pub const UNAVAILABLE_HEADER: &str = "X-ThePost-Milter";

/// Решение цепочки фильтров.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Принять (письмо — в [`Outcome::message`]).
    Accept,
    /// Отказ или временный отказ с этим ответом SMTP.
    Refuse(Reply),
    /// Принять и не доставлять.
    Discard,
}

/// Итог проверки письма цепочкой.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// Решение.
    pub verdict: Verdict,
    /// Письмо с изменениями заголовков от фильтров.
    pub message: Bytes,
    /// Причина карантина (`quarantine`), если фильтр его запросил.
    pub quarantine: Option<String>,
}

/// Цепочка фильтров Milter.
#[derive(Clone)]
pub struct Milters {
    cfg: MilterConfig,
    hostname: String,
}

impl fmt::Debug for Milters {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Milters")
            .field("filters", &self.cfg.filters.len())
            .finish_non_exhaustive()
    }
}

/// Изменение заголовков от фильтра.
#[derive(Debug, Clone, PartialEq, Eq)]
enum HeaderMod {
    /// В конец заголовков.
    Add(String, String),
    /// На позицию (0 — перед первым).
    Insert(usize, String, String),
    /// Замена `n`-го (с 1) заголовка с именем; пустое значение — удаление.
    Change(usize, String, String),
}

/// Ответ шага: дальше или итог фильтра.
macro_rules! step {
    ($e:expr) => {
        match $e {
            Step::Next | Step::Skip => {}
            Step::Done(d) => return Ok(d),
        }
    };
}
/// Итог одного фильтра.
enum Filtered {
    Continue,
    Refuse(Reply),
    Discard,
}

impl Milters {
    /// Цепочка из секции `smtp.milter`; `hostname` — макрос `j`.
    pub fn new(cfg: MilterConfig, hostname: impl Into<String>) -> Self {
        Self {
            cfg,
            hostname: hostname.into(),
        }
    }

    /// Есть фильтры для режима `mode`.
    pub fn applies(&self, mode: Mode) -> bool {
        self.cfg.filters.iter().any(|f| f.applies(mode))
    }

    /// Проверяет письмо `raw` транзакции `env` фильтрами её режима по порядку.
    pub async fn check(&self, env: &Envelope, raw: Bytes) -> Outcome {
        let mut out = Outcome {
            verdict: Verdict::Accept,
            message: raw,
            quarantine: None,
        };
        for f in self.cfg.filters.iter().filter(|f| f.applies(env.mode)) {
            let mut mods = Vec::new();
            let res = self
                .filter(f, env, &out.message, &mut mods, &mut out.quarantine)
                .await;
            match res {
                Ok(Filtered::Continue) => {
                    if !mods.is_empty() {
                        out.message = apply(&out.message, &mods);
                    }
                }
                Ok(Filtered::Refuse(r)) => {
                    tracing::info!(id = %env.id, milter = %f.name, code = r.code, "milter: письмо отклонено");
                    out.verdict = Verdict::Refuse(r);
                    return out;
                }
                Ok(Filtered::Discard) => {
                    tracing::info!(id = %env.id, milter = %f.name, "milter: письмо отброшено");
                    out.verdict = Verdict::Discard;
                    return out;
                }
                Err(e) => {
                    tracing::warn!(id = %env.id, milter = %f.name, error = %e, "milter: фильтр недоступен");
                    match f.on_failure {
                        MilterOnFailure::Accept => {
                            let h = format!("{UNAVAILABLE_HEADER}: {}; unavailable\r\n", f.name);
                            let mut m = BytesMut::from(h.as_bytes());
                            m.extend_from_slice(&out.message);
                            out.message = m.freeze();
                        }
                        MilterOnFailure::Tempfail => {
                            out.verdict = Verdict::Refuse(reply(
                                451,
                                [4, 7, 1],
                                "Content filter unavailable, try again later",
                            ));
                            return out;
                        }
                    }
                }
            }
        }
        out
    }

    async fn filter(
        &self,
        f: &MilterFilter,
        env: &Envelope,
        raw: &[u8],
        mods: &mut Vec<HeaderMod>,
        quarantine: &mut Option<String>,
    ) -> Result<Filtered, ProtoError> {
        let stream = tokio::time::timeout(f.connect_timeout, TcpStream::connect(&f.address))
            .await
            .map_err(|_| ProtoError::Timeout)??;
        let mut s = Conversation {
            io: stream,
            filter: f,
            neg: proto::OptNeg {
                version: proto::VERSION,
                actions: proto::ACTIONS,
                protocol: proto::PROTOCOL,
            },
        };
        let res = self.converse(&mut s, env, raw, mods, quarantine).await;
        // Завершение сессии — без ожидания ответа; ошибка закрытия не меняет решение.
        let _ = proto::write(&mut s.io, &Packet::new(proto::C_QUIT, Bytes::new())).await;
        res
    }

    async fn converse<S: AsyncRead + AsyncWrite + Unpin>(
        &self,
        s: &mut Conversation<'_, S>,
        env: &Envelope,
        raw: &[u8],
        mods: &mut Vec<HeaderMod>,
        quarantine: &mut Option<String>,
    ) -> Result<Filtered, ProtoError> {
        s.negotiate().await?;
        let ip = env.conn.client.ip().to_canonical();
        let id = env.id.as_bytes();
        let client_addr = ip.to_string();
        s.macros(
            proto::C_CONNECT,
            &[
                ("j", self.hostname.as_bytes()),
                ("{daemon_name}", b"thepost"),
                ("{client_addr}", client_addr.as_bytes()),
                ("{client_name}", env.helo.as_bytes()),
            ],
        )
        .await?;
        let mut connect = BytesMut::new();
        connect.extend_from_slice(env.helo.as_bytes());
        connect.extend_from_slice(b"\0");
        let (family, addr) = match ip {
            std::net::IpAddr::V4(a) => (b'4', a.to_string()),
            std::net::IpAddr::V6(a) => (b'6', a.to_string()),
        };
        connect.extend_from_slice(&[family]);
        connect.extend_from_slice(&env.conn.client.port().to_be_bytes());
        connect.extend_from_slice(addr.as_bytes());
        connect.extend_from_slice(b"\0");
        step!(
            s.command(
                Packet::new(proto::C_CONNECT, connect.freeze()),
                proto::P_NOCONNECT,
                proto::P_NR_CONN
            )
            .await?
        );
        step!(
            s.command(
                Packet::strings(proto::C_HELO, &[env.helo.as_bytes()]),
                proto::P_NOHELO,
                proto::P_NR_HELO
            )
            .await?
        );
        let login = env.principal.as_ref().map(|p| p.login.as_bytes());
        let mut mail_macros: Vec<(&str, &[u8])> =
            vec![("i", id), ("{mail_addr}", env.mail_from.as_bytes())];
        if let Some(login) = login {
            mail_macros.push(("{auth_authen}", login));
        }
        s.macros(proto::C_MAIL, &mail_macros).await?;
        let from = format!("<{}>", env.mail_from);
        step!(
            s.command(
                Packet::strings(proto::C_MAIL, &[from.as_bytes()]),
                proto::P_NOMAIL,
                proto::P_NR_MAIL
            )
            .await?
        );
        for rcpt in &env.rcpts {
            s.macros(proto::C_RCPT, &[("{rcpt_addr}", rcpt.as_bytes())])
                .await?;
            let to = format!("<{rcpt}>");
            step!(
                s.command(
                    Packet::strings(proto::C_RCPT, &[to.as_bytes()]),
                    proto::P_NORCPT,
                    proto::P_NR_RCPT
                )
                .await?
            );
        }
        s.macros(proto::C_DATA, &[("i", id)]).await?;
        step!(
            s.command(
                Packet::new(proto::C_DATA, Bytes::new()),
                proto::P_NODATA,
                proto::P_NR_DATA
            )
            .await?
        );
        let (headers, body) = split_message(raw);
        let leadspc = s.neg.protocol & proto::P_HDR_LEADSPC != 0;
        for (name, value) in headers {
            let value = if leadspc {
                value.to_owned()
            } else {
                value.trim_start_matches([' ', '\t']).to_owned()
            };
            let value = value.replace("\r\n", "\n");
            step!(
                s.command(
                    Packet::strings(proto::C_HEADER, &[name.as_bytes(), value.as_bytes()]),
                    proto::P_NOHDRS,
                    proto::P_NR_HDR
                )
                .await?
            );
        }
        s.macros(proto::C_EOH, &[("i", id)]).await?;
        step!(
            s.command(
                Packet::new(proto::C_EOH, Bytes::new()),
                proto::P_NOEOH,
                proto::P_NR_EOH
            )
            .await?
        );
        for chunk in body.chunks(proto::BODY_CHUNK) {
            match s
                .command(
                    Packet::new(proto::C_BODY, Bytes::copy_from_slice(chunk)),
                    proto::P_NOBODY,
                    proto::P_NR_BODY,
                )
                .await?
            {
                Step::Next => {}
                Step::Skip => break,
                Step::Done(d) => return Ok(d),
            }
        }
        s.macros(proto::C_BODYEOB, &[("i", id)]).await?;
        proto::write(&mut s.io, &Packet::new(proto::C_BODYEOB, Bytes::new())).await?;
        loop {
            let p = s.read(s.filter.end_timeout).await?;
            match p.code {
                proto::R_PROGRESS => {}
                proto::R_ADDHEADER => {
                    if let [name, value] = p.split()[..] {
                        mods.push(HeaderMod::Add(text(name), text(value)));
                    }
                }
                proto::R_INSHEADER | proto::R_CHGHEADER => {
                    let mut d = p.data.clone();
                    let index = proto::take_u32(&mut d).unwrap_or_default() as usize;
                    let rest = Packet::new(p.code, d);
                    if let [name, value] = rest.split()[..] {
                        let (name, value) = (text(name), text(value));
                        mods.push(if p.code == proto::R_INSHEADER {
                            HeaderMod::Insert(index, name, value)
                        } else {
                            HeaderMod::Change(index, name, value)
                        });
                    }
                }
                proto::R_QUARANTINE => {
                    let reason = p.split().first().map(|r| text(r)).unwrap_or_default();
                    *quarantine = Some(reason);
                }
                proto::R_ADDRCPT | proto::R_DELRCPT | proto::R_REPLBODY => {
                    tracing::warn!(milter = %s.filter.name, code = %char::from(p.code), "milter: изменение не согласовано, пропуск");
                }
                _ => {
                    return Ok(match final_action(&p)? {
                        Some(d) => d,
                        None => Filtered::Continue,
                    });
                }
            }
        }
    }
}

/// Шаг протокола до конца тела.
enum Step {
    Next,
    Skip,
    Done(Filtered),
}

struct Conversation<'a, S> {
    io: S,
    filter: &'a MilterFilter,
    neg: proto::OptNeg,
}

impl<S: AsyncRead + AsyncWrite + Unpin> Conversation<'_, S> {
    async fn read(&mut self, timeout: Duration) -> Result<Packet, ProtoError> {
        tokio::time::timeout(timeout, proto::read(&mut self.io))
            .await
            .map_err(|_| ProtoError::Timeout)?
    }

    async fn negotiate(&mut self) -> Result<(), ProtoError> {
        proto::write(&mut self.io, &self.neg.packet()).await?;
        let p = self.read(self.filter.command_timeout).await?;
        let theirs = proto::OptNeg::parse(&p)?;
        if theirs.version < 2 {
            return Err(ProtoError::Protocol(format!(
                "unsupported version {}",
                theirs.version
            )));
        }
        if theirs.protocol & !proto::PROTOCOL != 0 {
            return Err(ProtoError::Protocol(format!(
                "unsupported protocol steps {:#x}",
                theirs.protocol & !proto::PROTOCOL
            )));
        }
        self.neg = proto::OptNeg {
            version: theirs.version.min(proto::VERSION),
            actions: theirs.actions & proto::ACTIONS,
            protocol: theirs.protocol,
        };
        Ok(())
    }

    /// Макросы для команды `cmd` (только версии 2+, всегда без ответа).
    async fn macros(&mut self, cmd: u8, macros: &[(&str, &[u8])]) -> Result<(), ProtoError> {
        let mut b = BytesMut::new();
        b.extend_from_slice(&[cmd]);
        for (name, value) in macros {
            b.extend_from_slice(name.as_bytes());
            b.extend_from_slice(b"\0");
            b.extend_from_slice(value);
            b.extend_from_slice(b"\0");
        }
        proto::write(&mut self.io, &Packet::new(proto::C_MACRO, b.freeze())).await?;
        Ok(())
    }

    /// Команда шага: пропуск (`no`), без ответа (`nr`) или ожидание ответа.
    async fn command(&mut self, p: Packet, no: u32, nr: u32) -> Result<Step, ProtoError> {
        if self.neg.protocol & no != 0 {
            return Ok(Step::Next);
        }
        proto::write(&mut self.io, &p).await?;
        if self.neg.protocol & nr != 0 {
            return Ok(Step::Next);
        }
        loop {
            let r = self.read(self.filter.command_timeout).await?;
            return match r.code {
                proto::R_PROGRESS => continue,
                proto::R_CONTINUE => Ok(Step::Next),
                proto::R_SKIP if p.code == proto::C_BODY => Ok(Step::Skip),
                _ => Ok(match final_action(&r)? {
                    Some(d) => Step::Done(d),
                    None => Step::Done(Filtered::Continue),
                }),
            };
        }
    }
}

/// Итоговое действие фильтра: `None` — `accept`/`continue` (письмо идёт дальше).
fn final_action(p: &Packet) -> Result<Option<Filtered>, ProtoError> {
    Ok(match p.code {
        proto::R_ACCEPT | proto::R_CONTINUE => None,
        proto::R_REJECT => Some(Filtered::Refuse(reply(
            550,
            [5, 7, 1],
            "Message rejected by content filter",
        ))),
        proto::R_TEMPFAIL => Some(Filtered::Refuse(reply(
            451,
            [4, 7, 1],
            "Message deferred by content filter",
        ))),
        proto::R_DISCARD => Some(Filtered::Discard),
        proto::R_REPLYCODE => match proto::parse_replycode(p) {
            Some((code, esc, text)) => Some(Filtered::Refuse(reply(code, esc, text))),
            None => {
                return Err(ProtoError::Protocol("bad reply code".into()));
            }
        },
        c => {
            return Err(ProtoError::Protocol(format!(
                "unexpected response {:?}",
                char::from(c)
            )));
        }
    })
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// Заголовки (имя, значение без `:`, со свёрткой `\r\n`) и тело письма.
fn split_message(raw: &[u8]) -> (Vec<(String, String)>, &[u8]) {
    let (head, body) = split_head(raw);
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in head.split_inclusive(|b| *b == b'\n') {
        let line = String::from_utf8_lossy(line);
        let line = line.trim_end_matches(['\r', '\n']);
        if line.starts_with([' ', '\t']) {
            if let Some((_, v)) = headers.last_mut() {
                v.push_str("\r\n");
                v.push_str(line);
            }
        } else if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim_end().to_owned(), value.to_owned()));
        }
    }
    (headers, body)
}

/// Блок заголовков (с завершающим `\r\n` последнего) и тело (после пустой строки).
fn split_head(raw: &[u8]) -> (&[u8], &[u8]) {
    if raw.starts_with(b"\r\n") {
        return (&[], &raw[2..]);
    }
    match raw.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(i) => (&raw[..i + 2], &raw[i + 4..]),
        None => (raw, &[]),
    }
}

/// Применяет изменения заголовков к письму.
fn apply(raw: &[u8], mods: &[HeaderMod]) -> Bytes {
    let (head, body) = split_head(raw);
    // Заголовки — целыми строками со свёрткой.
    let mut fields: Vec<String> = Vec::new();
    for line in String::from_utf8_lossy(head).split_inclusive('\n') {
        if line.starts_with([' ', '\t'])
            && let Some(last) = fields.last_mut()
        {
            last.push_str(line);
        } else {
            fields.push(line.to_owned());
        }
    }
    let field = |name: &str, value: &str| {
        // Значение от фильтра: `\n` свёртки — в `\r\n`, без управляющих символов вне свёртки.
        let value = value.replace("\r\n", "\n").replace('\r', "");
        let value = value.replace('\n', "\r\n");
        let sep = if value.starts_with([' ', '\t']) {
            ""
        } else {
            " "
        };
        format!("{name}:{sep}{value}\r\n")
    };
    let is = |f: &str, name: &str| {
        f.split_once(':')
            .is_some_and(|(n, _)| n.trim_end().eq_ignore_ascii_case(name))
    };
    for m in mods {
        match m {
            HeaderMod::Add(name, value) => fields.push(field(name, value)),
            HeaderMod::Insert(i, name, value) => {
                let i = (*i).min(fields.len());
                fields.insert(i, field(name, value));
            }
            HeaderMod::Change(n, name, value) => {
                let pos = fields
                    .iter()
                    .enumerate()
                    .filter(|(_, f)| is(f, name))
                    .nth(n.saturating_sub(1))
                    .map(|(i, _)| i);
                match (pos, value.is_empty()) {
                    (Some(i), true) => {
                        fields.remove(i);
                    }
                    (Some(i), false) => fields[i] = field(name, value),
                    // Изменение несуществующего заголовка — добавление (как в Sendmail).
                    (None, false) => fields.push(field(name, value)),
                    (None, true) => {}
                }
            }
        }
    }
    let mut out = BytesMut::with_capacity(raw.len() + 256);
    for f in &fields {
        out.extend_from_slice(f.as_bytes());
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body);
    out.freeze()
}

impl MilterFilter {
    fn applies(&self, mode: Mode) -> bool {
        match mode {
            Mode::Inbound => self.inbound,
            Mode::Submission => self.submission,
        }
    }
}

#[cfg(test)]
mod tests;
