# JMAP-22: подпись профиля .mobileconfig сертификатом сервера

lane: jmap
deps: [JMAP-14, DEPS-03]
class: A
files: [crates/tp-jmap/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: docs/manual-tests/JMAP-22.md

Решение владельца 2026-11-11, п. 3.

## Сделать
- `/mail/profile.mobileconfig` подписывается CMS SignedData ключом и цепочкой TLS-сертификата сервера (секция `tls`);
  нет ключа — профиль без подписи и предупреждение в журнале.
- Ручная проверка: установка профиля на iPhone или Mac — «Проверено», имя организации.

## Приёмка
- Тест: подпись профиля проверяется цепочкой сертификата; снимок профиля без изменений содержимого.
