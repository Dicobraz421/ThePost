//! BLOB-хранилище: локальный диск и S3, кэш NVMe, GC, «сироты», verify, мигратор (SRS 3.2–3.5, 3.7).

use std::ops::Range;

use tp_contracts::{Error, Result};

pub mod cache;
pub mod local;
pub mod s3;

pub use cache::{BlobCache, CacheConfig};
pub use local::{LocalConfig, LocalStore};
pub use s3::{S3Config, S3Store};

/// Проверка произвольного пути `put_raw`/`get_raw`/`delete_raw`: относительный, сегменты через `/`,
/// без пустых сегментов и сегментов, начинающихся с `.` (служебные каталоги, `..`).
fn check_raw_path(path: &str) -> Result<()> {
    let bad = path.is_empty()
        || path.contains(['\\', '\0'])
        || path
            .split('/')
            .any(|segment| segment.is_empty() || segment.starts_with('.'));
    if bad {
        return Err(Error::invalid(format!("bad object path: {path}")));
    }
    Ok(())
}

/// Диапазон чтения объекта размером `size`: конец обрезается по размеру,
/// начало за концом объекта или после конца диапазона — `Invalid`.
fn clamp_range(range: Option<Range<u64>>, size: u64) -> Result<Range<u64>> {
    let Some(range) = range else {
        return Ok(0..size);
    };
    if range.start > range.end || range.start > size {
        return Err(Error::invalid(format!(
            "range {range:?} out of object size {size}"
        )));
    }
    Ok(range.start..range.end.min(size))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_paths() {
        for ok in ["idx/t/a/seg", "a", "a/b.c"] {
            assert_eq!(check_raw_path(ok), Ok(()), "{ok}");
        }
        for bad in [
            "", "/abs", "a/", "a//b", "../x", "a/../b", ".tmp/x", "a/.b", "a\\b",
        ] {
            assert!(check_raw_path(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn ranges() {
        assert_eq!(clamp_range(None, 5), Ok(0..5));
        assert_eq!(clamp_range(Some(1..3), 5), Ok(1..3));
        assert_eq!(clamp_range(Some(2..10), 5), Ok(2..5));
        assert_eq!(clamp_range(Some(5..6), 5), Ok(5..5));
        assert!(clamp_range(Some(6..7), 5).is_err());
        assert!(clamp_range(Some(Range { start: 3, end: 2 }), 5).is_err());
    }
}
