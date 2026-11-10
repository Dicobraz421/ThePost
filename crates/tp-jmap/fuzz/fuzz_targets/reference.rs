//! Ссылка на результат (до первого NUL — ответ JSON, после — выбор пути): JSON Pointer с `~0`/`~1` и `*`,
//! ошибки `invalidResultReference`.
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| tp_jmap::fuzz::reference(data));
