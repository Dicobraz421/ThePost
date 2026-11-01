//! Подпроцесс в тестах — этот же тестовый бинарник, запущенный на тест [`worker_entry`].

use std::io::{Cursor, Write};

use mail_parser::{MessageParser, MimeHeaders};
use tp_testkit::corpus;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use super::*;

const TEST_WORKER_ENV: &str = "TP_SEARCH_EXTRACT_TEST_WORKER";

/// Точка входа подпроцесса; при обычном прогоне тестов ничего не делает.
#[test]
fn worker_entry() {
    if std::env::var_os(TEST_WORKER_ENV).is_none() {
        return;
    }
    let code = i32::from(run_worker().is_err());
    std::process::exit(code);
}

fn extractor(limits: ExtractLimits) -> Extractor {
    let cmd = WorkerCommand::current_exe([
        "--exact",
        "extract::tests::worker_entry",
        "--nocapture",
        "--test-threads=1",
        "-q",
    ])
    .expect("current_exe")
    .env(TEST_WORKER_ENV, "1");
    let cfg = ExtractConfig {
        sandbox: SandboxConfig {
            workers: 1,
            memory_limit_bytes: 1 << 30,
            timeout_ms: 30_000,
            ..SandboxConfig::default()
        },
        limits,
    };
    Extractor::new(cfg, cmd)
}

/// Разбор без подпроцесса (тот же код, что в подпроцессе).
fn direct(content_type: &str, name: &str, data: &[u8]) -> Result<String, NotExtracted> {
    let format = Format::detect(content_type, name, data).ok_or(NotExtracted::Unsupported)?;
    let job = Job {
        format,
        limits: ExtractLimits::default(),
    };
    let input = encode(&job, data)?;
    handle(&input)
        .map(|t| String::from_utf8(t).expect("utf-8"))
        .map_err(NotExtracted::Failed)
}

/// Вложения писем корпуса: тип, имя, данные.
fn corpus_attachments() -> Vec<(String, String, Vec<u8>)> {
    let parser = MessageParser::default();
    let mut out = Vec::new();
    for mail in corpus().expect("corpus") {
        let Some(msg) = parser.parse(&mail.raw) else {
            continue;
        };
        for a in msg.attachments() {
            let ct = a
                .content_type()
                .map(|c| match c.subtype() {
                    Some(sub) => format!("{}/{sub}", c.ctype()),
                    None => c.ctype().to_owned(),
                })
                .unwrap_or_default();
            let name = a.attachment_name().unwrap_or_default().to_owned();
            out.push((ct, name, a.contents().to_vec()));
        }
    }
    out
}

/// Ожидаемый текст вложений корпуса по имени файла.
const EXPECTED: &[(&str, &[&str])] = &[
    ("report.pdf", &["Quarterly report"]),
    ("a.pdf", &["Quarterly report"]),
    ("contract.docx", &["Договор поставки"]),
    ("b.docx", &["Договор поставки"]),
    ("budget.xlsx", &["Budget 2026", "42"]),
    ("c.xlsx", &["Budget 2026", "42"]),
];

fn check_corpus(name: &str, text: &str) {
    if let Some((_, words)) = EXPECTED.iter().find(|(n, _)| *n == name) {
        for w in *words {
            assert!(text.contains(w), "{name}: {w:?} not in {text:?}");
        }
    }
}

#[test]
fn corpus_attachments_direct() {
    let attachments = corpus_attachments();
    let mut checked = 0;
    for (ct, name, data) in &attachments {
        let res = direct(ct, name, data);
        if EXPECTED.iter().any(|(n, _)| n == name) {
            check_corpus(name, &res.expect(name));
            checked += 1;
        }
    }
    assert_eq!(checked, EXPECTED.len());
}

#[tokio::test]
async fn corpus_attachments_in_sandbox() {
    let ex = extractor(ExtractLimits::default());
    let mut checked = 0;
    for (ct, name, data) in corpus_attachments() {
        let res = ex.extract(&ct, &name, &data).await;
        if EXPECTED.iter().any(|(n, _)| *n == name) {
            check_corpus(&name, &res.expect("extracted"));
            checked += 1;
        }
    }
    assert_eq!(checked, EXPECTED.len());
}

fn zip_of(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut w = ZipWriter::new(Cursor::new(Vec::new()));
    let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    for (name, data) in files {
        w.start_file(*name, opts).expect("start");
        w.write_all(data).expect("write");
    }
    w.finish().expect("finish").into_inner()
}

const ODF_NS: &str = r#"xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0""#;

#[test]
fn odf_ooxml_html_text() {
    let odt = zip_of(&[
        ("mimetype", b"application/vnd.oasis.opendocument.text"),
        (
            "content.xml",
            format!(
                r#"<?xml version="1.0"?><office:document-content {ODF_NS}><office:body><office:text><text:h>Протокол</text:h><text:p>Решили &amp; утвердили<text:tab/>смету</text:p></office:text></office:body></office:document-content>"#
            )
            .as_bytes(),
        ),
    ]);
    let text = direct("application/vnd.oasis.opendocument.text", "p.odt", &odt).expect("odt");
    assert!(text.contains("Протокол\n"), "{text:?}");
    assert!(text.contains("Решили & утвердили смету"), "{text:?}");

    let ods = zip_of(&[
        ("mimetype", b"application/vnd.oasis.opendocument.spreadsheet"),
        (
            "META-INF/manifest.xml",
            br#"<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0"/>"#,
        ),
        (
            "content.xml",
            format!(
                r#"<?xml version="1.0"?><office:document-content {ODF_NS}><office:body><office:spreadsheet><table:table table:name="Лист1"><table:table-row><table:table-cell office:value-type="string"><text:p>Смета</text:p></table:table-cell><table:table-cell office:value-type="float" office:value="7"><text:p>7</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#
            )
            .as_bytes(),
        ),
    ]);
    let text = direct("", "s.ods", &ods).expect("ods");
    assert!(text.contains("Смета\t7"), "{text:?}");

    let slide = |t: &str| {
        format!(
            r#"<p:sld xmlns:p="p" xmlns:a="a"><p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>{t}</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#
        )
    };
    let (s1, s2, s10) = (slide("Первый слайд"), slide("Второй"), slide("Десятый"));
    let pptx = zip_of(&[
        ("ppt/presentation.xml", b"<p:presentation xmlns:p=\"p\"/>"),
        ("ppt/slides/slide10.xml", s10.as_bytes()),
        ("ppt/slides/slide2.xml", s2.as_bytes()),
        ("ppt/slides/slide1.xml", s1.as_bytes()),
    ]);
    let text = direct("", "deck.pptx", &pptx).expect("pptx");
    assert_eq!(text.trim(), "Первый слайд\n\nВторой\n\nДесятый", "{text:?}");

    let html = "<html><body><p>Счёт <b>№5</b></p><script>x()</script></body></html>";
    let text = direct("text/html", "", html.as_bytes()).expect("html");
    assert!(text.contains("Счёт") && text.contains("№5"), "{text:?}");
    assert!(!text.contains('<'), "{text:?}");

    let text = direct("text/plain", "", "\u{feff}Привет".as_bytes()).expect("txt");
    assert_eq!(text, "Привет");
}

#[test]
fn unsupported_and_broken() {
    assert_eq!(
        Format::detect("image/png", "a.png", b"\x89PNG"),
        None,
        "images are not extracted"
    );
    assert_eq!(
        direct("image/png", "a.png", b"\x89PNG"),
        Err(NotExtracted::Unsupported)
    );
    assert!(matches!(
        direct("application/pdf", "x.pdf", b"%PDF-1.4 garbage"),
        Err(NotExtracted::Failed(_))
    ));
    assert!(matches!(
        direct("", "x.zip", &zip_of(&[("a.txt", b"x")])),
        Err(NotExtracted::Failed(_))
    ));
    assert!(decode(b"\xff\xff\xff\xff{}").is_err());
}

#[test]
fn text_is_truncated() {
    let limits = ExtractLimits {
        max_text_bytes: 5,
        ..ExtractLimits::default()
    };
    let text = formats::extract(Format::Text, "Привет".as_bytes(), &limits).expect("txt");
    assert_eq!(text, "Пр");
}

/// Архив-«бомба»: 16 МиБ нулей сжимаются примерно в 16 КиБ (степень сжатия ~1000).
fn bomb() -> Vec<u8> {
    let zeros = vec![0u8; 16 << 20];
    zip_of(&[
        ("word/document.xml", b"<w:document/>"),
        ("word/media/zero.bin", &zeros),
    ])
}

#[test]
fn zip_bomb_rejected_direct() {
    let err = direct("", "bomb.docx", &bomb()).expect_err("bomb");
    assert!(
        matches!(&err, NotExtracted::Failed(m) if m.contains("zip bomb")),
        "{err:?}"
    );
    // Предел распакованного размера и числа файлов — независимо от степени сжатия.
    let doc = zip_of(&[
        (
            "word/document.xml",
            b"<w:document><w:p><w:t>x</w:t></w:p></w:document>",
        ),
        ("a", &[7u8; 4096]),
    ]);
    let small = ExtractLimits {
        max_unpacked_bytes: 1024,
        ..ExtractLimits::default()
    };
    let res = formats::extract(Format::Office, &doc, &small);
    assert_eq!(res, Err("zip bomb: unpacked size".to_owned()));
    let few = ExtractLimits {
        max_entries: 1,
        ..ExtractLimits::default()
    };
    let res = formats::extract(Format::Office, &doc, &few);
    assert_eq!(res, Err("zip bomb: 2 entries".to_owned()));
}

#[tokio::test]
async fn zip_bomb_rejected_in_sandbox_and_pool_recovers() {
    let ex = extractor(ExtractLimits::default());
    let err = ex
        .extract("", "bomb.docx", &bomb())
        .await
        .expect_err("bomb");
    assert!(
        matches!(&err, NotExtracted::Failed(m) if m.contains("zip bomb")),
        "{err:?}"
    );
    let text = ex
        .extract("text/plain", "a.txt", "после бомбы".as_bytes())
        .await
        .expect("pool recovers");
    assert_eq!(text, "после бомбы");
}

#[test]
fn config_defaults_and_unknown() {
    assert_eq!(
        crate::tests::from_pairs::<ExtractLimits>(&[]).expect("defaults"),
        ExtractLimits::default()
    );
    assert!(crate::tests::from_pairs::<ExtractLimits>(&[("max_text", "1")]).is_err());
}
