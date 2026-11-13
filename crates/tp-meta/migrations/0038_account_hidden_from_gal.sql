-- Признак «скрыта из книги «Организация»» у учётной записи (CTR-83; CONTRACTS.md#Metadata, CONTRACTS.md#Directory):
-- нет в GAL, вход и приём почты — как обычно. Задаётся записью учётной записи (AccountUpsert::hidden_from_gal);
-- каталог PostgreSQL (узел без LDAP, META-65) передаёт его в DirAccount::hidden_from_gal. Прежние учётные записи
-- видны в книге.

ALTER TABLE accounts
    ADD COLUMN hidden_from_gal boolean NOT NULL DEFAULT false;
