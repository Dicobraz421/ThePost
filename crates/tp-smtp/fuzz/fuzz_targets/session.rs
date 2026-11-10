//! Поток команд и данных сессии SMTP (первый байт — режим: нечётный — submission): разбор команд, `DATA`,
//! `BDAT`, конвейер и лимиты; ответы — корректные строки SMTP.
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| tp_smtp::fuzz::session(data));
