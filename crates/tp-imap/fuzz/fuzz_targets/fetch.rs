//! Атрибуты FETCH (до первого NUL) по письму (после него): разделы, BINARY, диапазоны, ответ FETCH.
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| tp_imap::fuzz::fetch(data));
