# DEP-06: бакет S3 и TLS в compose

lane: deploy
deps: [DEP-01]
class: A
files: [deploy/*]
read: [docs/decisions/2026-10-30-owner-answers.md#Кластер, CI, развёртывание, docs/decisions/2026-10-30-owner-answers.md#Шифрование любого обмена]
checks: [none]
manual: no

## Сделать
- Профиль dev создаёт бакет `thepost-test` при старте.
- Профиль по умолчанию: самоподписанный CA (скрипт генерации), PostgreSQL с ssl, Valkey с tls, S3 по https; профиль dev — открытые каналы с явным `allow_plaintext`.

## Приёмка
- `docker compose --profile dev up` → бакет есть; профиль по умолчанию принимает только TLS-подключения.
