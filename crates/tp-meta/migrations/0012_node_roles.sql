-- Раздельные учётные записи PostgreSQL (META-23; SRS 2.7, 3.1; решение 2026-11-01-borrowings-security).
--
-- Владелец схемы (роль, применяющая миграции) в работе узлов не используется. Узлы входят ролями с минимальными
-- правами — членами групповых ролей без входа:
--   thepost_app    — общая роль узлов: SELECT, INSERT, UPDATE, DELETE в таблицах ядра; в таблицах только
--                    на добавление (триггер tp_append_only: журналы доставки и аудита) — только SELECT и INSERT;
--   thepost_reader — только чтение метаданных для ролей search и indexer: SELECT, запись — только в служебные
--                    таблицы узла (членство, аренды, позиции подписчиков журнала событий, манифесты индексов).
-- Ни одна из ролей не владеет таблицами, не создаёт объекты в схеме и не имеет BYPASSRLS; таблица миграций им
-- недоступна. Учётные записи узлов создаёт администратор СУБД или database secrets engine, например:
--   CREATE ROLE thepost_node LOGIN PASSWORD '…' IN ROLE thepost_app;
-- Групповые роли общие для кластера СУБД: создаются, если их нет и у владельца схемы есть CREATEROLE, иначе —
-- предупреждение (их создаёт администратор СУБД, затем права выдаёт `SELECT tp_grant_roles()`).
-- Миграции, создающие таблицы, заканчиваются `SELECT tp_grant_roles();`.

DO $$
DECLARE
    r text;
BEGIN
    FOREACH r IN ARRAY ARRAY['thepost_app', 'thepost_reader']
    LOOP
        CONTINUE WHEN EXISTS (SELECT 1 FROM pg_roles WHERE rolname = r);
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = current_user AND (rolsuper OR rolcreaterole)) THEN
            BEGIN
                EXECUTE format('CREATE ROLE %I NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOBYPASSRLS', r);
            -- Роль создана параллельно (другая БД того же кластера).
            EXCEPTION WHEN duplicate_object OR unique_violation THEN
                NULL;
            END;
        ELSE
            RAISE WARNING 'роль % не создана: у % нет CREATEROLE; создайте её и выполните SELECT tp_grant_roles()',
                r, current_user;
        END IF;
    END LOOP;
END
$$;

-- Выдаёт права групповым ролям на таблицы схемы public (секции — только через родительскую таблицу).
CREATE FUNCTION tp_grant_roles() RETURNS void
LANGUAGE plpgsql SET search_path = public, pg_temp AS $$
DECLARE
    t record;
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'thepost_app') THEN
        GRANT USAGE ON SCHEMA public TO thepost_app;
        GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO thepost_app;
        GRANT EXECUTE ON FUNCTION tp_delivery_partitions(date[]), tp_delivery_purge(timestamptz) TO thepost_app;
    END IF;
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'thepost_reader') THEN
        GRANT USAGE ON SCHEMA public TO thepost_reader;
    END IF;
    FOR t IN
        SELECT c.relname,
               EXISTS (SELECT 1 FROM pg_trigger g WHERE g.tgrelid = c.oid AND g.tgname = 'tp_append_only')
                   AS append_only
        FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
        WHERE n.nspname = 'public' AND c.relkind IN ('r', 'p') AND NOT c.relispartition
          AND c.relname <> '_sqlx_migrations'
    LOOP
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'thepost_app') THEN
            EXECUTE format('REVOKE ALL ON public.%I FROM thepost_app', t.relname);
            IF t.append_only THEN
                EXECUTE format('GRANT SELECT, INSERT ON public.%I TO thepost_app', t.relname);
            ELSE
                EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON public.%I TO thepost_app', t.relname);
            END IF;
        END IF;
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'thepost_reader') THEN
            EXECUTE format('REVOKE ALL ON public.%I FROM thepost_reader', t.relname);
            IF t.relname IN ('nodes', 'leases', 'event_cursors', 'index_manifests') THEN
                EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON public.%I TO thepost_reader', t.relname);
            ELSE
                EXECUTE format('GRANT SELECT ON public.%I TO thepost_reader', t.relname);
            END IF;
        END IF;
    END LOOP;
END
$$;

REVOKE ALL ON FUNCTION tp_grant_roles() FROM PUBLIC;

SELECT tp_grant_roles();
