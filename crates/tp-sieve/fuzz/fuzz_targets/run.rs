//! Скрипт Sieve и письмо (через байт NUL): выполнение в пределах лимитов, итог правил согласован.
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| tp_sieve::fuzz::run(data));
