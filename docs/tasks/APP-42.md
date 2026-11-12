# APP-42: CLI `thepost admin`: вход через OIDC device flow

lane: app
deps: [APP-11]
class: A
files: [crates/thepost/src/*]
read: [docs/spec/S5.md#5.2, docs/spec/S6.md]
checks: [nextest]
manual: no

Хвост волны app-07 (APP-11), PR #353: OIDC device flow не сделан — токен OIDC принимается только из переменной окружения.

Подтверждено владельцем — решение 79 (`docs/decisions/2026-11-11-stage2-followups.md`). Этап 2.

## Сделать
- `thepost admin login --oidc`: RFC 8628 — `device_authorization_endpoint` из `.well-known/openid-configuration`
  поставщика (`auth.oidc` в config.yaml или ключ CLI), код и адрес для пользователя, опрос токена с паузой `interval`;
  полученный токен — как сейчас (`THEPOST_ADMIN_TOKEN`/файл сессии CLI), без записи в историю оболочки.
- Без новых зависимостей (reqwest уже есть); если нужна библиотека OAuth — карточка полосы deps.

## Приёмка
- Тест на заглушке поставщика: `authorization_pending` → `slow_down` → токен; истёкший код — понятная ошибка.
