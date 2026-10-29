# PLT-06: Слушатели TCP: PROXY v2 и TLS

lane: platform
deps: [SYS-03]
class: A
files: [crates/tp-platform/src/listen.rs, crates/tp-platform/src/tls.rs]
read: [docs/spec/S6.md#6.1, docs/spec/S6.md#6.2]
checks: [nextest]
manual: no

## Сделать
- Приём соединений, разбор PROXY v2 (ppp) по настройке, адрес клиента в контексте; rustls-конфиг из файлов
- с перечитыванием без перезапуска; неявный TLS и хелпер STARTTLS (апгрейд потока); TLS 1.3 по умолчанию, 1.2 — опция.

## Приёмка
- Тесты: PROXY v2 заголовок даёт адрес клиента; STARTTLS-апгрейд с самоподписанным сертификатом.
