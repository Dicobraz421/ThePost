# META-18: динамические учётные данные PostgreSQL — ручная проверка

Нужны: стенд DEP-07 (compose, профиль `smb`: OpenBao по TLS, PostgreSQL). Шаги с `thepost serve` — после того как
приложение (полоса app) создаёт пул узла через `tp_meta::pool::connect_rotating`; до этого — автотесты
(`cargo nextest run -p tp-meta credentials pg_credentials` с `TEST_DATABASE_URL`).

## Подготовка database secrets engine (root-токен из тома `openbao-admin`)
1. `bao secrets enable database`
2. `bao write database/config/thepost plugin_name=postgresql-database-plugin allowed_roles=thepost
   connection_url='postgresql://{{username}}:{{password}}@postgres:5432/thepost?sslmode=verify-full'
   username=<админ БД> password=<пароль>`
3. `bao write database/roles/thepost db_name=thepost default_ttl=2m max_ttl=6m
   creation_statements="CREATE ROLE \"{{name}}\" LOGIN PASSWORD '{{password}}' VALID UNTIL '{{expiration}}' IN ROLE thepost;"`
4. Политика узла: `path "database/creds/thepost" { capabilities = ["read"] }`.

## Ротация
1. В `config.yaml` вместо `database.password`:
   ```yaml
   database:
     url: postgres://postgres:5432/thepost
     dynamic_credentials:
       username: { secret: "database/creds/thepost#username" }
       password: { secret: "database/creds/thepost#password" }
   ```
2. `thepost config check` → без ошибок; с `password` и `dynamic_credentials` вместе → ошибка
   «задайте либо password, либо dynamic_credentials».
3. `thepost serve` → в `SELECT usename FROM pg_stat_activity WHERE application_name = 'thepost'` — роль `v-…`.
4. Через ~80 с (две трети `default_ttl`) — аренда продлена (`bao list sys/leases/lookup/database/creds/thepost` —
   прежняя аренда, срок обновлён); новых ролей нет.
5. Ближе к `max_ttl` (6 мин) — в журнале `database.dynamic_credentials: учётные данные обновлены`; новые соединения —
   с новой ролью `v-…`, почта работает (отправка и чтение письма во время смены), ошибок соединения в журнале нет.
6. Отозвать аренду: `bao lease revoke -prefix database/creds/thepost` → при следующем продлении — новые учётные
   данные, в журнале `учётные данные обновлены`.
7. Запечатать OpenBao на минуту → в журнале предупреждения «секрет недоступен» раз в `retry_interval`, почта работает
   на прежних соединениях; после распечатывания — продление или новые учётные данные.
8. Значений логина и пароля в журнале нет (`… logs thepost | grep -E 'v-[a-z0-9-]{8,}'` — только имена ролей в
   `pg_stat_activity`, не в журнале).
