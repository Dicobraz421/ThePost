//! Поток байтов клиента: нарезка, разбор команд (imap-codec и расширения IMAP4rev2), имена папок;
//! команда imap-codec после сериализации разбирается в ту же.
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| tp_imap::fuzz::command(data));
