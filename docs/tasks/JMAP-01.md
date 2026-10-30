# JMAP-01: Ядро JMAP

lane: jmap
deps: [AUTH-03, PLT-06, KIT-05, CTR-10]
class: A
files: [crates/tp-jmap/src/core/*]
read: [docs/spec/S4.md#4.2]
checks: [nextest]
manual: no

## Сделать
- /.well-known/jmap, Session, Request/Response, ссылки на результаты, ошибки, лимиты (RFC 8620), Bearer и Basic (пароль приложения); реестр capability (модули).

## Приёмка
- Тесты по примерам RFC 8620.
