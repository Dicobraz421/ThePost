# AUTH-09: пароли приложений на контракте AppPasswordStore

lane: auth
deps: [CTR-22]
class: A
files: [crates/tp-auth/src/password.rs, crates/tp-auth/src/lib.rs]
read: [docs/CONTRACTS.md#Auth]
checks: [nextest]
manual: no

## Сделать
- `tp-auth` — на контракт `tp_contracts::app_password::AppPasswordStore` (CTR-22, PR #145): удалить свой трейт и `AppPassword`, тестовую `MemAppPasswords` заменить на `tp_testkit::MemAppPasswords`.
- Владелец — `AccountScope` вместо пары (организация, учётная запись); время — `OffsetDateTime` (`SystemTime::now().into()`).

## Приёмка
- Тесты паролей приложений (выпуск, предел, отзыв) проходят на `tp_testkit::MemAppPasswords`.
