# ADM-23: тест: `AccountQuery` из строки запроса

lane: admin
deps: [ADM-13]
class: A
files: [crates/tp-admin/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

Хвост волны admin-08 (ADM-13), PR #375: обход `AccountQuery` через строку запроса не проверяется — проверяется только JSON.

## Сделать
- Тест через роутер tp-admin (`tower::ServiceExt::oneshot`, без новых зависимостей): `GET …/accounts?prefix=…&after=…&limit=…`
  и `?login=…` разбираются в `AccountQuery`; несовместимые параметры (`login` с `prefix`) — 422; кириллица и `+` в
  значениях кодируются и читаются без потерь.

## Приёмка
- Тест зелёный в CI.
