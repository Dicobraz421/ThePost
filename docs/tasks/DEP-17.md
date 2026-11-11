# DEP-17: стенд — CalDAV, CardDAV, ManageSieve

lane: deploy
deps: [APP-25]
class: A
files: [deploy/*]
read: [docs/spec/S4.md#4.4, docs/spec/APP-D.md]
checks: [nextest]
manual: no

## Сделать
- Пример config.yaml и compose: роль `dav`, `sieve.managesieve` (порт 4190 наружу), DNS SRV/TXT `_caldavs._tcp`, `_carddavs._tcp`,
  `_sieve._tcp` в описании стенда и в подсказках `thepost doctor`.
- Наполнение стенда (DEP-12): по календарю и адресной книге у тестовых учётных записей.

## Приёмка
- `docker compose --profile smb up` — `/.well-known/caldav` отвечает, порт 4190 принимает `CAPABILITY`.
