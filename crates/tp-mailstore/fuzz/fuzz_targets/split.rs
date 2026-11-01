//! Разбор письма на скелет и части: сборка обратно побайтова, пределы MIME соблюдаются.
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| tp_mailstore::fuzz::split(data));
