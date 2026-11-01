//! ENVELOPE и BODYSTRUCTURE: без паники, дерево частей в пределах MIME.
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| tp_mailstore::fuzz::meta(data));
