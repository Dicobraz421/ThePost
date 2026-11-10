#!/bin/sh
# Учётные записи The Post в PostgreSQL (META-23, DEP-08; SRS 2.7, 3.1) — все без SUPERUSER, BYPASSRLS, CREATEDB,
# CREATEROLE, REPLICATION (с SUPERUSER или BYPASSRLS RLS не изолирует организации, doctor — Fail):
#   thepost_owner    — владелец БД и схемы public, только миграции (database.owner);
#   thepost          — вход узла (database.url), член thepost_app: DML без владения таблицами;
#   thepost_readonly — только чтение метаданных, член thepost_reader.
# Групповые роли thepost_app и thepost_reader (без входа) — права на таблицы им выдаёт миграция (tp_grant_roles).
# Подключение администратора — переменные PG* (psql). Пароли (нет — пароль роли не меняется):
#   TP_DB_PASSWORD_DIR — файлы db-password, db-owner-password, db-readonly-password (новые пароли от openbao-init;
#   применяются и удаляются) или TP_DB_PASSWORD, TP_DB_OWNER_PASSWORD, TP_DB_READONLY_PASSWORD (dev, вручную).
# Установка до DEP-08 (таблицы принадлежат thepost) переводится: владелец — thepost_owner. Повторный запуск ничего
# не меняет.
set -eu

DB=${TP_DB_NAME:-thepost}
APP=${TP_DB_ROLE:-thepost}
OWNER=${TP_DB_OWNER_ROLE:-thepost_owner}
READONLY=${TP_DB_READONLY_ROLE:-thepost_readonly}

# Пароль: файл <имя>-password в TP_DB_PASSWORD_DIR или значение переменной; пусто — не менять.
password() { # <имя файла> <значение переменной>
  if [ -n "${TP_DB_PASSWORD_DIR:-}" ]; then
    if [ -s "$TP_DB_PASSWORD_DIR/$1-password" ]; then cat "$TP_DB_PASSWORD_DIR/$1-password"; fi
  else
    printf %s "$2"
  fi
}
app_pw=$(password db "${TP_DB_PASSWORD:-}")
owner_pw=$(password db-owner "${TP_DB_OWNER_PASSWORD:-}")
readonly_pw=$(password db-readonly "${TP_DB_READONLY_PASSWORD:-}")

for i in $(seq 30); do
  pg_isready -q && break
  sleep 1
done

psql -X -q -v ON_ERROR_STOP=1 -v db="$DB" -v app="$APP" -v owner="$OWNER" -v readonly="$READONLY" \
  -v app_pw="$app_pw" -v owner_pw="$owner_pw" -v readonly_pw="$readonly_pw" <<'SQL'
SELECT format('CREATE ROLE %I', r)
FROM unnest(ARRAY['thepost_app', 'thepost_reader', :'owner', :'app', :'readonly']) AS r
WHERE NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = r) \gexec
ALTER ROLE thepost_app WITH NOLOGIN NOSUPERUSER NOBYPASSRLS NOCREATEDB NOCREATEROLE NOREPLICATION;
ALTER ROLE thepost_reader WITH NOLOGIN NOSUPERUSER NOBYPASSRLS NOCREATEDB NOCREATEROLE NOREPLICATION;
SELECT format('ALTER ROLE %I WITH LOGIN NOSUPERUSER NOBYPASSRLS NOCREATEDB NOCREATEROLE NOREPLICATION', r)
FROM unnest(ARRAY[:'owner', :'app', :'readonly']) AS r \gexec
SELECT format('ALTER ROLE %I PASSWORD %L', r, pw)
FROM (VALUES (:'owner', :'owner_pw'), (:'app', :'app_pw'), (:'readonly', :'readonly_pw')) AS p (r, pw)
WHERE pw <> '' \gexec
SELECT format('GRANT thepost_app TO %I', :'app')
WHERE NOT pg_has_role(:'app', 'thepost_app', 'MEMBER') \gexec
SELECT format('GRANT thepost_reader TO %I', :'readonly')
WHERE NOT pg_has_role(:'readonly', 'thepost_reader', 'MEMBER') \gexec
SELECT format('CREATE DATABASE %I OWNER %I', :'db', :'owner')
WHERE NOT EXISTS (SELECT 1 FROM pg_database WHERE datname = :'db') \gexec
ALTER DATABASE :"db" OWNER TO :"owner";
REVOKE ALL ON DATABASE :"db" FROM PUBLIC;
GRANT CONNECT ON DATABASE :"db" TO thepost_app, thepost_reader;
\connect :"db"
SELECT format('REASSIGN OWNED BY %I TO %I', :'app', :'owner')
WHERE EXISTS (SELECT 1 FROM pg_shdepend d JOIN pg_database b ON b.oid = d.dbid
              WHERE d.deptype = 'o' AND b.datname = current_database() AND d.refobjid = :'app'::regrole) \gexec
-- Права групповых ролей на существующие таблицы (новые выдают миграции); до первой миграции схемы нет.
DO $$ BEGIN IF to_regprocedure('public.tp_grant_roles()') IS NOT NULL THEN PERFORM public.tp_grant_roles(); END IF; END $$;
SQL

if [ -n "${TP_DB_PASSWORD_DIR:-}" ]; then
  rm -f "$TP_DB_PASSWORD_DIR/db-password" "$TP_DB_PASSWORD_DIR/db-owner-password" "$TP_DB_PASSWORD_DIR/db-readonly-password"
fi
echo "postgres-role: БД $DB — владелец $OWNER, узел $APP (thepost_app), только чтение $READONLY (thepost_reader)"
