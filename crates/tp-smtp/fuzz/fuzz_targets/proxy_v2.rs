//! Заголовок PROXY protocol v2 и команды smtp-in за ним по соединению loopback со слушателем `tp-platform`.
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| tp_smtp::fuzz::proxy_v2(data));
