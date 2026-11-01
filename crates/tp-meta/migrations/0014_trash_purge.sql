-- «Очистить корзину» в клиенте: настройка trash.purge и скрытые письма (META-29, CTR-26;
-- решение 2026-11-01-trash-purge; SRS 4.7).

-- Скрытое письмо (TrashStore::purge с hide; erase под Legal Hold): остаётся в корзине (mailbox_id, UID, срок
-- trash_expires_at), но не видно пользователю — его нет в MessageStore, в счётчиках папки и в квоте.
-- Видит и восстанавливает его администратор (TrashStore::list_hidden, restore); срок истекает как обычно.
ALTER TABLE messages ADD COLUMN hidden_at timestamptz;
ALTER TABLE messages ADD CONSTRAINT messages_hidden_in_trash CHECK (hidden_at IS NULL OR trashed_at IS NOT NULL);
CREATE INDEX messages_hidden ON messages (tenant_id, account_id, trashed_at) WHERE hidden_at IS NOT NULL;

-- Правило администратора trash.purge на уровне цепочки политик: значение и разрешение выбора пользователю
-- (задаются вместе; NULL — не задано на этом уровне).
ALTER TABLE policies
    ADD COLUMN trash_purge text CHECK (trash_purge IN ('hide', 'erase')),
    ADD COLUMN trash_purge_user_can_choose boolean,
    ADD CONSTRAINT policies_trash_purge_pair
        CHECK ((trash_purge IS NULL) = (trash_purge_user_can_choose IS NULL));

-- Выбор пользователя trash.purge (PolicyResolver::set_user_trash_purge); действует только при user_can_choose
-- правила администратора. NULL — не выбран.
ALTER TABLE accounts ADD COLUMN trash_purge text CHECK (trash_purge IN ('hide', 'erase'));
