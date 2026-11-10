//! Аргументы `Email/query` на письмах-образцах: `NOT` обращает фильтр, порядок сортировки, окно результатов.
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| tp_jmap::fuzz::query(data));
