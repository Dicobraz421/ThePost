//! XML-тело запроса DAV: дерево в пределах лимитов, запросы из него разбираются без паники.
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| tp_dav::fuzz::xml(data));
