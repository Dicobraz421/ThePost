//! Параметры расширений ESMTP: первая строка — после `MAIL FROM:<…>`, следующие — после `RCPT TO:<…>`,
//! остаток — тело письма.
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| tp_smtp::fuzz::esmtp(data));
