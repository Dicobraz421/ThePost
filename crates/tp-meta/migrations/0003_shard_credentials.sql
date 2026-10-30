-- Учётные данные шардов из хранилища секретов (CTR-15, META-17): адрес без пароля, пароль — ссылка на секрет.

-- Ссылка на секрет `<путь>#<поле>` (SecretRef: сегменты [A-Za-z0-9_.-]+ без `.` и `..`).
ALTER TABLE shards ADD COLUMN credentials text
    CONSTRAINT shards_credentials_ref CHECK (
        credentials ~ '^[A-Za-z0-9_.-]+(/[A-Za-z0-9_.-]+)*#[A-Za-z0-9_.-]+$'
        AND credentials !~ '(^|/)\.{1,2}(/|#)'
        AND length(credentials) <= 512
    );

-- Пароль в адресе шарда (`user:password@` или параметр `password=`) запрещён. NOT VALID: прежние строки
-- не проверяются миграцией (старт не блокируется) — их отклоняет справочник при чтении, новые и
-- изменённые строки проверяет ограничение.
ALTER TABLE shards ADD CONSTRAINT shards_dsn_without_password CHECK (
    dsn IS NULL OR (
        dsn !~ '^[A-Za-z][A-Za-z0-9+.-]*://[^/?#@]*:[^/?#@]*@'
        AND dsn !~* '[?&]password='
    )
) NOT VALID;
