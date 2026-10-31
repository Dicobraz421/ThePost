//! `AUTH` (RFC 4954) роли submission поверх [`SaslServer`]: механизмы — `tp-auth`, здесь — команда,
//! base64, отмена `*` и коды ответов.
//!
//! AUTH предлагается и принимается только по TLS (SRS 4.1, 6.2): без TLS — `538 5.7.11`. Попытки
//! ограничиваются по адресу клиента ([`RateLimiter::check_ip`]); после `limits.max_auth_failures`
//! неудач подряд адрес блокируется, соединение закрывается. Частота отправки — по учётной записи
//! ([`RateLimiter::check_account`]) на каждый `MAIL`.

use std::time::Instant;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use smtp_proto::request::receiver::LineReceiver;
use smtp_proto::{
    AUTH_GSSAPI, AUTH_LOGIN, AUTH_OAUTHBEARER, AUTH_PLAIN, AUTH_XOAUTH2, EXT_AUTH, EhloResponse,
};
use tp_contracts::Error;
use tp_contracts::sasl::{Mechanism, SaslContext, SaslFailure, SaslSession, SaslStep};
use tp_platform::Verdict;

use super::session::{Exit, Session, State, push, push_line};
use super::{Mode, Reply, reply};

/// Идущий обмен SASL: сеанс механизма и строка ответа клиента.
pub(super) struct Exchange {
    sasl: Box<dyn SaslSession>,
    rx: LineReceiver<()>,
}

/// Бит механизма в ответе `EHLO` (`smtp-proto`).
fn bit(m: Mechanism) -> u64 {
    match m {
        Mechanism::Plain => AUTH_PLAIN,
        Mechanism::Login => AUTH_LOGIN,
        Mechanism::OauthBearer => AUTH_OAUTHBEARER,
        Mechanism::Xoauth2 => AUTH_XOAUTH2,
        Mechanism::Gssapi => AUTH_GSSAPI,
    }
}

/// Механизм по биту из команды `AUTH`.
fn from_bit(b: u64) -> Option<Mechanism> {
    Mechanism::ALL.into_iter().find(|m| bit(*m) == b)
}

impl Session {
    /// Расширение `AUTH` в ответе `EHLO`: submission, по TLS, до успешной аутентификации.
    pub(super) fn advertise_auth(&self, ehlo: &mut EhloResponse<&str>) {
        if self.ctx.mode != Mode::Submission || !self.conn.tls || self.principal.is_some() {
            return;
        }
        let Some(sasl) = &self.ctx.sasl else {
            return;
        };
        let mechanisms = sasl
            .mechanisms(true)
            .into_iter()
            .fold(0, |acc, m| acc | bit(m));
        if mechanisms != 0 {
            ehlo.capabilities |= EXT_AUTH;
            ehlo.auth_mechanisms = mechanisms;
        }
    }

    /// Команда `AUTH`; `Some` — обмен продолжается (`334`).
    pub(super) async fn auth(
        &mut self,
        mechanism: u64,
        initial: String,
        out: &mut Vec<u8>,
    ) -> Option<State> {
        let refuse = if self.ctx.mode != Mode::Submission {
            Some(reply(502, [5, 5, 1], "Command not implemented"))
        } else if !self.esmtp {
            Some(reply(503, [5, 5, 1], "Send EHLO first"))
        } else if self.principal.is_some() {
            Some(reply(503, [5, 5, 1], "Already authenticated"))
        } else if self.tx.is_some() {
            Some(reply(
                503,
                [5, 5, 1],
                "AUTH not permitted during a mail transaction",
            ))
        } else if !self.conn.tls {
            Some(reply(
                538,
                [5, 7, 11],
                "Encryption required for requested authentication mechanism",
            ))
        } else {
            None
        };
        if let Some(r) = refuse {
            self.fail(r, out);
            return None;
        }
        if let Some(limiter) = &self.ctx.limiter {
            match limiter.check_ip(self.conn.client.ip()) {
                Verdict::Allow => {}
                Verdict::Limited { .. } => {
                    push(
                        out,
                        &reply(
                            454,
                            [4, 7, 0],
                            "Too many authentication attempts, try later",
                        ),
                    );
                    return None;
                }
                Verdict::Blocked { .. } => {
                    push(
                        out,
                        &reply(421, [4, 7, 0], "Too many authentication failures"),
                    );
                    self.exit = Some(Exit::Closed);
                    return None;
                }
            }
        }
        let Some(mechanism) = from_bit(mechanism) else {
            self.fail(
                reply(504, [5, 5, 4], "Unrecognized authentication type"),
                out,
            );
            return None;
        };
        let ctx = SaslContext {
            service: "smtp",
            tls: self.conn.tls,
        };
        let started = match &self.ctx.sasl {
            Some(sasl) => sasl.start(mechanism, &ctx),
            None => Err(Error::invalid("no SASL server")),
        };
        let sasl = match started {
            Ok(s) => s,
            Err(Error::Invalid(_)) => {
                self.fail(
                    reply(504, [5, 5, 4], "Unrecognized authentication type"),
                    out,
                );
                return None;
            }
            Err(e) => {
                tracing::warn!(error = %e, "smtp: AUTH недоступен");
                push(
                    out,
                    &reply(454, [4, 7, 0], "Temporary authentication failure"),
                );
                return None;
            }
        };
        // Нет начального ответа — пусто; `=` — пустой начальный ответ (RFC 4954, 4).
        let response = match initial.as_str() {
            "" => None,
            "=" => Some(Vec::new()),
            b64 => match B64.decode(b64) {
                Ok(v) => Some(v),
                Err(_) => {
                    self.fail(reply(501, [5, 5, 2], "Invalid base64 data"), out);
                    return None;
                }
            },
        };
        let ex = Exchange {
            sasl,
            rx: LineReceiver::new(()),
        };
        self.auth_step(ex, response, out).await
    }

    /// Ответ клиента на вызов `334`.
    pub(super) async fn auth_bytes(
        &mut self,
        mut ex: Exchange,
        bytes: &mut std::slice::Iter<'_, u8>,
        out: &mut Vec<u8>,
    ) -> State {
        if !ex.rx.ingest(bytes) {
            return State::Auth(ex);
        }
        let line = std::mem::take(&mut ex.rx.buf);
        if line == b"*" {
            self.fail(reply(501, [5, 0, 0], "Authentication cancelled"), out);
            return State::command();
        }
        let Ok(response) = B64.decode(&line) else {
            self.fail(reply(501, [5, 5, 2], "Invalid base64 data"), out);
            return State::command();
        };
        self.auth_step(ex, Some(response), out)
            .await
            .unwrap_or_else(State::command)
    }

    async fn auth_step(
        &mut self,
        mut ex: Exchange,
        response: Option<Vec<u8>>,
        out: &mut Vec<u8>,
    ) -> Option<State> {
        match ex.sasl.step(response.as_deref()).await {
            Ok(SaslStep::Challenge(c)) => {
                push_line(out, 334, &B64.encode(c));
                Some(State::Auth(ex))
            }
            Ok(SaslStep::Done(p)) => {
                tracing::info!(client = %self.conn.client, login = %p.login, "smtp: AUTH");
                self.principal = Some(p);
                self.auth_failures = 0;
                push(out, &reply(235, [2, 7, 0], "Authentication successful"));
                None
            }
            Ok(SaslStep::Fail(f)) => {
                self.auth_failed(f, out);
                None
            }
            Err(Error::Invalid(_)) => {
                self.fail(
                    reply(501, [5, 5, 2], "Invalid authentication exchange"),
                    out,
                );
                None
            }
            Err(e) => {
                tracing::warn!(error = %e, "smtp: AUTH недоступен");
                push(
                    out,
                    &reply(454, [4, 7, 0], "Temporary authentication failure"),
                );
                None
            }
        }
    }

    /// Неуспешная аутентификация: `535`; после `limits.max_auth_failures` — блокировка адреса и закрытие.
    fn auth_failed(&mut self, f: SaslFailure, out: &mut Vec<u8>) {
        tracing::info!(client = %self.conn.client, failure = ?f, "smtp: AUTH отклонён");
        let r: Reply = match f {
            SaslFailure::Authentication => {
                reply(535, [5, 7, 8], "Authentication credentials invalid")
            }
            SaslFailure::Authorization => {
                reply(535, [5, 7, 8], "Authorization identity not permitted")
            }
            SaslFailure::Expired => reply(535, [5, 7, 8], "Credentials expired"),
        };
        push(out, &r);
        self.auth_failures += 1;
        if self.auth_failures >= self.ctx.config.limits.max_auth_failures {
            if let Some(limiter) = &self.ctx.limiter {
                limiter.block_ip_at(self.conn.client.ip(), Instant::now());
            }
            push(
                out,
                &reply(421, [4, 7, 0], "Too many authentication failures"),
            );
            self.exit = Some(Exit::Closed);
        }
    }

    /// Лимит частоты отправки учётной записи (submission); `Some` — отказ.
    pub(super) fn mail_rate_limit(&self) -> Option<Reply> {
        let (Some(limiter), Some(p)) = (&self.ctx.limiter, &self.principal) else {
            return None;
        };
        if limiter
            .check_account(&format!("submit:{}", p.login))
            .is_allowed()
        {
            None
        } else {
            Some(reply(
                451,
                [4, 7, 1],
                "Sending rate exceeded, try again later",
            ))
        }
    }
}
