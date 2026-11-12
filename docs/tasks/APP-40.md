# APP-40: защита от перебора по адресу и подсети в узле

lane: app
deps: [CTR-57]
class: A
files: [crates/thepost/src/*]
read: [docs/spec/S6.md#6.3]
checks: [nextest]
manual: no

Хвост волны app-16, PR #293: `Guarded` передаёт в `AbuseGuard` только учётную запись (`Attempt::ip` — `None`).

## Сделать
- `Guarded` реализует `verify_from`: адрес — в `tp_auth::Attempt::ip` для `check`, `failure`, `success`; неудачи с
  несуществующими именами входа засчитываются адресу и подсети (SRS 6.3).

## Приёмка
- Тест сборки сервисов: N неудачных входов с одного адреса под разными несуществующими именами — адрес заблокирован,
  вход с другого адреса работает.
