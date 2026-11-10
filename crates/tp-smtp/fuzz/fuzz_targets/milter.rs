//! Поток ответов фильтра Milter на всю переписку проверки письма.
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| tp_smtp::fuzz::milter(data));
