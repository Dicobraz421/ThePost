//! Литералы и нарезка потока: разбор не зависит от кусков, байты литерала не исполняются как команда.
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| tp_imap::fuzz::literals(data));
