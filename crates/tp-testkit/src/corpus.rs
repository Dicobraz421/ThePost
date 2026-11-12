//! Тестовый корпус писем, генератор синтетических писем по модели нагрузки (SRS прил. Д.3)
//! и хелперы тестов с PostgreSQL.
//!
//! Корпус — `crates/tp-testkit/corpus/*.eml`, байты как есть (`.gitattributes`: без нормализации):
//! multipart/mixed/alternative/related/report/signed, вложения PDF/DOCX/XLSX, 8bit, КОИ-8, Windows-1251,
//! UTF-8, ISO-2022-JP, вложенные message/rfc822, длинные строки; файлы `NN-broken-*` — битый MIME.
//!
//! Корпус vCard — `crates/tp-testkit/corpus/*.vcf` ([`vcards`], [`vcard`]): по одной карточке в файле,
//! строки с CRLF, данные вымышленные; что проверяет каждый файл — [`VCARDS`].

use std::fs;
use std::path::PathBuf;

use tp_contracts::{Result, TenantId};

/// Письмо корпуса.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorpusMail {
    /// Имя файла без расширения (`20-mixed-pdf`).
    pub name: String,
    /// Исходные байты письма.
    pub raw: Vec<u8>,
}

impl CorpusMail {
    /// Письмо с заведомо битым MIME (разбор должен не падать, результат не проверяется).
    pub fn is_broken(&self) -> bool {
        self.name.contains("-broken-")
    }
}

/// Каталог корпуса.
pub fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("corpus")
}

/// Все письма корпуса по имени файла. Чтение синхронное: вызывать вне async-кода
/// (или через `spawn_blocking`).
pub fn corpus() -> Result<Vec<CorpusMail>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(corpus_dir())? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "eml")
            && let Some(name) = path.file_stem().and_then(|s| s.to_str())
        {
            out.push(CorpusMail {
                name: name.to_owned(),
                raw: fs::read(&path)?,
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Письмо корпуса по имени файла без номера и расширения (`mixed-pdf`).
pub fn corpus_mail(name: &str) -> Result<CorpusMail> {
    corpus()?
        .into_iter()
        .find(|m| m.name.split_once('-').is_some_and(|(_, n)| n == name))
        .ok_or(tp_contracts::Error::NotFound)
}

// ---------- vCard ----------

/// Файлы корпуса vCard: имя без расширения и что файл проверяет.
pub const VCARDS: &[(&str, &str)] = &[
    (
        "01-v4-cyrillic-x",
        "vCard 4.0: кириллица в FN/N/ORG/NOTE, ORG из трёх уровней, X-свойства (в том числе с X-параметром)",
    ),
    (
        "02-v4-org",
        "vCard 4.0: организация (KIND:org) без свойства N",
    ),
    (
        "03-v3-item-group",
        "vCard 3.0: группы свойств item1./item2./item3. с подписями X-ABLabel, TYPE списком через запятую",
    ),
    (
        "04-v4-group-members",
        "vCard 4.0: группа контактов (KIND:group), MEMBER — urn:uuid карточек 01 и 07 и mailto:",
    ),
    (
        "05-v4-photo-data-uri",
        "vCard 4.0: фото в PHOTO как data:-URI (PNG, base64, перенос строк), LOGO — ссылкой с MEDIATYPE",
    ),
    (
        "06-v3-photo-base64",
        "vCard 3.0: фото в PHOTO;ENCODING=b;TYPE=PNG",
    ),
    (
        "07-v4-multi-adr-tel",
        "vCard 4.0: три адреса, четыре телефона и три почты с TYPE и PREF; LABEL у адреса, tel:-URI с ext",
    ),
    (
        "08-v4-escaping-folding",
        "vCard 4.0: экранирование `\\,` `\\;` `\\\\` `\\n` `\\N`, параметр в кавычках с `,;:` и `^n` (RFC 6868), \
         перенос длинных строк по 75 октетов (кириллица, ASCII, внутри значения параметра)",
    ),
    (
        "09-export-apple",
        "выгрузка «Контактов» macOS (3.0): PRODID, type= строчными и повторами, item-группы, X-ABADR, \
         X-ABDATE, X-ABLabel вида `_$!<…>!$_`, X-ABUID; без UID",
    ),
    (
        "10-export-google",
        "выгрузка Google Контактов (3.0): повтор TYPE, item-группы с X-ABLabel, CATEGORIES:myContacts; без UID",
    ),
    (
        "11-export-outlook-v21",
        "выгрузка Outlook (vCard 2.1): CHARSET=Windows-1251 и ENCODING=QUOTED-PRINTABLE с мягкими переносами, \
         параметры без имени (TEL;WORK;VOICE), LABEL, X-MS-OL-*; без UID; файл — чистый ASCII",
    ),
    (
        "12-export-thunderbird",
        "выгрузка Thunderbird (4.0): EMAIL;PREF=1 первой строкой, VALUE=TEXT у TEL и TZ, VALUE=URL, UID без urn:uuid",
    ),
    (
        "13-export-nextcloud",
        "выгрузка Nextcloud / Sabre VObject (3.0): PRODID, TYPE в кавычках, X-SOCIALPROFILE, \
         BDAY;VALUE=DATE, REV;VALUE=DATE-AND-OR-TIME",
    ),
];

/// Карточка корпуса vCard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorpusCard {
    /// Имя файла без расширения (`02-v4-org`).
    pub name: String,
    /// Текст карточки как в файле (UTF-8, CRLF, строки перенесены).
    pub text: String,
    /// Что файл проверяет ([`VCARDS`]).
    pub checks: &'static str,
}

/// Все карточки корпуса vCard по имени файла. Чтение синхронное: вызывать вне async-кода
/// (или через `spawn_blocking`).
pub fn vcards() -> Result<Vec<CorpusCard>> {
    let dir = corpus_dir();
    VCARDS
        .iter()
        .map(|&(name, checks)| {
            Ok(CorpusCard {
                name: name.to_owned(),
                text: fs::read_to_string(dir.join(format!("{name}.vcf")))?,
                checks,
            })
        })
        .collect()
}

/// Карточка корпуса по имени файла без номера и расширения (`v4-org`).
pub fn vcard(name: &str) -> Result<CorpusCard> {
    vcards()?
        .into_iter()
        .find(|c| c.name.split_once('-').is_some_and(|(_, n)| n == name))
        .ok_or(tp_contracts::Error::NotFound)
}

// ---------- PostgreSQL ----------

/// URL тестовой базы из `TEST_DATABASE_URL`; `None` — тесты с PostgreSQL пропускаются.
pub fn pg_url() -> Option<String> {
    std::env::var("TEST_DATABASE_URL")
        .ok()
        .filter(|u| !u.trim().is_empty())
}

/// Уникальное имя схемы для изоляции теста (`test_<32 hex>`): тест создаёт её
/// (`CREATE SCHEMA`), работает в ней (`search_path`, см. [`pg_url_in_schema`]) и удаляет.
pub fn pg_schema() -> String {
    let id = TenantId::new().to_string().replace('-', "");
    format!("test_{id}")
}

/// URL тестовой базы с `search_path` = `schema` (параметр `options` libpq); `None` — нет `TEST_DATABASE_URL`.
pub fn pg_url_in_schema(schema: &str) -> Option<String> {
    let url = pg_url()?;
    let sep = if url.contains('?') { '&' } else { '?' };
    Some(format!("{url}{sep}options=-c%20search_path%3D{schema}"))
}

// ---------- Синтетические письма ----------

/// Модель нагрузки для генератора (SRS прил. Д.3). Средний размер письма относится к тексту
/// письма без вложений; вложения добавляются к нему.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadModel {
    /// Средний размер текста письма, байт (Д.3: 75 KB).
    pub avg_body: usize,
    /// Доля писем с вложениями (Д.3: 20 %).
    pub attachment_ratio: f64,
    /// Средний размер вложения, байт (Д.3: 500 KB).
    pub avg_attachment: usize,
    /// Доля вложений, совпадающих с ранее сгенерированными, — внутренние дубли MIME-частей (Д.3: 30 %).
    pub duplicate_ratio: f64,
    /// Число пользователей домена, между которыми идёт переписка.
    pub users: u32,
    /// Домен адресов.
    pub domain: String,
}

impl Default for LoadModel {
    fn default() -> Self {
        Self {
            avg_body: 75 * 1024,
            attachment_ratio: 0.2,
            avg_attachment: 500 * 1024,
            duplicate_ratio: 0.3,
            users: 100,
            domain: "example.com".into(),
        }
    }
}

/// SplitMix64: детерминированный генератор (одинаковый корпус при одинаковом зерне).
#[derive(Debug, Clone)]
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Равномерно в `[0, 1)`.
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    /// Размер около `avg`: равномерно в `[avg/2, 3avg/2]`.
    fn around(&mut self, avg: usize) -> usize {
        avg / 2 + self.below(avg + 1)
    }
}

const WORDS: &[&str] = &[
    "отчёт",
    "договор",
    "поставка",
    "совещание",
    "бюджет",
    "проект",
    "срок",
    "согласование",
    "клиент",
    "счёт",
    "report",
    "meeting",
    "budget",
    "deadline",
    "review",
    "contract",
    "invoice",
    "schedule",
    "quarter",
    "release",
    "и",
    "в",
    "на",
    "по",
    "the",
    "of",
    "to",
    "for",
];

const ATTACHMENTS: &[(&str, &str)] = &[
    ("application/pdf", "pdf"),
    (
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "docx",
    ),
    (
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "xlsx",
    ),
    ("application/octet-stream", "bin"),
];

/// Генератор синтетических писем по [`LoadModel`]; бесконечный итератор байтов писем (CRLF).
#[derive(Debug, Clone)]
pub struct SyntheticMail {
    model: LoadModel,
    rng: Rng,
    seq: u64,
    /// Ранее сгенерированные вложения (base64) для дублей.
    pool: Vec<(usize, String)>,
}

impl SyntheticMail {
    /// Генератор с зерном `seed`.
    pub fn new(seed: u64, model: LoadModel) -> Self {
        Self {
            model,
            rng: Rng(seed),
            seq: 0,
            pool: Vec::new(),
        }
    }

    fn address(&mut self) -> String {
        let n = self.rng.below(self.model.users.max(1) as usize);
        format!("user{n}@{}", self.model.domain)
    }

    fn text(&mut self, size: usize) -> String {
        let mut out = String::with_capacity(size + 80);
        let mut line = 0;
        while out.len() < size {
            let w = WORDS[self.rng.below(WORDS.len())];
            if line + w.len() > 72 {
                out.push_str("\r\n");
                line = 0;
            } else if line > 0 {
                out.push(' ');
                line += 1;
            }
            out.push_str(w);
            line += w.len();
        }
        out.push_str("\r\n");
        out
    }

    /// Вложение: тип и тело base64 (новое или дубль ранее выданного).
    fn attachment(&mut self) -> (usize, String) {
        if !self.pool.is_empty() && self.rng.unit() < self.model.duplicate_ratio {
            let i = self.rng.below(self.pool.len());
            return self.pool[i].clone();
        }
        let kind = self.rng.below(ATTACHMENTS.len());
        let size = self.rng.around(self.model.avg_attachment);
        let data: Vec<u8> = (0..size.div_ceil(8))
            .flat_map(|_| self.rng.next().to_le_bytes())
            .take(size)
            .collect();
        let encoded = base64(&data);
        self.pool.push((kind, encoded.clone()));
        (kind, encoded)
    }
}

impl Iterator for SyntheticMail {
    type Item = Vec<u8>;

    fn next(&mut self) -> Option<Vec<u8>> {
        self.seq += 1;
        let seq = self.seq;
        let (from, to) = (self.address(), self.address());
        let subject = format!("Synthetic {seq}: {}", WORDS[self.rng.below(10)]);
        let body_size = self.rng.around(self.model.avg_body);
        let body = self.text(body_size);
        let mut out = format!(
            "From: {from}\r\nTo: {to}\r\nSubject: {subject}\r\nMessage-ID: <synthetic-{seq}@{domain}>\r\n\
             Date: Thu, 29 Oct 2026 10:00:00 +0000\r\nMIME-Version: 1.0\r\n",
            domain = self.model.domain
        );
        let text_headers =
            "Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n";
        if self.rng.unit() < self.model.attachment_ratio {
            let (kind, data) = self.attachment();
            let (ctype, ext) = ATTACHMENTS[kind];
            out.push_str(&format!(
                "Content-Type: multipart/mixed; boundary=\"syn-{seq}\"\r\n\r\n--syn-{seq}\r\n{text_headers}{body}\
                 --syn-{seq}\r\nContent-Type: {ctype}; name=\"file-{seq}.{ext}\"\r\n\
                 Content-Transfer-Encoding: base64\r\nContent-Disposition: attachment; filename=\"file-{seq}.{ext}\"\r\n\r\n\
                 {data}--syn-{seq}--\r\n"
            ));
        } else {
            out.push_str(text_headers);
            out.push_str(&body);
        }
        Some(out.into_bytes())
    }
}

/// Base64 со строками по 76 символов (CRLF).
fn base64(data: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len() * 4 / 3 + data.len() / 57 * 2 + 4);
    for (i, chunk) in data.chunks(3).enumerate() {
        if i > 0 && i % 19 == 0 {
            out.push_str("\r\n");
        }
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for j in 0..4 {
            if j <= chunk.len() {
                out.push(char::from(ABC[((n >> (18 - 6 * j)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out.push_str("\r\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use mail_parser::{MessageParser, MimeHeaders};

    #[test]
    fn corpus_parses_without_panic() {
        let all = corpus().expect("corpus");
        assert!(all.len() >= 40, "в корпусе {} писем", all.len());
        let parser = MessageParser::default();
        for mail in &all {
            let parsed = parser.parse(&mail.raw);
            if !mail.is_broken() {
                let msg = parsed.unwrap_or_else(|| panic!("{} не разобрано", mail.name));
                assert!(
                    msg.subject().is_some() || mail.name.contains("headers-only"),
                    "{}",
                    mail.name
                );
            }
        }
    }

    /// Логические строки карточки: переносы RFC 6350 (CRLF + пробел/табуляция) сняты.
    fn unfold(text: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for line in text.split("\r\n").filter(|l| !l.is_empty()) {
            match (line.strip_prefix([' ', '\t']), out.last_mut()) {
                (Some(rest), Some(prev)) => prev.push_str(rest),
                _ => out.push(line.to_owned()),
            }
        }
        out
    }

    #[test]
    fn vcards_table_matches_files() {
        let mut files: Vec<String> = fs::read_dir(corpus_dir())
            .expect("corpus dir")
            .map(|e| e.expect("entry").path())
            .filter(|p| p.extension().is_some_and(|e| e == "vcf"))
            .map(|p| {
                p.file_stem()
                    .expect("stem")
                    .to_str()
                    .expect("utf-8")
                    .to_owned()
            })
            .collect();
        files.sort();
        let table: Vec<&str> = VCARDS.iter().map(|(n, _)| *n).collect();
        assert_eq!(files, table, "каждый .vcf описан в VCARDS и наоборот");
        assert!(VCARDS.iter().all(|(_, checks)| !checks.is_empty()));
        assert_eq!(vcards().expect("vcards").len(), VCARDS.len());
    }

    #[test]
    fn vcards_are_well_formed() {
        for card in vcards().expect("vcards") {
            let n = &card.name;
            assert!(card.text.starts_with("BEGIN:VCARD\r\nVERSION:"), "{n}");
            assert!(card.text.ends_with("\r\nEND:VCARD\r\n"), "{n}");
            assert_eq!(
                card.text.matches("BEGIN:VCARD").count(),
                1,
                "{n}: одна карточка в файле"
            );
            assert_eq!(
                card.text.matches('\n').count(),
                card.text.matches("\r\n").count(),
                "{n}: только CRLF"
            );
            assert!(
                card.text.split("\r\n").all(|l| l.len() <= 75),
                "{n}: строка длиннее 75 октетов"
            );
            let lines = unfold(&card.text);
            let has = |prop: &str| {
                lines.iter().any(|l| {
                    l.strip_prefix(prop)
                        .is_some_and(|r| r.starts_with([':', ';']))
                })
            };
            assert!(has("FN"), "{n}: нет FN");
            // N обязателен до vCard 4.0.
            assert!(has("N") || lines[1] == "VERSION:4.0", "{n}: нет N");
        }
    }

    #[test]
    fn vcards_content() {
        let lines = |name: &str| unfold(&vcard(name).expect(name).text);
        assert!(
            lines("v4-cyrillic-x").contains(&"X-THEPOST-TEST:произвольное значение".to_owned())
        );
        assert!(lines("v4-org").contains(&"KIND:org".to_owned()));
        assert!(
            lines("v3-item-group")
                .iter()
                .any(|l| l.starts_with("item1.EMAIL"))
        );
        let group = lines("v4-group-members");
        assert!(group.contains(&"KIND:group".to_owned()));
        assert_eq!(group.iter().filter(|l| l.starts_with("MEMBER:")).count(), 3);
        assert!(
            lines("v4-photo-data-uri")
                .iter()
                .any(|l| l.starts_with("PHOTO:data:image/png;base64,iVBOR"))
        );
        assert!(
            lines("v3-photo-base64")
                .iter()
                .any(|l| l.starts_with("PHOTO;ENCODING=b;TYPE=PNG:iVBOR"))
        );
        let multi = lines("v4-multi-adr-tel");
        for (prop, count) in [("ADR", 3), ("TEL", 4), ("EMAIL", 3)] {
            assert_eq!(
                multi.iter().filter(|l| l.starts_with(prop)).count(),
                count,
                "{prop}"
            );
        }
        let esc = lines("v4-escaping-folding");
        let note = esc.iter().find(|l| l.starts_with("NOTE:")).expect("NOTE");
        assert!(
            note.len() > 300
                && note.contains("\\n")
                && note.contains("\\;")
                && note.contains("\\\\")
        );
        assert!(
            note.contains("перенести"),
            "перенос не рвёт слова при склейке"
        );
        let outlook = vcard("export-outlook-v21").expect("outlook").text;
        assert!(outlook.is_ascii() && outlook.contains("=\r\n"));
        assert!(matches!(
            vcard("no-such-card"),
            Err(tp_contracts::Error::NotFound)
        ));
    }

    #[test]
    fn corpus_content() {
        let parser = MessageParser::default();
        let parse = |name: &str| corpus_mail(name).expect(name).raw;
        let koi = parse("plain-koi8r-8bit");
        let msg = parser.parse(&koi).expect("koi8-r");
        assert!(msg.body_text(0).expect("text").contains("Привет"));
        assert_eq!(msg.subject(), Some("Письмо в КОИ-8"));
        let cp = parse("plain-cp1251-8bit");
        let msg = parser.parse(&cp).expect("cp1251");
        assert!(msg.body_text(0).expect("text").contains("булок"));

        let three = parse("mixed-three-attachments");
        let msg = parser.parse(&three).expect("three");
        let names: Vec<_> = msg
            .attachments()
            .filter_map(|a| a.attachment_name())
            .collect();
        assert_eq!(names, ["a.pdf", "b.docx", "c.xlsx"]);
        let pdf = msg.attachment(0).expect("pdf");
        assert!(pdf.contents().starts_with(b"%PDF-"));

        let nested = parse("rfc822-nested-3");
        let msg = parser.parse(&nested).expect("nested");
        let inner = msg
            .attachment(0)
            .and_then(|a| a.message())
            .expect("level 2");
        assert_eq!(inner.subject(), Some("Level 2"));
        assert!(corpus_mail("nope").is_err());
    }

    #[test]
    fn synthetic_follows_model() {
        let model = LoadModel {
            avg_body: 2048,
            avg_attachment: 8192,
            ..LoadModel::default()
        };
        let mails: Vec<_> = SyntheticMail::new(7, model.clone()).take(500).collect();
        assert_eq!(
            mails,
            SyntheticMail::new(7, model).take(500).collect::<Vec<_>>(),
            "детерминирован по зерну"
        );
        let parser = MessageParser::default();
        let mut with_att = 0;
        let mut bodies = Vec::new();
        for raw in &mails {
            let msg = parser.parse(raw).expect("synthetic");
            if let Some(a) = msg.attachment(0) {
                with_att += 1;
                bodies.push(a.contents().to_vec());
            }
        }
        let ratio = f64::from(with_att) / mails.len() as f64;
        assert!((0.12..0.28).contains(&ratio), "доля с вложениями {ratio}");
        let unique: std::collections::HashSet<_> = bodies.iter().collect();
        let dup = 1.0 - unique.len() as f64 / bodies.len() as f64;
        assert!((0.15..0.45).contains(&dup), "доля дублей {dup}");
        assert!(bodies.iter().all(|b| (4096..=12288).contains(&b.len())));
    }

    #[test]
    fn pg_helpers() {
        let a = pg_schema();
        assert!(a.starts_with("test_") && a.len() == 37);
        assert!(a.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_'));
        assert_ne!(a, pg_schema());
        if pg_url().is_some() {
            assert!(pg_url_in_schema(&a).expect("url").contains(&a));
        } else {
            assert_eq!(pg_url_in_schema(&a), None);
        }
    }

    #[test]
    fn base64_matches_reference() {
        assert_eq!(base64(b""), "\r\n");
        assert_eq!(base64(b"f"), "Zg==\r\n");
        assert_eq!(base64(b"fo"), "Zm8=\r\n");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy\r\n");
        let long = base64(&[0u8; 120]);
        assert!(long.lines().all(|l| l.len() <= 76));
    }
}
