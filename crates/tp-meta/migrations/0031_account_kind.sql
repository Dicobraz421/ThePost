-- Вид учётной записи (CTR-48; CONTRACTS.md#Metadata, CONTRACTS.md#Sharing): пользователь или ресурс (переговорная,
-- оборудование). Задаётся записью учётной записи (AccountUpsert::kind).

ALTER TABLE accounts
    ADD COLUMN kind text NOT NULL DEFAULT 'user' CHECK (kind IN ('user', 'resource'));
