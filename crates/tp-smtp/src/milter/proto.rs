//! Пакеты протокола Milter v6 (сторона MTA): длина (u32, big-endian, вместе с кодом), код команды, данные.
//!
//! Описание протокола — `libmilter/docs` Sendmail и `include/libmilter/mfdef.h`; здесь — только то, что
//! нужно клиенту: команды MTA → фильтр и ответы фильтр → MTA.

use bytes::{Buf, BufMut, Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Версия протокола.
pub const VERSION: u32 = 6;

/// Наибольший размер пакета ответа фильтра (защита от мусора в потоке).
pub const MAX_PACKET: usize = 1024 * 1024;

/// Наибольший кусок тела в одной команде `B` (`MILTER_CHUNK_SIZE`).
pub const BODY_CHUNK: usize = 65535;

// Команды MTA (SMFIC_*).
pub const C_ABORT: u8 = b'A';
pub const C_BODY: u8 = b'B';
pub const C_CONNECT: u8 = b'C';
pub const C_MACRO: u8 = b'D';
pub const C_BODYEOB: u8 = b'E';
pub const C_HELO: u8 = b'H';
pub const C_HEADER: u8 = b'L';
pub const C_MAIL: u8 = b'M';
pub const C_EOH: u8 = b'N';
pub const C_OPTNEG: u8 = b'O';
pub const C_QUIT: u8 = b'Q';
pub const C_RCPT: u8 = b'R';
pub const C_DATA: u8 = b'T';

// Ответы фильтра (SMFIR_*).
pub const R_ADDRCPT: u8 = b'+';
pub const R_DELRCPT: u8 = b'-';
pub const R_ACCEPT: u8 = b'a';
pub const R_REPLBODY: u8 = b'b';
pub const R_CONTINUE: u8 = b'c';
pub const R_DISCARD: u8 = b'd';
pub const R_ADDHEADER: u8 = b'h';
pub const R_INSHEADER: u8 = b'i';
pub const R_CHGHEADER: u8 = b'm';
pub const R_PROGRESS: u8 = b'p';
pub const R_QUARANTINE: u8 = b'q';
pub const R_REJECT: u8 = b'r';
pub const R_SKIP: u8 = b's';
pub const R_TEMPFAIL: u8 = b't';
pub const R_REPLYCODE: u8 = b'y';

// Действия, которые MTA разрешает фильтру (SMFIF_*).
pub const F_ADDHDRS: u32 = 0x01;
pub const F_CHGHDRS: u32 = 0x10;
pub const F_QUARANTINE: u32 = 0x20;
/// Предлагаемые действия: добавление и изменение заголовков, карантин.
pub const ACTIONS: u32 = F_ADDHDRS | F_CHGHDRS | F_QUARANTINE;

// Шаги протокола (SMFIP_*): `NO*` — не отправлять команду, `NR_*` — не ждать ответа.
pub const P_NOCONNECT: u32 = 0x01;
pub const P_NOHELO: u32 = 0x02;
pub const P_NOMAIL: u32 = 0x04;
pub const P_NORCPT: u32 = 0x08;
pub const P_NOBODY: u32 = 0x10;
pub const P_NOHDRS: u32 = 0x20;
pub const P_NOEOH: u32 = 0x40;
pub const P_NR_HDR: u32 = 0x80;
pub const P_NODATA: u32 = 0x200;
pub const P_SKIP: u32 = 0x400;
pub const P_RCPT_REJ: u32 = 0x800;
pub const P_NR_CONN: u32 = 0x1000;
pub const P_NR_HELO: u32 = 0x2000;
pub const P_NR_MAIL: u32 = 0x4000;
pub const P_NR_RCPT: u32 = 0x8000;
pub const P_NR_DATA: u32 = 0x10000;
pub const P_NR_EOH: u32 = 0x40000;
pub const P_NR_BODY: u32 = 0x80000;
pub const P_HDR_LEADSPC: u32 = 0x100000;
/// Все шаги, которые MTA умеет пропускать (кроме `RCPT_REJ`: отклонённые получатели фильтру не передаются).
pub const PROTOCOL: u32 = 0x001F_FFFF & !P_RCPT_REJ;

/// Ошибка обмена с фильтром.
#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    /// Ввод-вывод.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// Нарушение протокола.
    #[error("protocol: {0}")]
    Protocol(String),
    /// Фильтр не ответил вовремя.
    #[error("timeout")]
    Timeout,
}

/// Пакет: код и данные.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    /// Код команды или ответа.
    pub code: u8,
    /// Данные.
    pub data: Bytes,
}

impl Packet {
    /// Пакет с кодом `code` и данными `data`.
    pub fn new(code: u8, data: impl Into<Bytes>) -> Self {
        Self {
            code,
            data: data.into(),
        }
    }

    /// Данные — строки, завершённые `\0`.
    pub fn strings(code: u8, parts: &[&[u8]]) -> Self {
        let mut b = BytesMut::new();
        for p in parts {
            b.put_slice(p);
            b.put_u8(0);
        }
        Self::new(code, b.freeze())
    }

    /// Строки данных, разделённые `\0` (завершающий `\0` не обязателен).
    pub fn split(&self) -> Vec<&[u8]> {
        let data = self.data.strip_suffix(b"\0").unwrap_or(&self.data);
        data.split(|b| *b == 0).collect()
    }
}

/// Согласованные параметры: версия, действия, шаги.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OptNeg {
    /// Версия.
    pub version: u32,
    /// Действия (SMFIF_*).
    pub actions: u32,
    /// Шаги (SMFIP_*).
    pub protocol: u32,
}

impl OptNeg {
    /// Пакет `O`.
    pub fn packet(&self) -> Packet {
        let mut b = BytesMut::with_capacity(12);
        b.put_u32(self.version);
        b.put_u32(self.actions);
        b.put_u32(self.protocol);
        Packet::new(C_OPTNEG, b.freeze())
    }

    /// Разбор ответа `O` (после трёх чисел версии 6 могут идти списки макросов — пропускаются).
    pub fn parse(p: &Packet) -> Result<Self, ProtoError> {
        if p.code != C_OPTNEG || p.data.len() < 12 {
            return Err(ProtoError::Protocol(format!(
                "expected option negotiation, got {:?}",
                char::from(p.code)
            )));
        }
        let mut d = p.data.clone();
        Ok(Self {
            version: d.get_u32(),
            actions: d.get_u32(),
            protocol: d.get_u32(),
        })
    }
}

/// Записывает пакет.
pub async fn write<W: AsyncWrite + Unpin>(w: &mut W, p: &Packet) -> std::io::Result<()> {
    let len = u32::try_from(p.data.len() + 1)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "milter packet"))?;
    let mut b = BytesMut::with_capacity(5 + p.data.len());
    b.put_u32(len);
    b.put_u8(p.code);
    b.put_slice(&p.data);
    w.write_all(&b).await?;
    w.flush().await
}

/// Читает пакет.
pub async fn read<R: AsyncRead + Unpin>(r: &mut R) -> Result<Packet, ProtoError> {
    let len = r.read_u32().await? as usize;
    if len == 0 || len > MAX_PACKET {
        return Err(ProtoError::Protocol(format!("bad packet length {len}")));
    }
    let mut buf = vec![0; len];
    r.read_exact(&mut buf).await?;
    let mut data = Bytes::from(buf);
    let code = data.get_u8();
    Ok(Packet { code, data })
}

/// Ответ SMTP из пакета `y`: `"550 5.7.1 текст"`. Неверный код — `None`.
pub fn parse_replycode(p: &Packet) -> Option<(u16, [u8; 3], String)> {
    let text = String::from_utf8_lossy(p.split().first().copied().unwrap_or_default()).into_owned();
    // Многострочный ответ — первая строка.
    let line = text.lines().next().unwrap_or_default();
    let (code, rest) = line.split_at_checked(3)?;
    let code: u16 = code.parse().ok()?;
    if !(400..600).contains(&code) {
        return None;
    }
    let rest = rest.trim_start_matches(['-', ' ']);
    let (esc, msg) = match rest.split_once(' ') {
        Some((esc, msg)) => match parse_esc(esc) {
            Some(e) => (e, msg),
            None => (default_esc(code), rest),
        },
        None => match parse_esc(rest) {
            Some(e) => (e, ""),
            None => (default_esc(code), rest),
        },
    };
    Some((code, esc, msg.to_owned()))
}

fn parse_esc(s: &str) -> Option<[u8; 3]> {
    let mut it = s.split('.').map(|p| p.parse::<u8>().ok());
    let esc = [it.next()??, it.next()??, it.next()??];
    it.next().is_none().then_some(esc)
}

fn default_esc(code: u16) -> [u8; 3] {
    if code >= 500 { [5, 7, 1] } else { [4, 7, 1] }
}

/// Число из начала данных (индекс в `i`, `m`).
pub fn take_u32(data: &mut Bytes) -> Option<u32> {
    (data.len() >= 4).then(|| data.get_u32())
}
