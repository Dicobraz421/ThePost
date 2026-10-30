#!/bin/sh
# Роль приложения в PostgreSQL: без SUPERUSER и BYPASSRLS (иначе RLS не изолирует организации, doctor — Fail),
# владелец БД — миграции идут той же ролью. Подключение администратора — переменные PG* (psql).
# Пароль: TP_DB_PASSWORD_FILE (новый пароль от openbao-init; применяется и удаляется, нет файла — роль не меняется)
# или TP_DB_PASSWORD (профиль dev). Повторный запуск ничего не меняет.
set -eu

ROLE=${TP_DB_ROLE:-thepost}
DB=${TP_DB_NAME:-thepost}

if [ -n "${TP_DB_PASSWORD_FILE:-}" ]; then
  if [ ! -s "$TP_DB_PASSWORD_FILE" ]; then
    echo "postgres-role: нового пароля нет, роль $ROLE не меняется"
    exit 0
  fi
  pw=$(cat "$TP_DB_PASSWORD_FILE")
else
  pw=${TP_DB_PASSWORD:?TP_DB_PASSWORD или TP_DB_PASSWORD_FILE}
fi

for i in $(seq 30); do
  pg_isready -q && break
  sleep 1
done

psql -X -q -v ON_ERROR_STOP=1 -v role="$ROLE" -v db="$DB" -v pw="$pw" <<'SQL'
SELECT format('CREATE ROLE %I', :'role')
WHERE NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = :'role') \gexec
ALTER ROLE :"role" WITH LOGIN NOSUPERUSER NOBYPASSRLS NOCREATEDB NOCREATEROLE NOREPLICATION PASSWORD :'pw';
SELECT format('CREATE DATABASE %I OWNER %I', :'db', :'role')
WHERE NOT EXISTS (SELECT 1 FROM pg_database WHERE datname = :'db') \gexec
ALTER DATABASE :"db" OWNER TO :"role";
REVOKE ALL ON DATABASE :"db" FROM PUBLIC;
SQL

if [ -n "${TP_DB_PASSWORD_FILE:-}" ]; then rm -f "$TP_DB_PASSWORD_FILE"; fi
echo "postgres-role: роль $ROLE (без SUPERUSER и BYPASSRLS) — владелец БД $DB"
