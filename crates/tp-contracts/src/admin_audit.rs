//! Журнал аудита Admin API (CONTRACTS.md#AdminAuditLog; SRS 5.2, 6.8; CTR-25, решение 98).
//!
//! Каждое изменение администратора (запрос, изменяющий данные, включая отказ в доступе) и доступ к содержимому
//! писем — запись [`AdminAuditRecord`]. Журнал — только добавление: записи не меняются и не удаляются. Тело запроса
//! и секреты не записываются. Запись делает Admin API (`tp-admin`), экспорт в SIEM (syslog, OTLP) — тоже он, поверх
//! записи в журнал. Реализация — `tp-meta` (таблица `audit_log`, META-22, META-70), in-memory — `tp-testkit`.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::error::{Error, Result};
use crate::ids::{AccountId, TenantId};

/// Запись журнала аудита Admin API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminAuditRecord {
    /// Момент (UTC).
    pub at: OffsetDateTime,
    /// Организация, к которой относится операция; `None` — уровень инсталляции (в чтение по организации не
    /// попадает).
    pub tenant: Option<TenantId>,
    /// Организация администратора.
    pub actor_tenant: TenantId,
    /// Учётная запись администратора.
    pub actor: AccountId,
    /// Имя входа администратора.
    pub actor_login: String,
    /// Роли администратора Admin API на момент операции (`installation_admin`, `organization_admin`,
    /// `operator`, `auditor`); пусто — ролей нет (отказ).
    pub roles: Vec<String>,
    /// Метод HTTP.
    pub method: String,
    /// Шаблон маршрута (`/api/v1/orgs/{org}/transport/domains`); неизвестный маршрут — путь.
    pub route: String,
    /// Путь запроса.
    pub path: String,
    /// Код ответа.
    pub status: u16,
    /// Доступ к содержимому писем: число писем, тема и отправитель которых запрошены; `None` — запрос содержимое
    /// не отдаёт. Самих тем в записи нет.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_messages: Option<u64>,
}

/// Запись журнала с номером.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminAuditEntry {
    /// Номер записи: ≥ 1, возрастает в порядке добавления, уникален в инсталляции.
    pub seq: u64,
    /// Запись.
    #[serde(flatten)]
    pub record: AdminAuditRecord,
}

/// Запрос к журналу аудита организации.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminAuditQuery {
    /// Начало интервала по `at`, включительно.
    pub since: Option<OffsetDateTime>,
    /// Конец интервала по `at`, не включительно.
    pub until: Option<OffsetDateTime>,
    /// Не более записей, ≥ 1.
    pub limit: usize,
    /// Курсор: записи с номером больше этого (`next` предыдущей страницы).
    pub after: Option<u64>,
}

impl AdminAuditQuery {
    /// Проверяет запрос: `limit` ≥ 1, `since` раньше `until`; иначе [`Error::Invalid`].
    pub fn validate(&self) -> Result<()> {
        if self.limit == 0 {
            return Err(Error::invalid("audit: limit must be at least 1"));
        }
        if let (Some(s), Some(u)) = (self.since, self.until)
            && s >= u
        {
            return Err(Error::invalid("audit: since must be before until"));
        }
        Ok(())
    }
}

/// Страница журнала аудита.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminAuditPage {
    /// Записи по возрастанию `seq`.
    pub entries: Vec<AdminAuditEntry>,
    /// Курсор следующей страницы (`seq` последней записи), если страница полная; `None` — записей больше нет.
    pub next: Option<u64>,
}

/// Журнал аудита Admin API: только добавление и чтение по организации.
#[async_trait]
pub trait AuditLog: Send + Sync + 'static {
    /// Добавляет запись; возвращает её номер.
    async fn append(&self, record: &AdminAuditRecord) -> Result<u64>;

    /// Записи организации `tenant` (без уровня инсталляции и без чужих организаций) по возрастанию `seq`:
    /// `at` в интервале [`since`, `until`), `seq` больше `after`, не более `limit`. Неверный запрос
    /// ([`AdminAuditQuery::validate`]) — [`Error::Invalid`].
    async fn search(&self, tenant: TenantId, query: &AdminAuditQuery) -> Result<AdminAuditPage>;
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn object_safe() {
        fn _l(_: Arc<dyn AuditLog>) {}
    }

    #[test]
    fn query_validate() {
        let now = OffsetDateTime::UNIX_EPOCH;
        let q = AdminAuditQuery {
            since: Some(now),
            until: Some(now + time::Duration::seconds(1)),
            limit: 1,
            after: None,
        };
        assert_eq!(q.validate(), Ok(()));
        let empty = AdminAuditQuery {
            limit: 0,
            ..q.clone()
        };
        assert!(matches!(empty.validate(), Err(Error::Invalid(_))));
        let reversed = AdminAuditQuery {
            until: Some(now),
            ..q
        };
        assert!(matches!(reversed.validate(), Err(Error::Invalid(_))));
    }

    #[test]
    fn entry_serde_flat() {
        let entry = AdminAuditEntry {
            seq: 7,
            record: AdminAuditRecord {
                at: OffsetDateTime::UNIX_EPOCH,
                tenant: Some(TenantId::DEFAULT),
                actor_tenant: TenantId::DEFAULT,
                actor: AccountId::new(),
                actor_login: "admin".into(),
                roles: vec!["organization_admin".into()],
                method: "PUT".into(),
                route: "/api/v1/orgs/{org}/policy".into(),
                path: "/api/v1/orgs/x/policy".into(),
                status: 200,
                content_messages: None,
            },
        };
        let v = serde_json::to_value(&entry).expect("ser");
        assert_eq!(v["seq"], 7);
        assert!(v.get("content_messages").is_none());
        let back: AdminAuditEntry = serde_json::from_value(v).expect("de");
        assert_eq!(back, entry);
    }
}
