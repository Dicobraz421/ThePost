//! Текст скрипта Sieve: разбор и проверка заканчиваются программой или ошибками с позициями, без паники.
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| tp_sieve::fuzz::parse(data));
