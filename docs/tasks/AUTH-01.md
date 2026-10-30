# AUTH-01: Синхронизация LDAP

lane: auth
deps: [CTR-05]
class: A
files: [crates/tp-auth/src/ldap.rs]
read: [docs/spec/S5.md#5.1]
checks: [nextest]
manual: no

## Сделать
- Реализация Directory: LDAPS, инкрементальная выборка (modifyTimestamp / USN для AD), учётки, группы, алиасы, атрибуты GAL; маппинг полей в конфигурации.

## Приёмка
- Тесты на фикстурах LDIF; интеграционный при TEST_LDAP_URL.
