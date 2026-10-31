//! Поток байтов клиента: нарезка, разбор команд (imap-codec и расширения IMAP4rev2), имена папок.
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| tp_imap::proto::fuzz_input(data));
