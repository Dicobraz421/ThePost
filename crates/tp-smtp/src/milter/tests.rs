//! Цепочка Milter с мок-фильтром на `127.0.0.1`: согласование, макросы, действия, таймауты, недоступность.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::{BufMut, Bytes, BytesMut};
use time::OffsetDateTime;
use tokio::net::TcpListener;

use super::proto::{self, Packet};
use super::*;
use crate::config::{MilterConfig, MilterFilter, MilterOnFailure};
use crate::server::tests::conn_info;

type Script = Arc<dyn Fn(&Packet) -> Vec<Packet> + Send + Sync>;

/// Мок-фильтр: отвечает на согласование шагами `protocol`, на прочие команды — по сценарию `script`
/// (на макросы и `Q` ответа нет). Принятые пакеты — в `seen`.
struct Mock {
    address: String,
    seen: Arc<Mutex<Vec<Packet>>>,
}

impl Mock {
    async fn start(protocol: u32, script: Script) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("addr").to_string();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let rec = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                let rec = rec.clone();
                let script = script.clone();
                tokio::spawn(async move {
                    while let Ok(p) = proto::read(&mut s).await {
                        rec.lock().expect("lock").push(p.clone());
                        let answers = match p.code {
                            proto::C_OPTNEG => vec![
                                proto::OptNeg {
                                    version: 6,
                                    actions: proto::ACTIONS,
                                    protocol,
                                }
                                .packet(),
                            ],
                            proto::C_MACRO => vec![],
                            proto::C_QUIT => return,
                            _ => script(&p),
                        };
                        for a in answers {
                            if proto::write(&mut s, &a).await.is_err() {
                                return;
                            }
                        }
                    }
                });
            }
        });
        Self { address, seen }
    }

    /// Коды принятых пакетов; сессия, начатая клиентом, — после её `Q` (клиент не ждёт ответа на `Q`).
    async fn codes(&self) -> String {
        for _ in 0..100 {
            let codes = self.codes_now();
            if codes.is_empty() || codes.ends_with('Q') {
                return codes;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        self.codes_now()
    }

    fn codes_now(&self) -> String {
        self.seen
            .lock()
            .expect("lock")
            .iter()
            .map(|p| char::from(p.code))
            .collect()
    }

    fn packets(&self, code: u8) -> Vec<Packet> {
        self.seen
            .lock()
            .expect("lock")
            .iter()
            .filter(|p| p.code == code)
            .cloned()
            .collect()
    }

    fn filter(&self, name: &str) -> MilterFilter {
        MilterFilter {
            name: name.into(),
            address: self.address.clone(),
            command_timeout: Duration::from_secs(2),
            end_timeout: Duration::from_secs(2),
            ..MilterFilter::default()
        }
    }
}

fn reply_of(code: u8) -> Packet {
    Packet::new(code, Bytes::new())
}

/// Сценарий: `continue` на всё, на конец тела — `eob`.
fn on_eob(eob: Vec<Packet>) -> Script {
    Arc::new(move |p: &Packet| {
        if p.code == proto::C_BODYEOB {
            eob.clone()
        } else {
            vec![reply_of(proto::R_CONTINUE)]
        }
    })
}

fn header_mod(code: u8, index: Option<u32>, name: &str, value: &str) -> Packet {
    let mut b = BytesMut::new();
    if let Some(i) = index {
        b.put_u32(i);
    }
    b.put_slice(name.as_bytes());
    b.put_u8(0);
    b.put_slice(value.as_bytes());
    b.put_u8(0);
    Packet::new(code, b.freeze())
}

fn envelope(mode: Mode) -> Envelope {
    Envelope {
        id: "q1".into(),
        mode,
        conn: conn_info(true),
        helo: "client.example.net".into(),
        esmtp: true,
        principal: None,
        mail_from: "sender@example.net".into(),
        rcpts: vec!["bob@example.org".into(), "carol@example.org".into()],
        smtputf8: false,
        started_at: OffsetDateTime::now_utc(),
    }
}

const MAIL: &[u8] =
    b"From: sender@example.net\r\nSubject: hello\r\nX-Spam: old\r\n  folded\r\n\r\nbody\r\n";

fn chain(filters: Vec<MilterFilter>) -> Milters {
    Milters::new(MilterConfig { filters }, "mx.example.org")
}

#[tokio::test]
async fn full_session_with_header_changes() {
    let mock = Mock::start(
        0,
        on_eob(vec![
            reply_of(proto::R_PROGRESS),
            header_mod(proto::R_ADDHEADER, None, "X-Spam-Score", "1.5"),
            header_mod(proto::R_INSHEADER, Some(0), "X-Top", "first"),
            header_mod(proto::R_CHGHEADER, Some(1), "X-Spam", ""),
            header_mod(proto::R_CHGHEADER, Some(1), "Subject", "[SPAM] hello"),
            reply_of(proto::R_ACCEPT),
        ]),
    )
    .await;
    let m = chain(vec![mock.filter("rspamd")]);
    assert!(m.applies(Mode::Inbound));
    assert!(!m.applies(Mode::Submission));
    let out = m
        .check(&envelope(Mode::Inbound), Bytes::from_static(MAIL))
        .await;
    assert_eq!(out.verdict, Verdict::Accept);
    assert_eq!(out.quarantine, None);
    assert_eq!(
        out.message,
        Bytes::from_static(
            b"X-Top: first\r\nFrom: sender@example.net\r\nSubject: [SPAM] hello\r\nX-Spam-Score: 1.5\r\n\r\nbody\r\n"
        )
    );
    // Порядок команд: согласование, макросы и шаги сессии, два получателя, три заголовка, тело, конец, выход.
    assert_eq!(mock.codes().await, "ODCHDMDRDRDTLLLDNBDEQ");
    let connect = &mock.packets(proto::C_CONNECT)[0];
    assert_eq!(
        &connect.data[..],
        b"client.example.net\x004\x9c\x40192.0.2.10\0"
    );
    let mail = &mock.packets(proto::C_MAIL)[0];
    assert_eq!(mail.split(), [&b"<sender@example.net>"[..]]);
    let macros = mock.packets(proto::C_MACRO);
    assert_eq!(macros[0].data[0], proto::C_CONNECT);
    assert!(macros[1].data.windows(14).any(|w| w == b"\0{mail_addr}\0s"));
    // Значение заголовка — без ведущего пробела, свёртка — `\n`.
    let headers = mock.packets(proto::C_HEADER);
    assert_eq!(headers[2].split(), [&b"X-Spam"[..], b"old\n  folded"]);
    assert_eq!(&mock.packets(proto::C_BODY)[0].data[..], b"body\r\n");
}

#[tokio::test]
async fn skipped_steps_and_no_reply() {
    // Фильтр отказался от HELO и заголовков, на RCPT ответа не ждёт.
    let mock = Mock::start(
        proto::P_NOHELO | proto::P_NOHDRS | proto::P_NR_RCPT,
        Arc::new(|p: &Packet| match p.code {
            proto::C_RCPT => vec![],
            proto::C_BODYEOB => vec![reply_of(proto::R_CONTINUE)],
            _ => vec![reply_of(proto::R_CONTINUE)],
        }),
    )
    .await;
    let out = chain(vec![mock.filter("f")])
        .check(&envelope(Mode::Inbound), Bytes::from_static(MAIL))
        .await;
    assert_eq!(out.verdict, Verdict::Accept);
    assert_eq!(out.message, Bytes::from_static(MAIL));
    assert_eq!(mock.codes().await, "ODCDMDRDRDTDNBDEQ");
}

#[tokio::test]
async fn reject_tempfail_replycode_discard() {
    let cases = [
        (vec![reply_of(proto::R_REJECT)], Some((550, [5, 7, 1]))),
        (vec![reply_of(proto::R_TEMPFAIL)], Some((451, [4, 7, 1]))),
        (
            vec![Packet::strings(
                proto::R_REPLYCODE,
                &[b"554 5.7.0 Spam message rejected"],
            )],
            Some((554, [5, 7, 0])),
        ),
        (
            vec![Packet::strings(proto::R_REPLYCODE, &[b"452 try later"])],
            Some((452, [4, 7, 1])),
        ),
        (vec![reply_of(proto::R_DISCARD)], None),
    ];
    for (eob, expect) in cases {
        let mock = Mock::start(0, on_eob(eob)).await;
        let out = chain(vec![mock.filter("f")])
            .check(&envelope(Mode::Inbound), Bytes::from_static(MAIL))
            .await;
        match (out.verdict, expect) {
            (Verdict::Refuse(r), Some((code, esc))) => {
                assert_eq!((r.code, r.esc), (code, esc), "{}", r.message);
            }
            (Verdict::Discard, None) => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    let mock = Mock::start(0, on_eob(vec![])).await;
    // Отказ на шаге до конца тела — отказ всему письму, дальше команды не идут.
    let rcpt_reject = Mock::start(
        0,
        Arc::new(|p: &Packet| {
            if p.code == proto::C_RCPT {
                vec![reply_of(proto::R_REJECT)]
            } else {
                vec![reply_of(proto::R_CONTINUE)]
            }
        }),
    )
    .await;
    let out = chain(vec![rcpt_reject.filter("first"), mock.filter("second")])
        .check(&envelope(Mode::Inbound), Bytes::from_static(MAIL))
        .await;
    assert!(matches!(out.verdict, Verdict::Refuse(ref r) if r.code == 550));
    assert_eq!(rcpt_reject.codes().await, "ODCHDMDRQ");
    assert_eq!(mock.codes().await, "", "second filter must not run");
}

#[tokio::test]
async fn chain_passes_changes_and_quarantine() {
    let first = Mock::start(
        0,
        on_eob(vec![
            header_mod(proto::R_ADDHEADER, None, "X-First", "yes"),
            Packet::strings(proto::R_QUARANTINE, &[b"virus found"]),
            reply_of(proto::R_CONTINUE),
        ]),
    )
    .await;
    let second = Mock::start(0, on_eob(vec![reply_of(proto::R_ACCEPT)])).await;
    let mut f2 = second.filter("second");
    f2.submission = true;
    let out = chain(vec![first.filter("first"), f2.clone()])
        .check(&envelope(Mode::Inbound), Bytes::from_static(MAIL))
        .await;
    assert_eq!(out.verdict, Verdict::Accept);
    assert_eq!(out.quarantine.as_deref(), Some("virus found"));
    // Второй фильтр видит заголовок, добавленный первым.
    let names: Vec<_> = second
        .packets(proto::C_HEADER)
        .iter()
        .map(|p| String::from_utf8_lossy(p.split()[0]).into_owned())
        .collect();
    assert_eq!(names, ["From", "Subject", "X-Spam", "X-First"]);
    // Submission проверяет только фильтр с `submission: true`.
    let (first_before, second_before) = (first.codes().await.len(), second.codes().await.len());
    chain(vec![first.filter("first"), f2])
        .check(&envelope(Mode::Submission), Bytes::from_static(MAIL))
        .await;
    assert_eq!(first.codes().await.len(), first_before);
    assert!(second.codes().await.len() > second_before);
}

#[tokio::test]
async fn unavailable_filter_policy() {
    // Порт без слушателя.
    let closed = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = closed.local_addr().expect("addr").to_string();
    drop(closed);
    let mut f = MilterFilter {
        name: "av".into(),
        address,
        connect_timeout: Duration::from_secs(2),
        ..MilterFilter::default()
    };
    let out = chain(vec![f.clone()])
        .check(&envelope(Mode::Inbound), Bytes::from_static(MAIL))
        .await;
    assert_eq!(out.verdict, Verdict::Accept);
    assert!(
        out.message
            .starts_with(b"X-ThePost-Milter: av; unavailable\r\nFrom: ")
    );
    f.on_failure = MilterOnFailure::Tempfail;
    let out = chain(vec![f])
        .check(&envelope(Mode::Inbound), Bytes::from_static(MAIL))
        .await;
    assert!(matches!(out.verdict, Verdict::Refuse(ref r) if (r.code, r.esc) == (451, [4, 7, 1])));
}

#[tokio::test]
async fn timeout_is_failure() {
    // Фильтр молчит на конце тела.
    let mock = Mock::start(0, on_eob(vec![])).await;
    let mut f = mock.filter("slow");
    f.end_timeout = Duration::from_millis(200);
    f.on_failure = MilterOnFailure::Tempfail;
    let out = chain(vec![f])
        .check(&envelope(Mode::Inbound), Bytes::from_static(MAIL))
        .await;
    assert!(matches!(out.verdict, Verdict::Refuse(ref r) if r.code == 451));
}

#[test]
fn replycode_parsing() {
    let p = |s: &[u8]| proto::parse_replycode(&Packet::strings(proto::R_REPLYCODE, &[s]));
    assert_eq!(
        p(b"550 5.7.1 Gtube pattern"),
        Some((550, [5, 7, 1], "Gtube pattern".into()))
    );
    assert_eq!(p(b"421 4.7.0"), Some((421, [4, 7, 0], String::new())));
    assert_eq!(p(b"550 no"), Some((550, [5, 7, 1], "no".into())));
    assert_eq!(p(b"250 ok"), None);
    assert_eq!(p(b"x"), None);
}
