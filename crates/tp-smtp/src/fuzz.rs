//! Проверки разборщиков SMTP на произвольных данных (SMTP-19; SRS 6.8): цели cargo-fuzz `crates/tp-smtp/fuzz`
//! и тесты крейта на примерах. Каждая функция паникует, только если нарушен инвариант.
//!
//! - [`session`] — поток команд клиента (smtp-in или submission по первому байту) через [`Session`]: разбор
//!   команд и данных (`DATA`, `BDAT`), конвейер, лимиты; ответы — корректные строки SMTP, первая — `220`,
//!   письмо обработчику — в пределах лимитов размера и числа получателей, без повторов получателей;
//! - [`esmtp`] — параметры расширений ESMTP в `MAIL FROM` и `RCPT TO` (`SIZE`, `BODY`, `SMTPUTF8`, DSN,
//!   `AUTH=`): строки входа подставляются после адресов, затем `DATA`; инварианты — как у [`session`];
//! - [`proxy_v2`] — заголовок PROXY protocol v2 перед сессией smtp-in: соединение loopback со слушателем
//!   `tp-platform` (`proxy_protocol`), затем сессия на установленном соединении; балансировщик — адрес
//!   собеседника TCP;
//! - [`milter`] — ответы фильтра Milter: байты входа — поток пакетов фильтра на всю переписку проверки письма;
//!   итог — принято, отказ `4xx`/`5xx` или отброс; письмо не теряется.

use std::sync::OnceLock;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use time::OffsetDateTime;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::runtime::Runtime;
use tp_platform::{ConnInfo, EndpointConfig, Listener};

use crate::config::{Limits, MilterConfig, MilterFilter, MilterOnFailure, SmtpConfig, Timeouts};
use crate::milter::{Milters, Verdict};
use crate::server::{Envelope, Handler, Mode, Reply, Session, SessionCtx, reply};

/// Лимиты сессии в целях: малые, чтобы границы достигались быстро.
const MAX_MESSAGE: usize = 64 * 1024;
const MAX_RCPTS: usize = 8;

fn runtime() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap_or_else(|e| panic!("tokio runtime: {e}"))
    })
}

/// Обработчик целей: получатели с `reject` в адресе отклоняются, письмо проверяется по лимитам.
struct Check;

#[async_trait]
impl Handler for Check {
    async fn rcpt(&self, env: &Envelope, rcpt: &str) -> Reply {
        assert!(env.rcpts.len() < MAX_RCPTS, "получатель сверх лимита");
        assert!(
            !env.rcpts.iter().any(|r| r.eq_ignore_ascii_case(rcpt)),
            "повтор получателя"
        );
        if rcpt.contains("reject") {
            reply(550, [5, 1, 1], "No such user")
        } else {
            reply(250, [2, 1, 5], "OK")
        }
    }

    async fn data(&self, env: &Envelope, raw: Bytes) -> Reply {
        assert!(!env.rcpts.is_empty(), "письмо без получателей");
        assert!(env.rcpts.len() <= MAX_RCPTS, "получателей сверх лимита");
        assert!(
            raw.len() <= MAX_MESSAGE,
            "письмо сверх лимита: {}",
            raw.len()
        );
        reply(250, [2, 0, 0], "OK")
    }
}

fn ctx(mode: Mode) -> SessionCtx {
    SessionCtx {
        mode,
        config: std::sync::Arc::new(SmtpConfig {
            hostname: "mx.example.org".into(),
            limits: Limits {
                max_message_size: MAX_MESSAGE,
                max_recipients: MAX_RCPTS,
                ..Limits::default()
            },
            timeouts: Timeouts {
                command: Duration::from_secs(5),
                data: Duration::from_secs(5),
            },
            ..SmtpConfig::default()
        }),
        handler: std::sync::Arc::new(Check),
        sasl: None,
        limiter: None,
        send_limits: None,
        metrics: None,
        screen: None,
    }
}

fn conn() -> ConnInfo {
    ConnInfo {
        client: ([192, 0, 2, 10], 40000).into(),
        local: ([198, 51, 100, 1], 25).into(),
        proxy: None,
        tls: false,
    }
}

/// Ответы сервера — строки `NNN<SP|->текст` с CRLF, коды 2xx–5xx, строки продолжения — с тем же кодом.
fn check_replies(out: &[u8], greet: bool) {
    if out.is_empty() {
        return;
    }
    assert!(out.ends_with(b"\r\n"), "ответ без CRLF в конце");
    let text = String::from_utf8_lossy(out);
    let mut cont: Option<&str> = None;
    for (i, line) in text.split_terminator("\r\n").enumerate() {
        let code = line
            .get(..3)
            .unwrap_or_else(|| panic!("короткая строка {line:?}"));
        assert!(
            code.bytes().all(|b| b.is_ascii_digit()) && (b'2'..=b'5').contains(&code.as_bytes()[0]),
            "код {line:?}"
        );
        if i == 0 && greet {
            assert_eq!(code, "220", "приветствие");
        }
        if let Some(c) = cont {
            assert_eq!(c, code, "код строки продолжения");
        }
        cont = match line.as_bytes().get(3) {
            Some(b'-') => Some(code),
            None | Some(b' ') => None,
            Some(_) => panic!("разделитель {line:?}"),
        };
    }
    assert!(cont.is_none(), "многострочный ответ не завершён");
}

/// Сессия над входом `input`; возвращает ответы сервера.
async fn run_session(mode: Mode, input: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut io = tokio::io::join(input, &mut out);
        let mut s = Session::new(ctx(mode), conn(), false);
        let _ = s.serve(&mut io, true).await;
    }
    out
}

/// Поток команд клиента: первый байт — режим (нечётный — submission), остальное — команды и данные.
pub fn session(data: &[u8]) {
    let Some((&first, input)) = data.split_first() else {
        return;
    };
    let mode = if first & 1 == 1 {
        Mode::Submission
    } else {
        Mode::Inbound
    };
    let out = runtime().block_on(run_session(mode, input));
    check_replies(&out, true);
}

/// Параметры ESMTP: первая строка входа — после `MAIL FROM:<…>`, следующие (до четырёх) — после
/// `RCPT TO:<…>`, остаток — тело письма.
pub fn esmtp(data: &[u8]) {
    let mut parts = data.splitn(6, |b| *b == b'\n');
    let mail = parts.next().unwrap_or_default();
    let mut input = b"EHLO client.example.net\r\nMAIL FROM:<s@example.net> ".to_vec();
    input.extend_from_slice(strip_cr(mail));
    input.extend_from_slice(b"\r\n");
    let mut rest: &[u8] = &[];
    for (i, p) in parts.enumerate() {
        if i == 4 {
            rest = p;
            break;
        }
        input.extend_from_slice(format!("RCPT TO:<r{i}@example.org> ").as_bytes());
        input.extend_from_slice(strip_cr(p));
        input.extend_from_slice(b"\r\n");
    }
    input.extend_from_slice(b"DATA\r\nSubject: x\r\n\r\n");
    input.extend_from_slice(rest);
    input.extend_from_slice(b"\r\n.\r\nQUIT\r\n");
    let out = runtime().block_on(run_session(Mode::Inbound, &input));
    check_replies(&out, true);
}

fn strip_cr(b: &[u8]) -> &[u8] {
    b.strip_suffix(b"\r").unwrap_or(b)
}

/// Слушатель loopback с PROXY protocol (один на процесс).
fn proxy_listener() -> &'static (Listener, std::net::SocketAddr) {
    static L: OnceLock<(Listener, std::net::SocketAddr)> = OnceLock::new();
    L.get_or_init(|| {
        runtime().block_on(async {
            let tcp = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .unwrap_or_else(|e| panic!("bind: {e}"));
            let at = tcp.local_addr().unwrap_or_else(|e| panic!("addr: {e}"));
            let mut cfg = EndpointConfig::new(at);
            cfg.proxy_protocol = true;
            cfg.handshake_timeout = Duration::from_secs(2);
            let l = Listener::from_tcp(tcp, &cfg, None).unwrap_or_else(|e| panic!("listener: {e}"));
            (l, at)
        })
    })
}

/// Заголовок PROXY v2 и следующий за ним поток команд smtp-in по соединению loopback.
pub fn proxy_v2(data: &[u8]) {
    proxy_session(data);
}

/// Сведения установленного соединения (`None` — заголовок отвергнут) и ответы сервера.
fn proxy_session(data: &[u8]) -> (Option<ConnInfo>, Vec<u8>) {
    let (listener, at) = proxy_listener();
    runtime().block_on(async {
        let mut client = tokio::net::TcpStream::connect(at)
            .await
            .unwrap_or_else(|e| panic!("connect: {e}"));
        // Без TIME_WAIT: длинный прогон не исчерпывает порты.
        let _ = client.set_zero_linger();
        let peer = client.local_addr().unwrap_or_else(|e| panic!("addr: {e}"));
        let server = async {
            let incoming = listener
                .accept()
                .await
                .unwrap_or_else(|e| panic!("accept: {e}"));
            assert_eq!(incoming.peer(), peer);
            let Ok(mut conn) = incoming.establish().await else {
                return None;
            };
            assert_eq!(
                conn.info.proxy,
                Some(peer),
                "балансировщик — собеседник TCP"
            );
            assert!(!conn.info.tls);
            let mut s = Session::new(ctx(Mode::Inbound), conn.info, false);
            let _ = s.serve(&mut conn.stream, true).await;
            Some(conn.info)
        };
        let feed = async {
            let _ = client.write_all(data).await;
            let _ = client.shutdown().await;
            let mut out = Vec::new();
            let _ = client.read_to_end(&mut out).await;
            out
        };
        let (info, out) = tokio::join!(server, feed);
        check_replies(&out, true);
        (info, out)
    })
}

/// Ответы фильтра Milter на проверку письма: `data` — поток пакетов от фильтра.
pub fn milter(data: &[u8]) {
    milter_outcome(data);
}

/// Решения цепочки при `on_failure` = `accept` и `tempfail`.
fn milter_outcome(data: &[u8]) -> Vec<Verdict> {
    let filter = |on_failure| MilterFilter {
        name: "fuzz".into(),
        address: "127.0.0.1:1".into(),
        on_failure: Some(on_failure),
        ..MilterFilter::default()
    };
    let raw = Bytes::from_static(b"From: a@example.net\r\nSubject: x\r\n\r\nbody\r\n");
    let env = Envelope {
        id: "fuzz".into(),
        mode: Mode::Inbound,
        conn: conn(),
        helo: "client.example.net".into(),
        esmtp: true,
        principal: None,
        mail_from: "a@example.net".into(),
        rcpts: vec!["b@example.org".into()],
        smtputf8: false,
        started_at: OffsetDateTime::UNIX_EPOCH,
        source: None,
        gateway: None,
    };
    let mut verdicts = Vec::new();
    for on_failure in [MilterOnFailure::Accept, MilterOnFailure::Tempfail] {
        let m = Milters::new(
            MilterConfig {
                filters: vec![filter(on_failure)],
            },
            "mx.example.org",
        );
        let input = Bytes::copy_from_slice(data);
        let out =
            runtime().block_on(
                m.check_with(&crate::milter::Mail::of(&env), raw.clone(), |_| {
                    let input = input.clone();
                    async move {
                        Ok(tokio::io::join(
                            std::io::Cursor::new(input),
                            tokio::io::sink(),
                        ))
                    }
                }),
            );
        match &out.verdict {
            Verdict::Accept => {
                assert!(
                    out.message.ends_with(b"\r\n\r\nbody\r\n"),
                    "тело письма изменено"
                );
            }
            Verdict::Refuse(r) => {
                assert!((400..600).contains(&r.code), "код отказа {}", r.code);
                assert!(!r.message.contains(['\r', '\n']), "перевод строки в ответе");
            }
            Verdict::Discard => {}
        }
        verdicts.push(out.verdict);
    }
    verdicts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn examples_pass_checks() {
        session(b"\0EHLO a\r\nMAIL FROM:<a@b>\r\nRCPT TO:<c@d>\r\nDATA\r\nx\r\n.\r\nQUIT\r\n");
        session(b"\0EHLO a\r\nMAIL FROM:<a@b>\r\nRCPT TO:<c@d>\r\nBDAT 3 LAST\r\nabcQUIT\r\n");
        session(b"\x01EHLO a\r\nAUTH PLAIN AGFiAGM=\r\nMAIL FROM:<a@b>\r\n");
        session(b"\0HELO a\r\nRCPT TO:<x>\r\nNOOP\r\nRSET\r\nVRFY x\r\n\xff\xfe\r\n");
        esmtp(b"SIZE=10 BODY=8BITMIME\nNOTIFY=SUCCESS,FAILURE ORCPT=rfc822;x@y\nreject\n\n\nbody");
        esmtp(b"SMTPUTF8 RET=HDRS ENVID=abc AUTH=<>\n\xff\n");
        // Пустой поток — фильтр недоступен: по политике.
        let unavailable = milter_outcome(b"");
        assert_eq!(unavailable[0], Verdict::Accept);
        assert!(matches!(&unavailable[1], Verdict::Refuse(r) if r.code == 451));
        // Согласование и «continue» на все шаги, затем «accept».
        let mut ok = Vec::new();
        let mut neg = vec![0, 0, 0, 13, b'O'];
        neg.extend_from_slice(&[0, 0, 0, 6, 0, 0, 0, 0, 0, 0, 0, 0]);
        ok.extend_from_slice(&neg);
        for _ in 0..16 {
            ok.extend_from_slice(&[0, 0, 0, 1, b'c']);
        }
        assert_eq!(milter_outcome(&ok), [Verdict::Accept, Verdict::Accept]);
        ok.truncate(neg.len());
        ok.extend_from_slice(&[0, 0, 0, 1, b'a']);
        assert_eq!(milter_outcome(&ok), [Verdict::Accept, Verdict::Accept]);
        ok.truncate(neg.len());
        ok.extend_from_slice(b"\0\0\0\x0ey550 5.7.1 no\0");
        assert!(
            milter_outcome(&ok)
                .iter()
                .all(|v| matches!(v, Verdict::Refuse(r) if r.code == 550))
        );
    }

    #[test]
    fn proxy_header_and_garbage() {
        // PROXY v2, PROXY TCP4 203.0.113.7:4242 → 198.51.100.1:25, затем команды.
        let mut h = b"\r\n\r\n\0\r\nQUIT\n".to_vec();
        h.extend_from_slice(&[
            0x21, 0x11, 0, 12, 203, 0, 113, 7, 198, 51, 100, 1, 0x10, 0x92, 0, 25,
        ]);
        h.extend_from_slice(b"EHLO a\r\nQUIT\r\n");
        let (info, out) = proxy_session(&h);
        let info = info.expect("established");
        assert_eq!(info.client, ([203, 0, 113, 7], 4242).into());
        assert!(
            String::from_utf8_lossy(&out).contains("\r\n221 "),
            "{out:?}"
        );
        assert_eq!(proxy_session(b"GET / HTTP/1.1\r\n\r\n").0, None);
        assert_eq!(proxy_session(b"").0, None);
    }

    #[test]
    fn arbitrary_bytes_do_not_panic() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        for len in [0usize, 1, 7, 64, 300, 2000] {
            let mut buf = Vec::with_capacity(len);
            for _ in 0..len {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                buf.push(b" \r\nEHLOMAILRCPTDATABDAT:<>@0123456789"[(seed % 37) as usize]);
            }
            session(&buf);
            esmtp(&buf);
            milter(&buf);
        }
    }
}
