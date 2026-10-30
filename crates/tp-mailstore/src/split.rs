//! Модель «скелет + части» (CONTRACTS.md#MailStore, SRS 3.3).
//!
//! Письмо разбирается `mail-parser` на тела листовых частей MIME (текст, вложения) и скелет — всё остальное:
//! заголовки, границы MIME, преамбулы, а также таблица вырезанных частей и разделов (путь IMAP → диапазон тела).
//! Скелет хранит байты письма без тел частей, поэтому сборка обратно побайтова при любом разборе, в том
//! числе битого MIME: вырезаются только проверенные непересекающиеся диапазоны исходных байтов.
//!
//! Тело части — байты в кодировке передачи, как в письме (base64, quoted-printable, 8bit); объект части
//! адресуется SHA-256 этих байтов и дедуплицируется в пределах организации.
//!
//! Перед письмом скелет может содержать заголовки получателя (`Return-Path`, `Delivered-To`, результат
//! Sieve и точек принятия решений): у разных получателей скелеты разные, части — общие.
//!
//! # Формат объекта-скелета (версия 1)
//!
//! Целые — little-endian; строка — `u16` длина + UTF-8.
//!
//! ```text
//! "TPSK" 0x01
//! u64 size                         — размер собранного письма (с заголовками получателя)
//! u32 n; n × { u64 at, u64 len, str path }      — вырезанные части по возрастанию `at`
//! u32 m; m × { str path, u64 start, u64 end }   — разделы: тело части по пути IMAP
//! остальное — байты письма без вырезанных частей, по порядку
//! ```
//!
//! Скелет ссылается на части по пути IMAP; ключ объекта (с поколением) — в метаданных письма
//! (`PartRef`), потому что поколение выясняется только при загрузке.

use std::ops::Range;

use bytes::{BufMut, Bytes, BytesMut};
use mail_parser::{Encoding, Message, MessageParser, PartType};
use sha2::{Digest, Sha256};
use tp_contracts::{BlobHash, Error, Result};

/// Заголовок формата скелета.
const MAGIC: &[u8; 5] = b"TPSK\x01";

/// Предел вложенности MIME при разборе: глубже части остаются в скелете.
const MAX_DEPTH: usize = 64;

/// Вырезанная часть письма: тело листовой части MIME.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cut {
    /// Путь части IMAP (`"1.2"`).
    pub path: String,
    /// Диапазон тела в исходном письме.
    pub range: Range<u64>,
    /// SHA-256 тела.
    pub hash: BlobHash,
}

/// Раздел письма: тело части MIME (листовой, составной или вложенного письма) по пути IMAP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    /// Путь IMAP; `""` — тело письма целиком (после заголовков).
    pub path: String,
    /// Диапазон тела в письме.
    pub range: Range<u64>,
}

/// Разбор письма: вырезаемые части и разделы.
#[derive(Debug, Clone)]
pub struct Split {
    raw: Bytes,
    cuts: Vec<Cut>,
    sections: Vec<Section>,
}

impl Split {
    /// Разбирает письмо. Не падает на битом MIME: что не разобралось, остаётся в скелете.
    pub fn new(raw: Bytes) -> Self {
        let mut walk = Walk {
            len: raw.len() as u64,
            cuts: Vec::new(),
            sections: Vec::new(),
        };
        if let Some(msg) = MessageParser::default().parse(raw.as_ref())
            && let Some(root) = msg.parts.first()
        {
            walk.section("", root.offset_body, root.offset_end);
            walk.message(&msg, "", 0);
        }
        let Walk { cuts, sections, .. } = walk;
        let cuts = normalize(cuts)
            .into_iter()
            .map(|(path, range)| {
                let hash = hash(slice(&raw, &range));
                Cut { path, range, hash }
            })
            .collect();
        Split {
            raw,
            cuts,
            sections,
        }
    }

    /// Исходное письмо.
    pub fn raw(&self) -> &Bytes {
        &self.raw
    }

    /// Вырезанные части по возрастанию смещения.
    pub fn cuts(&self) -> &[Cut] {
        &self.cuts
    }

    /// Разделы письма.
    pub fn sections(&self) -> &[Section] {
        &self.sections
    }

    /// Тело вырезанной части (срез исходного буфера, без копирования).
    pub fn body(&self, cut: &Cut) -> Bytes {
        self.raw
            .slice(cut.range.start as usize..cut.range.end as usize)
    }

    /// Объект-скелет с заголовками получателя `prefix` перед письмом.
    pub fn skeleton(&self, prefix: &[u8]) -> Bytes {
        let shift = prefix.len() as u64;
        let cut_len: u64 = self.cuts.iter().map(|c| c.range.end - c.range.start).sum();
        let size = shift + self.raw.len() as u64;
        let mut out = BytesMut::with_capacity(
            64 + prefix.len() + self.raw.len() - cut_len as usize + 48 * self.sections.len(),
        );
        out.put_slice(MAGIC);
        out.put_u64_le(size);
        out.put_u32_le(len_u32(self.cuts.len()));
        for c in &self.cuts {
            out.put_u64_le(c.range.start + shift);
            out.put_u64_le(c.range.end - c.range.start);
            put_str(&mut out, &c.path);
        }
        out.put_u32_le(len_u32(self.sections.len()));
        for s in &self.sections {
            put_str(&mut out, &s.path);
            out.put_u64_le(s.range.start + shift);
            out.put_u64_le(s.range.end + shift);
        }
        out.put_slice(prefix);
        let mut pos = 0usize;
        for c in &self.cuts {
            out.put_slice(&self.raw[pos..c.range.start as usize]);
            pos = c.range.end as usize;
        }
        out.put_slice(&self.raw[pos..]);
        out.freeze()
    }
}

/// Сегмент собранного письма.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    /// Байты скелета.
    Inline {
        /// Смещение сегмента в письме.
        at: u64,
        /// Байты.
        data: Bytes,
    },
    /// Тело вырезанной части.
    Part {
        /// Смещение сегмента в письме.
        at: u64,
        /// Длина.
        len: u64,
        /// Путь части IMAP (ключ объекта — в `PartRef` метаданных).
        path: String,
    },
}

impl Segment {
    /// Диапазон сегмента в письме.
    pub fn range(&self) -> Range<u64> {
        match self {
            Segment::Inline { at, data } => *at..*at + data.len() as u64,
            Segment::Part { at, len, .. } => *at..*at + *len,
        }
    }
}

/// Разобранный объект-скелет.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skeleton {
    size: u64,
    segments: Vec<Segment>,
    sections: Vec<Section>,
}

impl Skeleton {
    /// Разбирает объект-скелет; битый формат — `Error::Internal`.
    pub fn decode(data: Bytes) -> Result<Skeleton> {
        decode(data).map_err(|e| Error::internal(format!("skeleton: {e}")))
    }

    /// Размер собранного письма.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Сегменты письма по порядку, без пропусков от 0 до `size`.
    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    /// Разделы письма.
    pub fn sections(&self) -> &[Section] {
        &self.sections
    }

    /// Диапазон тела части по пути IMAP.
    pub fn section(&self, path: &str) -> Option<Range<u64>> {
        self.sections
            .iter()
            .find(|s| s.path == path)
            .map(|s| s.range.clone())
    }

    /// Собирает письмо в памяти (тесты, малые письма); `part` — тело части по пути.
    pub fn join(&self, mut part: impl FnMut(&str) -> Option<Bytes>) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(usize::try_from(self.size).unwrap_or(0));
        for seg in &self.segments {
            match seg {
                Segment::Inline { data, .. } => out.extend_from_slice(data),
                Segment::Part { len, path, .. } => {
                    let body = part(path).ok_or(Error::NotFound)?;
                    if body.len() as u64 != *len {
                        return Err(Error::internal(format!("part {path}: size mismatch")));
                    }
                    out.extend_from_slice(&body);
                }
            }
        }
        Ok(out)
    }
}

/// Ошибка разбора скелета.
#[derive(Debug, thiserror::Error)]
enum DecodeError {
    #[error("bad magic")]
    Magic,
    #[error("truncated")]
    Truncated,
    #[error("bad utf-8 path")]
    Utf8,
    #[error("bad layout")]
    Layout,
}

/// Читатель полей скелета.
struct Reader {
    data: Bytes,
    pos: usize,
}

impl Reader {
    fn take(&mut self, n: usize) -> std::result::Result<&[u8], DecodeError> {
        let end = self.pos.checked_add(n).ok_or(DecodeError::Truncated)?;
        let s = self.data.get(self.pos..end).ok_or(DecodeError::Truncated)?;
        self.pos = end;
        Ok(s)
    }

    fn u16(&mut self) -> std::result::Result<u16, DecodeError> {
        let mut b = [0u8; 2];
        b.copy_from_slice(self.take(2)?);
        Ok(u16::from_le_bytes(b))
    }

    fn u32(&mut self) -> std::result::Result<u32, DecodeError> {
        let mut b = [0u8; 4];
        b.copy_from_slice(self.take(4)?);
        Ok(u32::from_le_bytes(b))
    }

    fn u64(&mut self) -> std::result::Result<u64, DecodeError> {
        let mut b = [0u8; 8];
        b.copy_from_slice(self.take(8)?);
        Ok(u64::from_le_bytes(b))
    }

    fn str(&mut self) -> std::result::Result<String, DecodeError> {
        let n = usize::from(self.u16()?);
        let s = self.take(n)?;
        String::from_utf8(s.to_vec()).map_err(|_| DecodeError::Utf8)
    }
}

fn decode(data: Bytes) -> std::result::Result<Skeleton, DecodeError> {
    let mut r = Reader { data, pos: 0 };
    if r.take(MAGIC.len())? != MAGIC {
        return Err(DecodeError::Magic);
    }
    let size = r.u64()?;
    let n = r.u32()?;
    let mut cuts = Vec::new();
    for _ in 0..n {
        let at = r.u64()?;
        let len = r.u64()?;
        let path = r.str()?;
        cuts.push((at, len, path));
    }
    let m = r.u32()?;
    let mut sections = Vec::new();
    for _ in 0..m {
        let path = r.str()?;
        let start = r.u64()?;
        let end = r.u64()?;
        if start > end || end > size {
            return Err(DecodeError::Layout);
        }
        sections.push(Section {
            path,
            range: start..end,
        });
    }
    let inline = r.data.slice(r.pos..);
    let mut segments = Vec::with_capacity(cuts.len() * 2 + 1);
    let (mut at, mut ipos) = (0u64, 0usize);
    let push_inline = |segments: &mut Vec<Segment>, at: u64, len: u64, ipos: &mut usize| {
        let len = usize::try_from(len).map_err(|_| DecodeError::Layout)?;
        let end = ipos.checked_add(len).ok_or(DecodeError::Layout)?;
        if end > inline.len() {
            return Err(DecodeError::Layout);
        }
        if len > 0 {
            segments.push(Segment::Inline {
                at,
                data: inline.slice(*ipos..end),
            });
        }
        *ipos = end;
        Ok(())
    };
    for (cut_at, len, path) in cuts {
        if cut_at < at || len == 0 {
            return Err(DecodeError::Layout);
        }
        push_inline(&mut segments, at, cut_at - at, &mut ipos)?;
        segments.push(Segment::Part {
            at: cut_at,
            len,
            path,
        });
        at = cut_at.checked_add(len).ok_or(DecodeError::Layout)?;
    }
    if at > size {
        return Err(DecodeError::Layout);
    }
    push_inline(&mut segments, at, size - at, &mut ipos)?;
    if ipos != inline.len() {
        return Err(DecodeError::Layout);
    }
    Ok(Skeleton {
        size,
        segments,
        sections,
    })
}

/// Обход дерева MIME: пути IMAP, разделы, кандидаты на вырезание.
struct Walk {
    len: u64,
    cuts: Vec<(String, Range<u64>)>,
    sections: Vec<Section>,
}

impl Walk {
    /// Диапазон тела, если он внутри письма.
    fn range(&self, start: u32, end: u32) -> Option<Range<u64>> {
        let (start, end) = (u64::from(start), u64::from(end));
        (start <= end && end <= self.len).then_some(start..end)
    }

    fn section(&mut self, path: &str, start: u32, end: u32) {
        if let Some(range) = self.range(start, end) {
            self.sections.push(Section {
                path: path.to_owned(),
                range,
            });
        }
    }

    /// Письмо (верхнее или вложенное `message/rfc822` с путём `base`): дети составного корня —
    /// `base.1`, `base.2`, ...; тело несоставного корня — `base.1`.
    fn message(&mut self, msg: &Message<'_>, base: &str, depth: usize) {
        if depth > MAX_DEPTH {
            return;
        }
        let Some(root) = msg.parts.first() else {
            return;
        };
        match &root.body {
            PartType::Multipart(children) => self.children(msg, 0, children, base, depth),
            _ => self.part(msg, 0, &child_path(base, 1), depth),
        }
    }

    fn children(
        &mut self,
        msg: &Message<'_>,
        parent: usize,
        children: &[u32],
        base: &str,
        depth: usize,
    ) {
        let mut n = 0;
        for &id in children {
            let id = id as usize;
            // Дети идут после родителя; иное — битая структура, не обходим (защита от циклов).
            if id <= parent || id >= msg.parts.len() {
                continue;
            }
            n += 1;
            self.part(msg, id, &child_path(base, n), depth + 1);
        }
    }

    fn part(&mut self, msg: &Message<'_>, id: usize, path: &str, depth: usize) {
        if depth > MAX_DEPTH {
            return;
        }
        let Some(part) = msg.parts.get(id) else {
            return;
        };
        self.section(path, part.offset_body, part.offset_end);
        match &part.body {
            PartType::Multipart(children) => self.children(msg, id, children, path, depth),
            // Незакодированное вложенное письмо разобрано по тем же байтам: смещения его частей — в исходном
            // письме. Закодированное (base64/qp) разобрано из декодированных байтов — хранится одной частью.
            PartType::Message(nested) if part.encoding == Encoding::None => {
                self.message(nested, path, depth + 1);
            }
            _ => {
                if let Some(range) = self.range(part.offset_body, part.offset_end)
                    && !range.is_empty()
                {
                    self.cuts.push((path.to_owned(), range));
                }
            }
        }
    }
}

fn child_path(base: &str, n: usize) -> String {
    if base.is_empty() {
        n.to_string()
    } else {
        format!("{base}.{n}")
    }
}

/// Сортирует кандидатов и отбрасывает пересекающиеся и повторные пути (остаются в скелете).
fn normalize(mut cuts: Vec<(String, Range<u64>)>) -> Vec<(String, Range<u64>)> {
    cuts.sort_by_key(|(_, r)| (r.start, r.end));
    let mut out: Vec<(String, Range<u64>)> = Vec::with_capacity(cuts.len());
    for (path, range) in cuts {
        let free = out.last().is_none_or(|(_, last)| last.end <= range.start);
        if free && !out.iter().any(|(p, _)| *p == path) {
            out.push((path, range));
        }
    }
    out
}

fn slice<'a>(raw: &'a [u8], range: &Range<u64>) -> &'a [u8] {
    &raw[range.start as usize..range.end as usize]
}

/// SHA-256 байтов.
pub fn hash(data: &[u8]) -> BlobHash {
    BlobHash(Sha256::digest(data).into())
}

fn len_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn put_str(out: &mut BytesMut, s: &str) {
    // Пути IMAP короткие: глубина ограничена MAX_DEPTH, номера — десятичные.
    let b = &s.as_bytes()[..s.len().min(usize::from(u16::MAX))];
    out.put_u16_le(b.len() as u16);
    out.put_slice(b);
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::HashMap;

    fn roundtrip(raw: &[u8], prefix: &[u8]) -> (Split, Skeleton, Vec<u8>) {
        let split = Split::new(Bytes::copy_from_slice(raw));
        let skel = Skeleton::decode(split.skeleton(prefix)).expect("decode");
        let parts: HashMap<String, Bytes> = split
            .cuts()
            .iter()
            .map(|c| (c.path.clone(), split.body(c)))
            .collect();
        let joined = skel.join(|p| parts.get(p).cloned()).expect("join");
        (split, skel, joined)
    }

    #[test]
    fn corpus_split_join_is_exact() {
        let corpus = tp_testkit::corpus().expect("corpus");
        assert!(corpus.len() >= 40);
        for mail in &corpus {
            let (split, skel, joined) = roundtrip(&mail.raw, b"");
            assert!(joined == mail.raw, "{}: сборка не совпала", mail.name);
            assert_eq!(skel.size(), mail.raw.len() as u64, "{}", mail.name);
            let mut prefix = b"Delivered-To: a@example.org\r\n".to_vec();
            let (_, skel, joined) = roundtrip(&mail.raw, &prefix);
            prefix.extend_from_slice(&mail.raw);
            assert!(joined == prefix, "{}: сборка с префиксом", mail.name);
            assert_eq!(skel.size(), prefix.len() as u64);
            for c in split.cuts() {
                assert_eq!(c.hash, hash(&split.body(c)), "{}", mail.name);
            }
            if !mail.is_broken()
                && !mail.name.contains("headers-only")
                && !mail.name.contains("empty-body")
            {
                assert!(!split.cuts().is_empty(), "{}: нет частей", mail.name);
            }
        }
    }

    #[test]
    fn attachment_is_separate_part() {
        let mail = tp_testkit::corpus::corpus_mail("mixed-pdf").expect("mail");
        let split = Split::new(Bytes::from(mail.raw.clone()));
        let paths: Vec<_> = split.cuts().iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, ["1", "2"]);
        let pdf = split.body(&split.cuts()[1]);
        // Тело — в кодировке передачи, без заголовков части и без CRLF перед границей.
        assert!(pdf.starts_with(b"JVBER"), "{:?}", &pdf[..16.min(pdf.len())]);
        assert!(
            !pdf.ends_with(b"\n"),
            "{:?}",
            &pdf[pdf.len().saturating_sub(16)..]
        );
        let skel = Skeleton::decode(split.skeleton(b"")).expect("decode");
        assert_eq!(skel.section("2"), Some(split.cuts()[1].range.clone()));
        assert!(skel.section("").is_some());
    }

    #[test]
    fn duplicate_attachments_share_hash() {
        let mail = tp_testkit::corpus::corpus_mail("duplicate-attachments").expect("mail");
        let split = Split::new(Bytes::from(mail.raw.clone()));
        let hashes: Vec<_> = split.cuts().iter().map(|c| c.hash).collect();
        let unique: std::collections::HashSet<_> = hashes.iter().collect();
        assert!(unique.len() < hashes.len(), "{hashes:?}");
    }

    #[test]
    fn single_part_body_is_part_1() {
        let raw = b"Subject: x\r\n\r\nhello\r\n";
        let (split, skel, joined) = roundtrip(raw, b"");
        assert_eq!(joined, raw);
        assert_eq!(split.cuts().len(), 1);
        assert_eq!(split.cuts()[0].path, "1");
        assert_eq!(skel.section("1"), skel.section(""));
    }

    #[test]
    fn nested_message_paths() {
        let mail = tp_testkit::corpus::corpus_mail("rfc822-with-attachment").expect("mail");
        let split = Split::new(Bytes::from(mail.raw.clone()));
        let paths: Vec<_> = split.cuts().iter().map(|c| c.path.clone()).collect();
        assert!(
            paths.iter().any(|p| p.starts_with("2.")),
            "части вложенного письма вырезаются: {paths:?}"
        );
    }

    #[test]
    fn decode_rejects_garbage() {
        assert!(Skeleton::decode(Bytes::from_static(b"nope")).is_err());
        let split = Split::new(Bytes::from_static(b"Subject: x\r\n\r\nbody\r\n"));
        let skel = split.skeleton(b"");
        for n in 0..skel.len() {
            assert!(Skeleton::decode(skel.slice(..n)).is_err(), "обрезка {n}");
        }
        let mut longer = skel.to_vec();
        longer.push(b'x');
        assert!(Skeleton::decode(Bytes::from(longer)).is_err());
    }

    proptest! {
        #[test]
        fn arbitrary_bytes_roundtrip(raw in proptest::collection::vec(any::<u8>(), 0..2048)) {
            let (_, _, joined) = roundtrip(&raw, b"X: y\r\n");
            prop_assert_eq!(&joined[6..], &raw[..]);
        }

        #[test]
        fn mime_like_roundtrip(
            body in "[a-zA-Z0-9 \r\n-]{0,200}",
            att in "[A-Za-z0-9+/=\r\n]{0,300}",
        ) {
            let raw = format!(
                "Subject: t\r\nContent-Type: multipart/mixed; boundary=\"b\"\r\n\r\n--b\r\n\
                 Content-Type: text/plain\r\n\r\n{body}\r\n--b\r\nContent-Type: application/pdf\r\n\
                 Content-Transfer-Encoding: base64\r\n\r\n{att}\r\n--b--\r\n"
            );
            let (_, _, joined) = roundtrip(raw.as_bytes(), b"");
            prop_assert_eq!(joined, raw.into_bytes());
        }
    }
}
