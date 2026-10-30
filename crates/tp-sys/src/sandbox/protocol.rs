//! Обмен с подпроцессом через stdin/stdout.
//!
//! - подпроцесс после установки ограничений пишет строку [`HANDSHAKE`]; всё до неё родитель пропускает;
//! - запрос: `u32 LE` длина + данные;
//! - ответ: `u8` статус (0 — успех, 1 — ошибка обработчика, данные — текст), `u8` флаги
//!   ([`FLAG_RETIRE`] — подпроцесс завершается после ответа), `u32 LE` длина + данные.

use std::io::{self, Read, Write};

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub(crate) const HANDSHAKE: &[u8] = b"TP-SANDBOX-READY/1\n";
/// Предел мусора до строки готовности (вывод тестового харнесса и т. п.), байт.
const HANDSHAKE_SCAN_LIMIT: usize = 64 * 1024;

pub(crate) const STATUS_OK: u8 = 0;
pub(crate) const STATUS_ERR: u8 = 1;
pub(crate) const FLAG_RETIRE: u8 = 1;

/// Ответ подпроцесса.
#[derive(Debug)]
pub(crate) struct Response {
    pub result: Result<Vec<u8>, String>,
    pub retire: bool,
}

// --- сторона подпроцесса (синхронно) ---

/// Читает запрос; `None` — родитель закрыл stdin.
pub(crate) fn read_request(r: &mut impl Read, max_len: u64) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len);
    if u64::from(len) > max_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "request too large",
        ));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    Ok(Some(buf))
}

pub(crate) fn write_response(
    w: &mut impl Write,
    result: &Result<Vec<u8>, String>,
    retire: bool,
) -> io::Result<()> {
    let (status, data) = match result {
        Ok(data) => (STATUS_OK, data.as_slice()),
        Err(msg) => (STATUS_ERR, msg.as_bytes()),
    };
    let len = u32::try_from(data.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "response too large"))?;
    let flags = if retire { FLAG_RETIRE } else { 0 };
    w.write_all(&[status, flags])?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(data)?;
    w.flush()
}

// --- сторона родителя (async) ---

/// Ждёт строку готовности, пропуская предшествующий вывод.
pub(crate) async fn read_handshake(r: &mut (impl AsyncBufRead + Unpin)) -> io::Result<()> {
    let mut seen = 0usize;
    let mut line = Vec::new();
    loop {
        line.clear();
        let n = r.read_until(b'\n', &mut line).await?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        if line.ends_with(HANDSHAKE) {
            return Ok(());
        }
        seen += n;
        if seen > HANDSHAKE_SCAN_LIMIT {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "no handshake"));
        }
    }
}

pub(crate) async fn write_request(
    w: &mut (impl AsyncWrite + Unpin),
    data: &[u8],
) -> io::Result<()> {
    let len = u32::try_from(data.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "request too large"))?;
    w.write_all(&len.to_le_bytes()).await?;
    w.write_all(data).await?;
    w.flush().await
}

/// Читает ответ; длина больше `max_len` — `InvalidData`.
pub(crate) async fn read_response(
    r: &mut (impl AsyncBufRead + Unpin),
    max_len: u64,
) -> io::Result<Response> {
    let mut head = [0u8; 6];
    r.read_exact(&mut head).await?;
    let [status, flags, l0, l1, l2, l3] = head;
    let len = u32::from_le_bytes([l0, l1, l2, l3]);
    if u64::from(len) > max_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "response too large",
        ));
    }
    let mut data = vec![0u8; len as usize];
    r.read_exact(&mut data).await?;
    let result = match status {
        STATUS_OK => Ok(data),
        STATUS_ERR => Err(String::from_utf8_lossy(&data).into_owned()),
        _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "bad status")),
    };
    Ok(Response {
        result,
        retire: flags & FLAG_RETIRE != 0,
    })
}
