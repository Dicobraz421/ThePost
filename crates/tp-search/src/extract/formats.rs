//! Текст из форматов вложений (выполняется в подпроцессе sandbox): готовые разборщики — `lopdf`
//! (PDF), `calamine` (XLSX, ODS), `zip` + `quick-xml` (DOCX, PPTX, ODT, ODP), `mail-parser` (HTML).

use std::io::{self, Cursor, Read};

use calamine::{Ods, Reader, Xlsx};
use quick_xml::escape::resolve_xml_entity;
use quick_xml::events::Event;
use zip::ZipArchive;

use super::{ExtractLimits, Format};

/// Причина, по которой текст не извлечён (текст ошибки задания sandbox).
pub(crate) type Failure = String;

/// Текст вложения формата `format`, не длиннее `limits.max_text_bytes`.
pub(crate) fn extract(
    format: Format,
    data: &[u8],
    limits: &ExtractLimits,
) -> Result<String, Failure> {
    let mut text = match format {
        Format::Pdf => pdf(data, limits)?,
        Format::Office => office(data, limits)?,
        Format::Html => mail_parser::decoders::html::html_to_text(&plain(data)),
        Format::Text => plain(data),
    };
    truncate(&mut text, limits.max_text_bytes);
    Ok(text)
}

/// Текст UTF-8 (неверные последовательности заменяются), без BOM.
fn plain(data: &[u8]) -> String {
    let data = data.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(data);
    String::from_utf8_lossy(data).into_owned()
}

/// Обрезает текст до `max` байт по границе символа.
fn truncate(text: &mut String, max: usize) {
    if text.len() > max {
        let mut end = max;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
}

fn pdf(data: &[u8], limits: &ExtractLimits) -> Result<String, Failure> {
    let doc = lopdf::Document::load_mem(data).map_err(|e| format!("pdf: {e}"))?;
    let pages: Vec<u32> = doc.get_pages().keys().copied().collect();
    let max = usize::try_from(limits.max_unpacked_bytes).unwrap_or(usize::MAX);
    doc.extract_text_with_limit(&pages, max)
        .map_err(|e| format!("pdf: {e}"))
}

/// Архив ZIP после проверки на «бомбу»: число файлов, суммарный распакованный размер (фактический, а не
/// заявленный в заголовках) и степень сжатия каждого файла.
fn checked_zip<'a>(
    data: &'a [u8],
    limits: &ExtractLimits,
) -> Result<ZipArchive<Cursor<&'a [u8]>>, Failure> {
    let mut zip = ZipArchive::new(Cursor::new(data)).map_err(|e| format!("zip: {e}"))?;
    if zip.len() > limits.max_entries {
        return Err(format!("zip bomb: {} entries", zip.len()));
    }
    let mut total = 0u64;
    for i in 0..zip.len() {
        let mut file = zip.by_index(i).map_err(|e| format!("zip: {e}"))?;
        let ratio_limit = file
            .compressed_size()
            .max(1)
            .saturating_mul(limits.max_ratio);
        let budget = limits.max_unpacked_bytes.saturating_sub(total);
        let limit = budget.min(ratio_limit);
        let n = io::copy(
            &mut (&mut file).take(limit.saturating_add(1)),
            &mut io::sink(),
        )
        .map_err(|e| format!("zip: {e}"))?;
        if n > limit {
            return Err(if n > ratio_limit {
                "zip bomb: compression ratio".to_owned()
            } else {
                "zip bomb: unpacked size".to_owned()
            });
        }
        total += n;
    }
    Ok(zip)
}

/// Документы в контейнере ZIP: OOXML (DOCX, PPTX, XLSX) и ODF (ODT, ODP, ODS).
fn office(data: &[u8], limits: &ExtractLimits) -> Result<String, Failure> {
    let mut zip = checked_zip(data, limits)?;
    let names: Vec<String> = zip
        .file_names()
        .filter_map(|n| n.ok())
        .map(|n| n.into_owned())
        .collect();
    let has = |name: &str| names.iter().any(|n| n == name);
    if has("word/document.xml") {
        return xml_text(&mut zip, &["word/document.xml".to_owned()]);
    }
    if has("ppt/presentation.xml") {
        let mut slides: Vec<(u32, String)> = names
            .iter()
            .filter_map(|n| {
                let num = n.strip_prefix("ppt/slides/slide")?.strip_suffix(".xml")?;
                Some((num.parse().ok()?, n.clone()))
            })
            .collect();
        slides.sort();
        let slides: Vec<String> = slides.into_iter().map(|(_, n)| n).collect();
        return xml_text(&mut zip, &slides);
    }
    if has("xl/workbook.xml") {
        let book: Xlsx<_> =
            calamine::open_workbook_from_rs(Cursor::new(data)).map_err(|e| format!("xlsx: {e}"))?;
        return Ok(sheets(book));
    }
    let mimetype = if has("mimetype") {
        let mut s = String::new();
        zip.by_name("mimetype")
            .map_err(|e| format!("zip: {e}"))?
            .take(256)
            .read_to_string(&mut s)
            .map_err(|e| format!("zip: {e}"))?;
        s
    } else {
        String::new()
    };
    match mimetype.trim() {
        "application/vnd.oasis.opendocument.spreadsheet" => {
            let book: Ods<_> = calamine::open_workbook_from_rs(Cursor::new(data))
                .map_err(|e| format!("ods: {e}"))?;
            Ok(sheets(book))
        }
        "application/vnd.oasis.opendocument.text"
        | "application/vnd.oasis.opendocument.presentation" => {
            xml_text(&mut zip, &["content.xml".to_owned()])
        }
        _ => Err("unsupported zip document".to_owned()),
    }
}

/// Ячейки листов: строки — через перевод строки, ячейки — через табуляцию.
fn sheets<RS, R: Reader<RS>>(mut book: R) -> String
where
    RS: io::Read + io::Seek,
{
    let mut out = String::new();
    for (_, range) in book.worksheets() {
        for row in range.rows() {
            let cells: Vec<String> = row
                .iter()
                .map(ToString::to_string)
                .filter(|c| !c.is_empty())
                .collect();
            if !cells.is_empty() {
                out.push_str(&cells.join("\t"));
                out.push('\n');
            }
        }
    }
    out
}

/// Элементы, после которых текст разделяется переводом строки (абзацы, заголовки, строки таблиц).
const BLOCKS: &[&str] = &["p", "h", "tr", "br", "line-break"];
/// Пустые элементы-разделители внутри абзаца.
const SPACES: &[&str] = &["tab", "s", "br", "line-break", "cr"];

/// Текст XML-частей документа по порядку.
fn xml_text(zip: &mut ZipArchive<Cursor<&[u8]>>, parts: &[String]) -> Result<String, Failure> {
    let mut out = String::new();
    for part in parts {
        let mut xml = Vec::new();
        zip.by_name(part)
            .map_err(|e| format!("zip: {e}"))?
            .read_to_end(&mut xml)
            .map_err(|e| format!("zip: {e}"))?;
        let mut reader = quick_xml::Reader::from_reader(xml.as_slice());
        let mut buf = Vec::new();
        loop {
            match reader
                .read_event_into(&mut buf)
                .map_err(|e| format!("xml: {e}"))?
            {
                Event::Text(t) => out.push_str(&t.xml10_content()),
                Event::CData(t) => out.push_str(&t.into_inner()),
                Event::GeneralRef(r) => {
                    if let Ok(Some(c)) = r.resolve_char_ref() {
                        out.push(c);
                    } else if let Some(s) = resolve_xml_entity(&r.xml10_content()) {
                        out.push_str(s);
                    }
                }
                Event::Empty(e) if SPACES.contains(&e.local_name().as_ref()) => out.push(' '),
                Event::End(e) if BLOCKS.contains(&e.local_name().as_ref()) => out.push('\n'),
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }
        out.push('\n');
    }
    Ok(out)
}
