//! Объём и свободное место файловой системы (`statvfs(3)`, SYS-13).

use std::ffi::CString;
use std::io;
use std::mem::MaybeUninit;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use nix::libc;

/// Объём файловой системы, в байтах.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsUsage {
    /// Полный объём файловой системы.
    pub total: u64,
    /// Доступно непривилегированному процессу (без резерва суперпользователя).
    pub available: u64,
}

/// Объём и свободное место файловой системы, на которой лежит `path`.
///
/// Вызов блокирующий: в async-коде — через `spawn_blocking`.
///
/// # Errors
/// Ошибка `statvfs` (нет пути, нет прав, ошибка ввода-вывода); путь с нулевым байтом —
/// [`io::ErrorKind::InvalidInput`].
pub fn fs_usage(path: &Path) -> io::Result<FsUsage> {
    let c_path = CString::new(path.as_os_str().as_bytes())?;
    let mut buf = MaybeUninit::<libc::statvfs>::uninit();
    loop {
        // SAFETY: `c_path` — строка с завершающим нулём, живёт до конца вызова; `buf` — память под
        // одну структуру `statvfs`, ядро заполняет её целиком при успехе.
        #[allow(unsafe_code)]
        let rc = unsafe { libc::statvfs(c_path.as_ptr(), buf.as_mut_ptr()) };
        if rc == 0 {
            break;
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
    // SAFETY: `statvfs` вернул 0 — структура заполнена.
    #[allow(unsafe_code)]
    let st = unsafe { buf.assume_init() };
    // Счётчики блоков — в единицах `f_frsize` (POSIX).
    Ok(FsUsage {
        total: st.f_blocks.saturating_mul(st.f_frsize),
        available: st.f_bavail.saturating_mul(st.f_frsize),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_dir_available_within_total() {
        let dir = std::env::temp_dir().join(format!("tp-sys-fs-usage-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("каталог");
        let usage = fs_usage(&dir);
        std::fs::remove_dir(&dir).expect("удаление");
        let usage = usage.expect("statvfs");
        assert!(usage.total > 0);
        assert!(usage.available <= usage.total, "{usage:?}");
    }

    #[test]
    fn missing_path_is_error() {
        let err = fs_usage(Path::new("/nonexistent-tp-sys/fs-usage")).expect_err("нет пути");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn nul_in_path_is_invalid_input() {
        let err = fs_usage(Path::new("/tmp/a\0b")).expect_err("нулевой байт");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}
