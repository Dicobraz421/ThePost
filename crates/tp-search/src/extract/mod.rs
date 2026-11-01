//! Извлечение текста вложений (SRS 4.6, 2.7): PDF, DOCX, PPTX, XLSX, ODT, ODP, ODS, HTML, TXT.
//!
//! Недоверенные файлы разбираются только в изолированном подпроцессе пула SYS-04 ([`SandboxPool`]:
//! seccomp без сети и файлов, пределы памяти, CPU и времени). Подпроцесс — подкоманда того же бинарного
//! файла [`WORKER_SUBCOMMAND`], вызывающая [`run_worker`]. Родитель определяет формат по типу, имени и
//! сигнатуре (без разбора), подпроцесс разбирает его готовыми библиотеками: `lopdf`, `calamine`,
//! `zip` + `quick-xml`, `mail-parser` (HTML). Архив ZIP проверяется до разбора: число файлов, фактический
//! распакованный размер и степень сжатия ([`ExtractLimits`]); «бомба» отклоняется.
//!
//! Сбой разбора, предел или аварийное завершение подпроцесса — вложение «не извлечено»
//! ([`NotExtracted`]): письмо индексируется без его текста, родительский процесс не затрагивается.

mod formats;

#[cfg(test)]
mod tests;

use std::io;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tp_sys::sandbox::{SandboxConfig, SandboxPool, WorkerCommand};

/// Подкоманда бинарного файла — подпроцесс извлечения текста.
pub const WORKER_SUBCOMMAND: &str = "search-extract";

/// Пределы разбора одного вложения (передаются подпроцессу с заданием).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ExtractLimits {
    /// Наибольший размер извлечённого текста, байт (дальше текст обрезается).
    pub max_text_bytes: usize,
    /// Наибольший распакованный размер архива (DOCX, XLSX, ODF) или потоков PDF, байт.
    pub max_unpacked_bytes: u64,
    /// Наибольшее число файлов в архиве.
    pub max_entries: usize,
    /// Наибольшая степень сжатия файла архива (распакованный размер к сжатому).
    pub max_ratio: u64,
}

impl Default for ExtractLimits {
    fn default() -> Self {
        Self {
            max_text_bytes: 1 << 20,
            max_unpacked_bytes: 256 << 20,
            max_entries: 10_000,
            max_ratio: 200,
        }
    }
}

/// Извлечение текста вложений (секция `search.extract`; поля необязательны, неизвестное — ошибка).
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ExtractConfig {
    /// Пул изолированных подпроцессов: число, память, CPU, время задания, размер входа и результата.
    pub sandbox: SandboxConfig,
    /// Пределы разбора.
    pub limits: ExtractLimits,
}

/// Формат вложения для подпроцесса.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Format {
    /// PDF.
    Pdf,
    /// Документ в контейнере ZIP: DOCX, PPTX, XLSX, ODT, ODP, ODS (вид определяет подпроцесс).
    Office,
    /// HTML.
    Html,
    /// Текст.
    Text,
}

/// Типы MIME документов в контейнере ZIP.
const OFFICE_TYPES: &[&str] = &[
    "application/vnd.openxmlformats-officedocument.",
    "application/vnd.oasis.opendocument.",
];
const OFFICE_EXTS: &[&str] = &["docx", "pptx", "xlsx", "odt", "odp", "ods"];

impl Format {
    /// Формат по типу MIME, имени файла и сигнатуре; не поддерживается — `None`.
    pub fn detect(content_type: &str, name: &str, data: &[u8]) -> Option<Format> {
        let ct = content_type.trim().to_ascii_lowercase();
        let ext = name
            .rsplit_once('.')
            .map(|(_, e)| e.to_ascii_lowercase())
            .unwrap_or_default();
        if data.starts_with(b"%PDF-") || ct == "application/pdf" || ext == "pdf" {
            return Some(Format::Pdf);
        }
        if data.starts_with(b"PK\x03\x04")
            && (OFFICE_TYPES.iter().any(|t| ct.starts_with(t))
                || OFFICE_EXTS.contains(&ext.as_str())
                || ct == "application/octet-stream"
                || ct.is_empty())
        {
            return Some(Format::Office);
        }
        if ct == "text/html" || ext == "html" || ext == "htm" {
            return Some(Format::Html);
        }
        if ct.starts_with("text/") || ext == "txt" {
            return Some(Format::Text);
        }
        None
    }
}

/// Вложение не извлечено: письмо индексируется без его текста.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NotExtracted {
    #[error("формат вложения не поддерживается")]
    Unsupported,
    #[error("текст вложения не извлечён: {0}")]
    Failed(String),
}

/// Задание подпроцессу: заголовок JSON и данные.
#[derive(Debug, Serialize, Deserialize)]
struct Job {
    format: Format,
    limits: ExtractLimits,
}

fn encode(job: &Job, data: &[u8]) -> Result<Vec<u8>, NotExtracted> {
    let header = serde_json::to_vec(job).map_err(|e| NotExtracted::Failed(e.to_string()))?;
    let len = u32::try_from(header.len()).map_err(|_| NotExtracted::Failed("header".into()))?;
    let mut out = Vec::with_capacity(4 + header.len() + data.len());
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&header);
    out.extend_from_slice(data);
    Ok(out)
}

fn decode(input: &[u8]) -> Result<(Job, &[u8]), String> {
    let (len, rest) = input
        .split_first_chunk::<4>()
        .ok_or_else(|| "bad job".to_owned())?;
    let len = usize::try_from(u32::from_le_bytes(*len)).map_err(|e| e.to_string())?;
    if rest.len() < len {
        return Err("bad job".to_owned());
    }
    let (header, data) = rest.split_at(len);
    let job = serde_json::from_slice(header).map_err(|e| format!("bad job: {e}"))?;
    Ok((job, data))
}

/// Обработчик задания в подпроцессе: текст вложения в UTF-8 или причина сбоя.
pub fn handle(input: &[u8]) -> Result<Vec<u8>, String> {
    let (job, data) = decode(input)?;
    formats::extract(job.format, data, &job.limits).map(String::into_bytes)
}

/// Точка входа подкоманды [`WORKER_SUBCOMMAND`]: вызывать до запуска рантайма и потоков.
pub fn run_worker() -> io::Result<()> {
    tp_sys::sandbox::run_worker(handle)
}

/// Извлечение текста вложений в пуле изолированных подпроцессов.
#[derive(Debug)]
pub struct Extractor {
    pool: SandboxPool,
    limits: ExtractLimits,
}

impl Extractor {
    /// Извлечение подпроцессами `cmd` (обычно [`Extractor::worker_command`]).
    pub fn new(cfg: ExtractConfig, cmd: WorkerCommand) -> Self {
        Self {
            pool: SandboxPool::new(cfg.sandbox, cmd),
            limits: cfg.limits,
        }
    }

    /// Подпроцесс — подкоманда [`WORKER_SUBCOMMAND`] текущего бинарного файла.
    pub fn worker_command() -> io::Result<WorkerCommand> {
        WorkerCommand::current_exe([WORKER_SUBCOMMAND])
    }

    /// Текст вложения с типом `content_type` и именем `name`.
    pub async fn extract(
        &self,
        content_type: &str,
        name: &str,
        data: &[u8],
    ) -> Result<String, NotExtracted> {
        let format = Format::detect(content_type, name, data).ok_or(NotExtracted::Unsupported)?;
        let job = Job {
            format,
            limits: self.limits,
        };
        let input = encode(&job, data)?;
        match self.pool.run(&input).await {
            Ok(text) => Ok(String::from_utf8_lossy(&text).into_owned()),
            Err(e) => {
                tracing::debug!(?format, error = %e, "attachment text not extracted");
                Err(NotExtracted::Failed(e.to_string()))
            }
        }
    }
}
