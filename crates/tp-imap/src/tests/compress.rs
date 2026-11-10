//! COMPRESS=DEFLATE, UNSELECT, LITERAL+ и BINARY вместе (IMAP-11): RFC 4978, RFC 3691, RFC 7888, RFC 3516.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::{Client, Env};
use crate::compress::Deflate;

/// Сеанс после `COMPRESS DEFLATE`: команды сжимаются, ответы распаковываются.
struct Zipped {
    c: Client,
    z: Deflate,
    text: Vec<u8>,
}

impl Zipped {
    async fn new(mut c: Client) -> Self {
        assert_eq!(
            c.cmd("Z1 COMPRESS DEFLATE").await,
            "Z1 OK DEFLATE active\r\n"
        );
        Self {
            c,
            z: Deflate::new(6),
            text: Vec::new(),
        }
    }

    async fn send(&mut self, data: &[u8]) {
        let packed = self.z.compress(data).expect("compress");
        // Сжатые данные — не открытый текст.
        assert!(data.len() < 16 || packed != data);
        let io = self.c.io.get_mut();
        io.write_all(&packed).await.expect("write");
    }

    /// Ответы до строки с тегом `tag` включительно.
    async fn until(&mut self, tag: &str) -> String {
        let mark = format!("{tag} ");
        loop {
            let text = String::from_utf8_lossy(&self.text).into_owned();
            let done = text
                .split_inclusive("\r\n")
                .any(|l| l.starts_with(&mark) && l.ends_with("\r\n"));
            if done {
                self.text.clear();
                return text;
            }
            let mut buf = [0u8; 4096];
            let n = tokio::time::timeout(Duration::from_secs(5), self.c.io.read(&mut buf))
                .await
                .expect("timeout")
                .expect("read");
            assert!(n > 0, "connection closed: {text}");
            let text = &mut self.text;
            self.z
                .decompress(&buf[..n], |chunk| text.extend_from_slice(chunk))
                .expect("inflate");
        }
    }

    async fn cmd(&mut self, line: &str) -> String {
        let tag = line.split(' ').next().unwrap_or_default().to_owned();
        self.send(format!("{line}\r\n").as_bytes()).await;
        self.until(&tag).await
    }
}

const MIME: &str = "From: a@example.org\r\nSubject: bin\r\nMIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=b\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nHello\r\n\
--b\r\nContent-Type: application/octet-stream\r\nContent-Transfer-Encoding: base64\r\n\r\n\
AAECAwQ=\r\n--b--\r\n";

#[tokio::test]
async fn compressed_session_with_literal_plus_and_binary() {
    let env = Env::new().await;
    let mut c = env.logged_in().await;
    assert!(c.cmd("C1 CAPABILITY").await.contains(" COMPRESS=DEFLATE"));
    let mut z = Zipped::new(c).await;
    // LITERAL+: литерал без ожидания `+`, в сжатом потоке.
    let r = z
        .cmd(&format!("A1 APPEND INBOX {{{}+}}\r\n{MIME}", MIME.len()))
        .await;
    assert!(r.contains("A1 OK [APPENDUID "), "{r}");
    // literal8 (BINARY) вместе с LITERAL+: двоичные данные без перекодирования.
    let raw = "From: a@example.org\r\nSubject: raw\r\nContent-Transfer-Encoding: binary\r\n\r\nx\u{1}y\r\n";
    let r = z
        .cmd(&format!("A2 APPEND INBOX ~{{{}+}}\r\n{raw}", raw.len()))
        .await;
    assert!(r.contains("A2 OK"), "{r}");
    let r = z.cmd("S1 SELECT INBOX").await;
    assert!(r.contains("* 2 EXISTS"), "{r}");
    // BINARY: часть base64 — декодированные байты.
    let r = z.cmd("F1 FETCH 1 (BINARY[2] BINARY.SIZE[2])").await;
    assert!(r.contains("BINARY.SIZE[2] 5"), "{r}");
    assert!(
        r.contains("BINARY[2] ~{5}\r\n\u{0}\u{1}\u{2}\u{3}\u{4}"),
        "{r}"
    );
    let r = z.cmd("F2 FETCH 2 BINARY.PEEK[1]").await;
    assert!(r.contains("x\u{1}y"), "{r}");
    // Большой ответ — несколько сжатых блоков.
    let r = z.cmd("F3 FETCH 1:2 BODY.PEEK[]").await;
    assert!(r.contains("AAECAwQ=") && r.contains("F3 OK"), "{r}");
    // Повторный COMPRESS — COMPRESSIONACTIVE.
    let r = z.cmd("Z2 COMPRESS DEFLATE").await;
    assert!(r.contains("Z2 NO [COMPRESSIONACTIVE]"), "{r}");
    // UNSELECT закрывает папку без EXPUNGE.
    assert!(z.cmd("U1 UNSELECT").await.contains("U1 OK"));
    assert!(z.cmd("U2 UNSELECT").await.contains("U2 BAD"));
    assert!(z.cmd("U3 FETCH 1 FLAGS").await.contains("U3 BAD"));
    let r = z.cmd("L1 LOGOUT").await;
    assert!(r.starts_with("* BYE"), "{r}");
}

#[tokio::test]
async fn compress_requires_login_and_config() {
    let mut env = Env::new().await;
    let mut c = env.client(true).await;
    assert!(c.cmd("Z1 COMPRESS DEFLATE").await.contains("Z1 BAD"));
    env.config.compress.enabled = false;
    let mut c = env.logged_in().await;
    assert!(!c.cmd("C1 CAPABILITY").await.contains("COMPRESS"));
    assert!(c.cmd("Z2 COMPRESS DEFLATE").await.contains("Z2 BAD"));
}

#[tokio::test]
async fn broken_stream_closes_session() {
    let env = Env::new().await;
    let c = env.logged_in().await;
    let mut z = Zipped::new(c).await;
    let io = z.c.io.get_mut();
    io.write_all(b"\xff\xfe\xfd not deflate")
        .await
        .expect("write");
    assert_eq!(z.c.rest().await, "");
}
