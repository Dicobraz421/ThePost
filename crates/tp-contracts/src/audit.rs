//! Аудит доступа к ящику (CONTRACTS.md#MailboxAudit; SRS 5.2, решение 2026-11-04-exchange-parity; CTR-35).
//!
//! Действия не владельца (представитель, участник общего ящика, администратор) — вход, чтение, поиск по тексту,
//! перемещение, удаление, отправка от имени, изменение прав — записываются по умолчанию; действия владельца (удаление,
//! перемещение, отправка, изменение правил и пересылки) — по политике [`AuditPolicy`]; вход, чтение и поиск владельцем
//! не записываются никогда. Протоколы (`tp-imap`, `tp-jmap`, `tp-smtp`, Admin API) вызывают [`MailboxAudit::record_audit`]
//! для каждого действия, политику применяет хранилище. Журнал — только добавление и поиск; выгрузка в SIEM —
//! Admin API. Реализация — `tp-meta` (META-33), in-memory — `tp-testkit`.

use std::collections::BTreeSet;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::error::{Error, Result};
use crate::ids::{AccountId, MailboxId, MessageId, TenantId};
use crate::meta::AccountScope;

/// Наибольшая длина текстовых полей записи (`actor`, `client`, `object.detail`), символов.
pub const MAX_AUDIT_TEXT_LEN: usize = 1000;

/// Кто выполнил действие относительно ящика.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuditActorKind {
    /// Владелец ящика.
    Owner,
    /// Представитель или участник общего ящика (права ACL, Send As, Send on Behalf).
    Delegate,
    /// Администратор (Admin API).
    Admin,
}

/// Действие с ящиком.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditAction {
    /// Вход в ящик (открытие сеанса доступа к чужому ящику, IMAP `SELECT` общего ящика).
    Login,
    /// Чтение письма.
    Read,
    /// Поиск по тексту писем (решение 114): IMAP `SEARCH`/`UID SEARCH` с `BODY`/`TEXT`, JMAP `Email/query` с фильтром
    /// `text`/`body`. Строка запроса — в [`AuditObject::detail`] (до [`MAX_AUDIT_TEXT_LEN`] символов без управляющих),
    /// папка — в [`AuditObject::mailbox`]; найденные письма и их содержимое не записываются. Поиск владельцем в своём
    /// ящике не записывается (не входит в [`AuditAction::OWNER`]).
    Search,
    /// Перемещение писем между папками.
    Move,
    /// Удаление писем (в корзину или окончательно).
    Delete,
    /// Отправка от своего имени (владелец).
    Send,
    /// Отправка от имени ящика (Send As).
    SendAs,
    /// Отправка по поручению (Send on Behalf).
    SendOnBehalf,
    /// Изменение прав (ACL папок, делегирование).
    ChangeRights,
    /// Изменение серверных правил (Sieve).
    ChangeRules,
    /// Изменение пересылки.
    ChangeForwarding,
}

impl AuditAction {
    /// Действия владельца, которые политика может включить (вход, чтение и поиск владельцем не записываются).
    pub const OWNER: [AuditAction; 5] = [
        AuditAction::Delete,
        AuditAction::Move,
        AuditAction::Send,
        AuditAction::ChangeRules,
        AuditAction::ChangeForwarding,
    ];
}

/// Объект действия: папка, письмо, пояснение (адрес пересылки, изменённое право).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AuditObject {
    /// Папка.
    pub mailbox: Option<MailboxId>,
    /// Письмо.
    pub message: Option<MessageId>,
    /// Пояснение: Message-ID, адрес, право, строка запроса поиска ([`AuditAction::Search`]); содержимое писем не
    /// записывается.
    pub detail: Option<String>,
}

/// Запись аудита ящика.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntry {
    /// Ящик (организация и учётная запись владельца).
    pub scope: AccountScope,
    /// Момент действия (UTC).
    pub at: OffsetDateTime,
    /// Кто: имя входа учётной записи или субъект OIDC администратора (непусто).
    pub actor: String,
    /// Учётная запись исполнителя; `None` — администратор.
    pub actor_account: Option<AccountId>,
    /// Кто относительно ящика.
    pub actor_kind: AuditActorKind,
    /// Действие.
    pub action: AuditAction,
    /// Объект.
    pub object: AuditObject,
    /// Клиент: протокол и адрес (`imap 192.0.2.1`); `None` — не известен.
    pub client: Option<String>,
}

fn plain(s: &str) -> bool {
    s.chars().count() <= MAX_AUDIT_TEXT_LEN && !s.chars().any(char::is_control)
}

impl AuditEntry {
    /// Проверка: `actor` непуст; текстовые поля — до [`MAX_AUDIT_TEXT_LEN`] символов без управляющих; у владельца
    /// `actor_account` — учётная запись ящика. Иначе — `Error::Invalid`.
    pub fn validate(&self) -> Result<()> {
        if self.actor.is_empty() || !plain(&self.actor) {
            return Err(Error::invalid(
                "audit: actor must be 1–1000 plain characters",
            ));
        }
        if !self.client.as_deref().is_none_or(plain)
            || !self.object.detail.as_deref().is_none_or(plain)
        {
            return Err(Error::invalid(
                "audit: text fields must be plain, up to 1000 characters",
            ));
        }
        if self.actor_kind == AuditActorKind::Owner
            && self.actor_account != Some(self.scope.account)
        {
            return Err(Error::invalid("audit: owner must be the mailbox account"));
        }
        Ok(())
    }
}

/// Политика аудита ящика. По умолчанию — действия не владельца записываются, действия владельца — нет.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditPolicy {
    /// Записывать действия не владельца (представители, администраторы).
    pub non_owner: bool,
    /// Действия владельца, которые записываются (из [`AuditAction::OWNER`]; чтение и вход владельца не записываются).
    pub owner: BTreeSet<AuditAction>,
}

impl Default for AuditPolicy {
    fn default() -> Self {
        AuditPolicy {
            non_owner: true,
            owner: BTreeSet::new(),
        }
    }
}

impl AuditPolicy {
    /// Политика «записывать всё»: не владелец и все действия владельца из [`AuditAction::OWNER`].
    pub fn all() -> Self {
        AuditPolicy {
            non_owner: true,
            owner: AuditAction::OWNER.into_iter().collect(),
        }
    }

    /// Проверка: в `owner` — только действия из [`AuditAction::OWNER`], иначе `Error::Invalid`.
    pub fn validate(&self) -> Result<()> {
        if self.owner.iter().all(|a| AuditAction::OWNER.contains(a)) {
            Ok(())
        } else {
            Err(Error::invalid(
                "audit policy: owner actions are delete, move, send, change_rules, change_forwarding",
            ))
        }
    }

    /// Записывается ли действие: владелец — только действия из `owner`, входящие в [`AuditAction::OWNER`] (вход,
    /// чтение и поиск — никогда); не владелец — `non_owner`.
    pub fn records(&self, kind: AuditActorKind, action: AuditAction) -> bool {
        match kind {
            AuditActorKind::Owner => {
                AuditAction::OWNER.contains(&action) && self.owner.contains(&action)
            }
            AuditActorKind::Delegate | AuditActorKind::Admin => self.non_owner,
        }
    }
}

/// Позиция продолжения поиска; непрозрачна, действительна для того же запроса.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AuditCursor(pub String);

/// Запрос к журналу аудита. Условия объединяются по «и»; `None` — без условия.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditQuery {
    /// Ящик.
    pub account: Option<AccountId>,
    /// Исполнитель (`actor`, точное совпадение без учёта регистра ASCII).
    pub actor: Option<String>,
    /// Кто относительно ящика.
    pub actor_kind: Option<AuditActorKind>,
    /// Действие.
    pub action: Option<AuditAction>,
    /// Начало интервала, включительно.
    pub since: Option<OffsetDateTime>,
    /// Конец интервала, не включительно.
    pub until: Option<OffsetDateTime>,
    /// Не более записей на страницу, ≥ 1.
    pub limit: usize,
    /// Продолжение после страницы с этой позицией.
    pub after: Option<AuditCursor>,
}

impl AuditQuery {
    /// Запрос без условий на `limit` записей.
    pub fn all(limit: usize) -> Self {
        AuditQuery {
            account: None,
            actor: None,
            actor_kind: None,
            action: None,
            since: None,
            until: None,
            limit,
            after: None,
        }
    }

    /// Проверка: `limit` ≥ 1, `since < until`; иначе `Error::Invalid`.
    pub fn validate(&self) -> Result<()> {
        if self.limit == 0 {
            return Err(Error::invalid("audit query limit must be positive"));
        }
        if let (Some(since), Some(until)) = (self.since, self.until)
            && since >= until
        {
            return Err(Error::invalid("audit query interval is empty"));
        }
        Ok(())
    }

    /// Запись удовлетворяет условиям (кроме организации и позиции).
    pub fn matches(&self, e: &AuditEntry) -> bool {
        self.account.is_none_or(|a| a == e.scope.account)
            && self
                .actor
                .as_deref()
                .is_none_or(|a| a.eq_ignore_ascii_case(&e.actor))
            && self.actor_kind.is_none_or(|k| k == e.actor_kind)
            && self.action.is_none_or(|a| a == e.action)
            && self.since.is_none_or(|t| e.at >= t)
            && self.until.is_none_or(|t| e.at < t)
    }
}

/// Страница результатов поиска.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditPage {
    /// Записи по возрастанию `at`; равные моменты — в порядке записи.
    pub entries: Vec<AuditEntry>,
    /// Позиция следующей страницы; `None` — записей больше нет.
    pub next: Option<AuditCursor>,
}

/// Журнал аудита ящиков: только добавление и поиск. Политика — организации и ящика (ящик приоритетнее).
#[async_trait]
pub trait MailboxAudit: Send + Sync + 'static {
    /// Записывает действие, если его требует политика ящика [`MailboxAudit::audit_policy`] ([`AuditPolicy::records`]);
    /// `true` — записано. Некорректная запись — `Error::Invalid` ([`AuditEntry::validate`]).
    async fn record_audit(&self, entry: AuditEntry) -> Result<bool>;

    /// Действующая политика ящика: заданная для ящика, иначе для организации, иначе [`AuditPolicy::default`].
    async fn audit_policy(&self, scope: AccountScope) -> Result<AuditPolicy>;

    /// Задаёт политику организации (`account = None`) или ящика; `None` — сбросить уровень.
    /// Некорректная политика — `Error::Invalid` ([`AuditPolicy::validate`]).
    async fn set_audit_policy(
        &self,
        tenant: TenantId,
        account: Option<AccountId>,
        policy: Option<AuditPolicy>,
    ) -> Result<()>;

    /// Поиск в организации `tenant` ([`AuditQuery::matches`]); некорректный запрос или позиция — `Error::Invalid`.
    async fn search_audit(&self, tenant: TenantId, query: &AuditQuery) -> Result<AuditPage>;

    /// Удаляет записи старше `before` по всем организациям (роль maintenance; срок хранения — настройка
    /// реализации). Реализация может удалять целыми сутками: записи суток, на которые приходится `before`, могут остаться.
    async fn purge_audit(&self, before: OffsetDateTime) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_safe() {
        fn _audit(_: std::sync::Arc<dyn MailboxAudit>) {}
    }

    fn entry(kind: AuditActorKind, action: AuditAction) -> AuditEntry {
        let scope = AccountScope {
            tenant: TenantId::DEFAULT,
            account: AccountId::new(),
        };
        AuditEntry {
            scope,
            at: OffsetDateTime::UNIX_EPOCH,
            actor: "alice@example.org".into(),
            actor_account: (kind == AuditActorKind::Owner).then_some(scope.account),
            actor_kind: kind,
            action,
            object: AuditObject::default(),
            client: Some("imap 192.0.2.1".into()),
        }
    }

    #[test]
    fn policy_records() {
        let p = AuditPolicy::default();
        assert!(p.records(AuditActorKind::Delegate, AuditAction::Read));
        assert!(p.records(AuditActorKind::Admin, AuditAction::Login));
        assert!(!p.records(AuditActorKind::Owner, AuditAction::Delete));
        let all = AuditPolicy::all();
        assert!(all.records(AuditActorKind::Owner, AuditAction::Delete));
        assert!(all.records(AuditActorKind::Owner, AuditAction::ChangeForwarding));
        assert!(!all.records(AuditActorKind::Owner, AuditAction::Read));
        assert!(!all.records(AuditActorKind::Owner, AuditAction::Login));
        assert!(!all.records(AuditActorKind::Owner, AuditAction::Search));
        assert!(p.records(AuditActorKind::Delegate, AuditAction::Search));
        let searching = AuditPolicy {
            non_owner: true,
            owner: [AuditAction::Search].into_iter().collect(),
        };
        assert!(matches!(searching.validate(), Err(Error::Invalid(_))));
        assert!(!searching.records(AuditActorKind::Owner, AuditAction::Search));
        let quiet = AuditPolicy {
            non_owner: false,
            owner: BTreeSet::new(),
        };
        assert!(!quiet.records(AuditActorKind::Delegate, AuditAction::Delete));
        let bad = AuditPolicy {
            non_owner: true,
            owner: [AuditAction::Read].into_iter().collect(),
        };
        assert!(matches!(bad.validate(), Err(Error::Invalid(_))));
        assert!(all.validate().is_ok());
    }

    #[test]
    fn entry_validate_and_query() {
        let e = entry(AuditActorKind::Owner, AuditAction::Delete);
        assert!(e.validate().is_ok());
        let mut foreign = e.clone();
        foreign.actor_account = Some(AccountId::new());
        assert!(foreign.validate().is_err());
        let mut empty = e.clone();
        empty.actor.clear();
        assert!(empty.validate().is_err());
        let mut ctl = e.clone();
        ctl.client = Some("imap\n".into());
        assert!(ctl.validate().is_err());

        let mut q = AuditQuery::all(10);
        assert!(q.matches(&e));
        q.actor = Some("ALICE@example.org".into());
        q.action = Some(AuditAction::Delete);
        assert!(q.matches(&e));
        q.actor_kind = Some(AuditActorKind::Delegate);
        assert!(!q.matches(&e));
        assert!(AuditQuery::all(0).validate().is_err());
        let s = serde_json::to_string(&e).expect("serialize");
        assert_eq!(serde_json::from_str::<AuditEntry>(&s).expect("parse"), e);
        assert_eq!(
            serde_json::to_string(&AuditAction::SendOnBehalf).expect("serialize"),
            "\"send_on_behalf\""
        );
        assert_eq!(
            serde_json::to_string(&AuditAction::Search).expect("serialize"),
            "\"search\""
        );
    }
}
