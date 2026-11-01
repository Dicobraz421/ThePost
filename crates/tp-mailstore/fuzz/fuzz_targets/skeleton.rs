//! Объект-скелет из произвольных байтов: ошибка или согласованные сегменты.
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| tp_mailstore::fuzz::skeleton(data));
