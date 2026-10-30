# PLT-12: клиент OpenBao/Vault и секреты в конфигурации — ручная проверка

Нужны: Docker с compose, клон репозитория. Профиль `smb` из `deploy/compose/dev.yml` (OpenBao по TLS, вход AppRole,
`compose/thepost-smb.yaml`). Шаги с `thepost serve`/`thepost doctor` — после подключения `VaultStore`, `doctor` и
`Reloader::with_secret_store` в приложении (полоса app); до этого проверяются автотесты и шаг «Автотест с OpenBao».

## Автотест с OpenBao
1. `docker compose -f deploy/compose/dev.yml --profile dev up -d --wait openbao openbao-init`
2. `TEST_OPENBAO_URL=http://localhost:8200 TEST_OPENBAO_TOKEN=thepost-dev-root cargo nextest run -p tp-platform openbao`
   → тест `secrets::tests::openbao_contract_suite` пройден (не пропущен); в OpenBao — mount `tp-kit`, AppRole `tp-kit-approle`.
3. С TLS (профиль `tls`): `TEST_OPENBAO_URL=https://localhost:8200 TEST_OPENBAO_CACERT=deploy/compose/tls/ca.pem`,
   `TEST_OPENBAO_TOKEN` — root-токен из тома `openbao-admin` (`init.txt`) → тест пройден.

## Старт и doctor (профиль smb)
1. `docker compose -f deploy/compose/dev.yml --profile smb up -d --build`
2. `docker compose -f deploy/compose/dev.yml --profile smb run --rm thepost doctor` → `secrets_config` Ok (TLS),
   `secrets_store` Ok (версия OpenBao), `secrets_seal` Warn (ключ распечатывания в файле), `secrets_login` Ok (approle),
   `secrets_token` Ok (`ttl 1h, renewable`); Fail нет.
3. В журнале `thepost` — `secret store login`, значений секретов и токенов в журнале нет
   (`docker compose … logs thepost | grep -i -E 'thepost-dev|s\.[A-Za-z0-9]{20}'` → пусто).

## Ротация секрета и перечитывание
1. Сменить пароль роли PostgreSQL и секрет `thepost/db` (root-токен из тома `openbao-admin`):
   `bao kv put -mount=thepost db username=thepost password=<новый>` и `ALTER ROLE thepost PASSWORD '<новый>'`.
2. `docker compose … kill -s HUP thepost` → в журнале `configuration reloaded`; новые соединения с PostgreSQL проходят.
3. Повторный SIGHUP без изменений → новой версии нет (`configuration reloaded` не появляется).

## Ошибки
1. Запечатать OpenBao (`bao operator seal`) и отправить SIGHUP → `configuration reload failed; previous version kept`,
   почта работает. `thepost doctor` → `secrets_store` Fail «store is sealed». Распечатать — следующий SIGHUP проходит.
2. В `config.yaml` задать `database.password: hunter2` → `thepost config check` и старт — ошибка
   `database.password: secret in plaintext is not allowed …`, значения `hunter2` в сообщении нет.
3. `database.url: postgres://thepost:pw@…` вне `profile: dev` → ошибка `password in the address …`.
4. `secrets.url: http://openbao-tls:8200` вне dev → ошибка `the channel to the secret store is TLS only`.
5. Испортить `secret_id` (`/etc/thepost/openbao/secret_id`) и перезапустить → старт с ошибкой
   `invalid role or secret ID`, `thepost doctor` → `secrets_login` Fail.
