//! Корзина и квоты (CONTRACTS.md#Trash, Retention, Quota (Ф1); SRS 4.7, 3.8).
//!
//! Удалённое пользователем попадает в корзину (`MessageStore::to_trash`, `MailboxStore::delete`) и хранится
//! срок из политики ([`Policy::trash_retention`](crate::meta::Policy), [`PolicyResolver`](crate::meta::PolicyResolver);
//! не задан — значение по умолчанию конфигурации хранилища, 30 суток). Срок вычисляет хранилище при переносе
//! в корзину и фиксирует в элементе (`expires_at`): изменение политики действует на элементы, удалённые после него.
//!
//! Истечение (роль maintenance, `tp-mailstore`): [`TrashStore::expired`] → точка принятия решений
//! `DecisionHooks::before_final_delete` → `MessageStore::expunge_final`; отказ хука — [`TrashStore::defer`].
//! `expunge_final` удаляет только письма, которые всё ещё в корзине: восстановленное после `expired` остаётся.
//! Политики хранения и Legal Hold — модуль «Соответствие требованиям» через этот хук (SRS 1.3).
//!
//! «Очистить корзину» в клиенте (IMAP EXPUNGE в корзине, JMAP `Email/set destroy`; решение 2026-11-01-trash-purge) —
//! [`TrashStore::purge`] по настройке `trash.purge` ([`TrashPurge`], [`TrashPurgeRule`],
//! [`PolicyResolver::trash_purge`](crate::meta::PolicyResolver::trash_purge)): `hide` — письмо скрывается и хранится
//! до конца срока корзины, `erase` — стирается сразу; Legal Hold (запрет `before_final_delete`) превращает `erase` в `hide`.
//! Скрытое письмо не видно пользователю: его нет в `MessageStore` (`get`, `list`, операции над письмами — как над
//! отсутствующим), в счётчиках папки, в квоте (`Quota`, `AccountStore::quota`); `ChangeLog` показывает его удалённым
//! из корзины, событие ядра — `Trashed` (индексатор поиска по текущему состоянию удаляет документ). Видит и восстанавливает
//! его администратор: [`TrashStore::list_hidden`], [`TrashStore::restore`]. Срок корзины для скрытых истекает как обычно
//! (`expired` → `before_final_delete` → `expunge_final`).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::error::{Error, Result};
use crate::ext::DecisionHooks;
use crate::ids::{MailboxId, MessageId, ModSeq};
use crate::meta::{AccountScope, Inserted, QuotaUsage};

/// Объект в корзине. Ф1 — письма; события и контакты добавятся с календарями.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum TrashObject {
    /// Письмо (корзина сохраняет `MessageId`).
    Message(MessageId),
}

/// Элемент корзины.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrashItem {
    /// Учётная запись.
    pub scope: AccountScope,
    /// Объект.
    pub object: TrashObject,
    /// Исходная папка (могла быть удалена — тогда восстановление во «Входящие»); `FinalDelete::mailbox`.
    pub origin: MailboxId,
    /// Размер, байт.
    pub size: u64,
    /// Момент приёма письма (политики хранения).
    pub received_at: OffsetDateTime,
    /// Момент удаления в корзину.
    pub deleted_at: OffsetDateTime,
    /// Окончательное удаление не раньше этого момента.
    pub expires_at: OffsetDateTime,
}

/// Результат восстановления из корзины.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Restored {
    /// Письмо: прежний `MessageId`, папка и новый UID.
    Message {
        /// Папка, куда восстановлено.
        mailbox: MailboxId,
        /// Идентификатор, UID, `ModSeq`.
        inserted: Inserted,
    },
}

/// Значение настройки `trash.purge`: что делает «очистить корзину» в клиенте.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrashPurge {
    /// Скрыть: письмо исчезает у пользователя, хранится до конца срока корзины, восстанавливает администратор.
    #[default]
    Hide,
    /// Стереть сразу (под Legal Hold — как `Hide`).
    Erase,
}

/// Правило администратора для `trash.purge` ([`Policy::trash_purge`](crate::meta::Policy), уровни цепочки политик).
/// Не задано ни на одном уровне — `TrashPurgeRule::default()`: `hide`, выбор пользователю не дан.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct TrashPurgeRule {
    /// Значение администратора.
    pub mode: TrashPurge,
    /// Пользователь может выбрать своё значение; иначе действует `mode`.
    pub user_can_choose: bool,
}

/// Настройка `trash.purge` учётной записи ([`PolicyResolver::trash_purge`](crate::meta::PolicyResolver::trash_purge)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TrashPurgeSetting {
    /// Действующее правило администратора (по цепочке политик).
    pub rule: TrashPurgeRule,
    /// Выбор пользователя; хранится и при `user_can_choose = false`, но тогда не действует.
    pub user: Option<TrashPurge>,
}

impl TrashPurgeSetting {
    /// Действующее значение: выбор пользователя, если правило его разрешает, иначе значение администратора.
    pub fn effective(&self) -> TrashPurge {
        match self.user {
            Some(mode) if self.rule.user_can_choose => mode,
            _ => self.rule.mode,
        }
    }
}

/// Результат [`TrashStore::purge`]. Письма вне корзины учётной записи (в том числе уже скрытые) не входят.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PurgeOutcome {
    /// Скрыты (`hide`; при `erase` — Legal Hold или ошибка точки принятия решений).
    pub hidden: Vec<MessageId>,
    /// Стёрты.
    pub erased: Vec<MessageId>,
    /// `ModSeq` изменения (ничего не изменено — текущий).
    pub modseq: ModSeq,
}

/// Корзина. Реализация — `tp-meta`; in-memory — `tp-testkit`.
#[async_trait]
pub trait TrashStore: Send + Sync + 'static {
    /// Видимые пользователю элементы корзины учётной записи (без скрытых), новые первыми, со смещения `offset`,
    /// не более `limit` (Admin API; JMAP видит корзину как папку).
    async fn list(
        &self,
        scope: AccountScope,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<TrashItem>>;

    /// Скрытые элементы корзины учётной записи ([`TrashStore::purge`] с `hide`), новые первыми (по `deleted_at`),
    /// со смещения `offset`, не более `limit` (Admin API: «восстанавливаемые элементы»).
    async fn list_hidden(
        &self,
        scope: AccountScope,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<TrashItem>>;

    /// Восстанавливает элемент, видимый или скрытый (для письма — как `MessageStore::restore`: в исходную папку,
    /// её нет — во «Входящие»; прежний `MessageId`, новый UID). Элемента нет — `Error::NotFound`.
    async fn restore(&self, scope: AccountScope, object: TrashObject) -> Result<Restored>;

    /// «Очистить корзину» пользователем: письма `ids`, видимые в корзине учётной записи, — по действующей настройке
    /// (`PolicyResolver::trash_purge`). `Hide` — скрыть. `Erase` — для каждого письма `hooks.before_final_delete`
    /// (`FinalDelete::mailbox` — исходная папка) вне транзакции БД: `Allow` — стереть (как `MessageStore::expunge_final`),
    /// `Deny` (Legal Hold, политика хранения) или ошибка хука — скрыть: стирание необратимо, при сомнении письмо
    /// сохраняется. Отсутствующие, лежащие вне корзины и уже скрытые письма пропускаются без ошибки.
    async fn purge(
        &self,
        scope: AccountScope,
        ids: &[MessageId],
        hooks: &DecisionHooks,
    ) -> Result<PurgeOutcome>;

    /// Элементы всех организаций с `expires_at <= now`, включая скрытые, по возрастанию `expires_at`,
    /// не более `limit` (системный вызов роли maintenance).
    async fn expired(&self, now: OffsetDateTime, limit: usize) -> Result<Vec<TrashItem>>;

    /// Откладывает окончательное удаление элемента (видимого или скрытого) до `until` (хук запретил удаление:
    /// Legal Hold, политика хранения). Элемента нет — `Error::NotFound`.
    async fn defer(
        &self,
        scope: AccountScope,
        object: TrashObject,
        until: OffsetDateTime,
    ) -> Result<()>;
}

/// Квота учётной записи: использование (письма, включая корзину, без скрытых) и лимит из политики
/// ([`Policy::quota_bytes`](crate::meta::Policy); не задан — значение по умолчанию конфигурации, `None` — без лимита).
/// Реализация — `tp-meta`; in-memory — `tp-testkit`. Проверяют `ingest`, `copy`, `APPEND`, JMAP `Email/import`.
#[async_trait]
pub trait Quota: Send + Sync + 'static {
    /// Использование и лимит.
    async fn usage(&self, scope: AccountScope) -> Result<QuotaUsage>;

    /// Можно добавить `bytes`, иначе `Error::QuotaExceeded`.
    async fn check(&self, scope: AccountScope, bytes: u64) -> Result<()> {
        if self.usage(scope).await?.allows(bytes) {
            Ok(())
        } else {
            Err(Error::QuotaExceeded)
        }
    }
}

impl QuotaUsage {
    /// После добавления `bytes` занято не больше лимита.
    pub fn allows(&self, bytes: u64) -> bool {
        self.limit_bytes
            .is_none_or(|limit| self.used_bytes.saturating_add(bytes) <= limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{AccountId, TenantId};
    use std::sync::Arc;

    #[test]
    fn object_safe() {
        fn _trash(_: Arc<dyn TrashStore>) {}
        fn _quota(_: Arc<dyn Quota>) {}
    }

    #[test]
    fn quota_allows() {
        let mut u = QuotaUsage {
            used_bytes: 90,
            messages: 3,
            limit_bytes: None,
        };
        assert!(u.allows(u64::MAX));
        u.limit_bytes = Some(100);
        assert!(u.allows(10));
        assert!(!u.allows(11));
        assert!(!u.allows(u64::MAX));
    }

    #[test]
    fn trash_purge_effective() {
        let s = |mode, user_can_choose, user| TrashPurgeSetting {
            rule: TrashPurgeRule {
                mode,
                user_can_choose,
            },
            user,
        };
        assert_eq!(TrashPurgeSetting::default().effective(), TrashPurge::Hide);
        // Без `user_can_choose` выбор пользователя не действует: приоритет администратора.
        assert_eq!(
            s(TrashPurge::Hide, false, Some(TrashPurge::Erase)).effective(),
            TrashPurge::Hide
        );
        assert_eq!(
            s(TrashPurge::Erase, false, Some(TrashPurge::Hide)).effective(),
            TrashPurge::Erase
        );
        assert_eq!(
            s(TrashPurge::Hide, true, Some(TrashPurge::Erase)).effective(),
            TrashPurge::Erase
        );
        assert_eq!(
            s(TrashPurge::Erase, true, None).effective(),
            TrashPurge::Erase
        );
        assert_eq!(
            serde_json::to_string(&TrashPurge::Erase).expect("serialize"),
            r#""erase""#
        );
    }

    #[test]
    fn trash_item_serde() {
        let item = TrashItem {
            scope: AccountScope {
                tenant: TenantId::DEFAULT,
                account: AccountId::new(),
            },
            object: TrashObject::Message(MessageId::new()),
            origin: MailboxId::new(),
            size: 10,
            received_at: OffsetDateTime::UNIX_EPOCH,
            deleted_at: OffsetDateTime::UNIX_EPOCH,
            expires_at: OffsetDateTime::UNIX_EPOCH + time::Duration::days(30),
        };
        let s = serde_json::to_string(&item).expect("serialize");
        assert_eq!(serde_json::from_str::<TrashItem>(&s).expect("parse"), item);
    }
}
