//! Тело запроса API: объект Request, вызовы методов как `Core/echo`, подстановка ссылок на результаты
//! с бюджетом, сериализация ответа.
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| tp_jmap::fuzz::request(data));
