//! FETCH (IMAP-04): элементы, разделы, BINARY, частичная выборка, `\Seen`; снимки ответов на корпусе.

use tp_contracts::meta::{Flag, SpecialUse};

use super::{Client, Env};

const MIXED: &str = "From: =?utf-8?b?0JDQu9C40YHQsA==?= <alice@example.com>\r\n\
To: Bob <bob@example.org>, carol@example.org\r\n\
Subject: Report\r\n\
Date: Thu, 29 Oct 2026 10:34:00 +0300\r\n\
Message-ID: <m1@example.com>\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"b1\"\r\n\
\r\n\
--b1\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
Content-Transfer-Encoding: quoted-printable\r\n\
\r\n\
caf=C3=A9\r\n\
--b1\r\n\
Content-Type: application/octet-stream; name=\"z.bin\"\r\n\
Content-Transfer-Encoding: base64\r\n\
Content-Disposition: attachment; filename=\"z.bin\"\r\n\
\r\n\
AAECAw==\r\n\
--b1\r\n\
Content-Type: message/rfc822\r\n\
\r\n\
From: dave@example.net\r\n\
Subject: Inner\r\n\
\r\n\
Inner body\r\n\
--b1--\r\n";

async fn selected(raw: &[u8], flags: tp_contracts::meta::Flags) -> (Env, Client) {
    let env = Env::new().await;
    let inbox = env.mailbox(SpecialUse::Inbox);
    env.deliver(inbox, raw, flags).await;
    let mut c = env.logged_in().await;
    assert!(c.cmd("S1 SELECT INBOX").await.contains("S1 OK"));
    (env, c)
}

#[tokio::test]
async fn metadata_items() {
    let (_env, mut c) = selected(MIXED.as_bytes(), [Flag::Flagged].into()).await;
    let size = MIXED.len();
    assert_eq!(
        c.cmd("A1 FETCH 1 (UID FLAGS INTERNALDATE RFC822.SIZE)")
            .await,
        format!(
            "* 1 FETCH (UID 1 FLAGS (\\Flagged) INTERNALDATE \"21-Sep-2026 14:13:20 +0000\" RFC822.SIZE {size})\r\n\
             A1 OK FETCH completed\r\n"
        )
    );
    // IMAP4rev1: имя отправителя — encoded-word; Sender и Reply-To — как From.
    let r = c.cmd("A2 FETCH 1 ENVELOPE").await;
    assert_eq!(
        r,
        "* 1 FETCH (ENVELOPE (\"Thu, 29 Oct 2026 10:34:00 +0300\" \"Report\" \
         ((\"=?utf-8?B?0JDQu9C40YHQsA==?=\" NIL \"alice\" \"example.com\")) \
         ((\"=?utf-8?B?0JDQu9C40YHQsA==?=\" NIL \"alice\" \"example.com\")) \
         ((\"=?utf-8?B?0JDQu9C40YHQsA==?=\" NIL \"alice\" \"example.com\")) \
         ((\"Bob\" NIL \"bob\" \"example.org\")(NIL NIL \"carol\" \"example.org\")) \
         NIL NIL NIL \"<m1@example.com>\"))\r\n\
         A2 OK FETCH completed\r\n"
    );
    let r = c.cmd("A3 FETCH 1 BODYSTRUCTURE").await;
    assert_eq!(
        r,
        "* 1 FETCH (BODYSTRUCTURE (\
         (\"TEXT\" \"plain\" (\"charset\" \"utf-8\") NIL NIL \"quoted-printable\" 9 0 NIL NIL NIL NIL)\
         (\"application\" \"octet-stream\" (\"name\" \"z.bin\") NIL NIL \"base64\" 8 NIL \
         (\"attachment\" (\"filename\" \"z.bin\")) NIL NIL)\
         (\"MESSAGE\" \"RFC822\" NIL NIL NIL \"7bit\" 52 \
         (NIL \"Inner\" ((NIL NIL \"dave\" \"example.net\")) ((NIL NIL \"dave\" \"example.net\")) \
         ((NIL NIL \"dave\" \"example.net\")) NIL NIL NIL NIL NIL) \
         (\"TEXT\" \"plain\" NIL NIL NIL \"7bit\" 10 0 NIL NIL NIL NIL) 3 NIL NIL NIL NIL) \
         \"mixed\" (\"boundary\" \"b1\") NIL NIL NIL))\r\n\
         A3 OK FETCH completed\r\n"
    );
    let r = c.cmd("A4 FETCH 1 BODY").await;
    assert!(
        r.starts_with("* 1 FETCH (BODY ((\"TEXT\" \"plain\" (\"charset\" \"utf-8\") NIL NIL \"quoted-printable\" 9 0)"),
        "{r}"
    );
    // Макрос FAST, UID FETCH добавляет UID.
    let r = c.cmd("A5 UID FETCH 1:* FAST").await;
    assert!(
        r.starts_with("* 1 FETCH (UID 1 FLAGS (\\Flagged) INTERNALDATE"),
        "{r}"
    );
    assert!(r.ends_with("A5 OK UID FETCH completed\r\n"), "{r}");
    // Несуществующий номер — BAD, несуществующий UID — пусто.
    assert!(c.cmd("A6 FETCH 2 FLAGS").await.starts_with("A6 BAD"));
    assert_eq!(
        c.cmd("A7 UID FETCH 5 FLAGS").await,
        "A7 OK UID FETCH completed\r\n"
    );
    let r = c.cmd("A8 UID FETCH 5:* UID").await;
    assert_eq!(r, "* 1 FETCH (UID 1)\r\nA8 OK UID FETCH completed\r\n");
}

#[tokio::test]
async fn body_sections() {
    let (_env, mut c) = selected(MIXED.as_bytes(), Default::default()).await;
    let r = c
        .cmd("A1 FETCH 1 (BODY.PEEK[HEADER.FIELDS (SUBJECT message-id)])")
        .await;
    assert_eq!(
        r,
        "* 1 FETCH (BODY[HEADER.FIELDS (SUBJECT message-id)] {49}\r\n\
         Subject: Report\r\nMessage-ID: <m1@example.com>\r\n\r\n)\r\n\
         A1 OK FETCH completed\r\n"
    );
    let r = c.cmd("A2 FETCH 1 (BODY.PEEK[1] BODY.PEEK[1.MIME])").await;
    assert_eq!(
        r,
        "* 1 FETCH (BODY[1] {9}\r\ncaf=C3=A9 BODY[1.MIME] {88}\r\n\
         Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\n)\r\n\
         A2 OK FETCH completed\r\n"
    );
    // Вложенное письмо: заголовок, тело, часть.
    let r = c
        .cmd("A3 FETCH 1 (BODY.PEEK[3.HEADER] BODY.PEEK[3.TEXT] BODY.PEEK[3.1])")
        .await;
    assert_eq!(
        r,
        "* 1 FETCH (BODY[3.HEADER] {42}\r\nFrom: dave@example.net\r\nSubject: Inner\r\n\r\n \
         BODY[3.TEXT] {10}\r\nInner body BODY[3.1] {10}\r\nInner body)\r\n\
         A3 OK FETCH completed\r\n"
    );
    // Частичная выборка: ответ с началом диапазона.
    let r = c
        .cmd("A4 FETCH 1 (BODY.PEEK[]<0.4> BODY.PEEK[TEXT]<2.3>)")
        .await;
    assert_eq!(
        r,
        "* 1 FETCH (BODY[]<0> {4}\r\nFrom BODY[TEXT]<2> {3}\r\nb1\r)\r\n\
         A4 OK FETCH completed\r\n"
    );
    let r = c.cmd("A5 FETCH 1 (RFC822.HEADER BODY.PEEK[9])").await;
    assert!(r.starts_with("* 1 FETCH (RFC822.HEADER {"), "{r}");
    assert!(
        r.contains("Content-Type: multipart/mixed; boundary=\"b1\"\r\n\r\n BODY[9] \"\")"),
        "{r}"
    );
}

#[tokio::test]
async fn binary_and_seen() {
    let (_env, mut c) = selected(MIXED.as_bytes(), Default::default()).await;
    // RFC 3516: base64 и quoted-printable сняты, NUL — литерал8.
    let r = c
        .cmd("A1 FETCH 1 (BINARY.PEEK[1] BINARY.SIZE[2] BINARY.PEEK[2])")
        .await;
    assert_eq!(
        r,
        "* 1 FETCH (BINARY[1] {5}\r\ncafé BINARY.SIZE[2] 4 BINARY[2] ~{4}\r\n\u{0}\u{1}\u{2}\u{3})\r\n\
         A1 OK FETCH completed\r\n"
    );
    // Диапазон: в ответе — начало (RFC 3516, 4.3: `BINARY[1]<1>`).
    let r = c
        .cmd("A11 FETCH 1 (BINARY.PEEK[2]<1.2> BINARY.PEEK[1]<3.10> UID)")
        .await;
    assert_eq!(
        r,
        "* 1 FETCH (BINARY[2]<1> {2}\r\n\u{1}\u{2} BINARY[1]<3> {2}\r\né UID 1)\r\n\
         A11 OK FETCH completed\r\n"
    );
    // Без .PEEK — \Seen и FLAGS в ответе; повторно флаг не меняется.
    let r = c.cmd("A2 FETCH 1 BODY[1]").await;
    assert_eq!(
        r,
        "* 1 FETCH (BODY[1] {9}\r\ncaf=C3=A9 FLAGS (\\Seen))\r\nA2 OK FETCH completed\r\n"
    );
    let r = c.cmd("A3 FETCH 1 (FLAGS RFC822.TEXT)").await;
    assert!(
        r.starts_with("* 1 FETCH (FLAGS (\\Seen) RFC822.TEXT {"),
        "{r}"
    );

    // EXAMINE: только чтение, \Seen не ставится.
    let (_env, mut c) = selected(MIXED.as_bytes(), Default::default()).await;
    assert!(c.cmd("B1 EXAMINE INBOX").await.contains("B1 OK"));
    let r = c.cmd("B2 FETCH 1 BODY[1]").await;
    assert!(!r.contains("FLAGS"), "{r}");
    assert_eq!(
        c.cmd("B3 FETCH 1 FLAGS").await,
        "* 1 FETCH (FLAGS ())\r\nB3 OK FETCH completed\r\n"
    );

    // Неизвестная кодировка передачи.
    let raw = "Subject: x\r\nContent-Transfer-Encoding: x-uuencode\r\n\r\nbody\r\n";
    let (_env, mut c) = selected(raw.as_bytes(), Default::default()).await;
    let r = c.cmd("C1 FETCH 1 BINARY.PEEK[1]").await;
    assert_eq!(
        r,
        "C1 NO [UNKNOWN-CTE] Unknown Content-Transfer-Encoding\r\n"
    );
}

#[tokio::test]
async fn rev2_utf8_and_state() {
    let env = Env::new().await;
    let mut c = env.logged_in().await;
    assert!(c.cmd("A1 FETCH 1 FLAGS").await.starts_with("A1 BAD"));
    let inbox = env.mailbox(SpecialUse::Inbox);
    env.deliver(inbox, MIXED.as_bytes(), Default::default())
        .await;
    assert!(c.cmd("A2 ENABLE IMAP4rev2").await.contains("A2 OK"));
    assert!(c.cmd("A3 SELECT INBOX").await.contains("A3 OK"));
    let r = c.cmd("A4 FETCH 1 ENVELOPE").await;
    assert!(r.contains("(({10}\r\nАлиса NIL \"alice\""), "{r}");
}

#[tokio::test]
async fn corpus_snapshots() {
    let env = Env::new().await;
    let inbox = env.mailbox(SpecialUse::Inbox);
    let corpus = tp_testkit::corpus().expect("corpus");
    for mail in &corpus {
        env.deliver(inbox, &mail.raw, Default::default()).await;
    }
    let mut c = env.logged_in().await;
    assert!(c.cmd("S1 EXAMINE INBOX").await.contains("S1 OK"));
    for (i, mail) in corpus.iter().enumerate() {
        let n = i + 1;
        let r = c
            .cmd(&format!(
                "F{n} FETCH {n} (ENVELOPE BODYSTRUCTURE BODY.PEEK[HEADER.FIELDS (FROM SUBJECT CONTENT-TYPE)] BINARY.SIZE[1])"
            ))
            .await;
        // Неизвестная кодировка в битом письме — допустимый ответ.
        assert!(
            r.ends_with(&format!("F{n} OK FETCH completed\r\n")) || mail.is_broken(),
            "{}: {r}",
            mail.name
        );
        insta::assert_snapshot!(mail.name.clone(), r);
    }
}
