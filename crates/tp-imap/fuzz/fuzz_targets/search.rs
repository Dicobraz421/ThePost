//! Критерии SEARCH и SEARCH RETURN на письмах-образцах: NOT обращает критерий, ALL — найденные номера.
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| tp_imap::fuzz::search(data));
