//! Журнал аудита Admin API (CONTRACTS.md#AdminAuditLog; SRS 5.2, 6.8; CTR-25, решение 98).
//!
//! Каждое изменение администратора (запрос, изменяющий данные, включая отказ в доступе) и доступ к содержимому
//! писем — запись [`AdminAuditRecord`]. Журнал — только добавление: записи не меняются и не удаляются. Тело запроса
//! и секреты не записываются. Запись делает Admin API (`tp-admin`) — только в журнал.
//!
//! Выгрузка в SIEM (syslog, OTLP; решение владельца 12.11, APP-59) читает журнал ([`AuditLog::export_page`]) от
//! отметки доставки приёмника ([`AuditLog::export_mark`]) и сдвигает её после доставки
//! ([`AuditLog::advance_export_mark`]): недоступный приёмник догоняет пропущенное, записи не теряются. Реализация —
//! `tp-meta` (таблицы `audit_log`, `audit_export`; META-22, META-70, APP-59), in-memory — `tp-testkit`.

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

/// Запрос к журналу аудита организации: условия — по «и».
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminAuditQuery {
    /// Начало интервала по `at`, включительно.
    pub since: Option<OffsetDateTime>,
    /// Конец интервала по `at`, не включительно.
    pub until: Option<OffsetDateTime>,
    /// Администратор: имя входа (`actor_login`, без учёта регистра) или UUID учётной записи (`actor`).
    pub actor: Option<String>,
    /// Метод HTTP (`method`), точно.
    pub method: Option<String>,
    /// Начало шаблона маршрута (`route`): `/api/v1/orgs/{org}/transport` — все маршруты транспорта.
    pub route_prefix: Option<String>,
    /// Не более записей, ≥ 1.
    pub limit: usize,
    /// Курсор: записи с номером больше этого (`next` предыдущей страницы).
    pub after: Option<u64>,
}

impl AdminAuditQuery {
    /// Запрос без условий: первые `limit` записей.
    pub fn first(limit: usize) -> Self {
        Self {
            since: None,
            until: None,
            actor: None,
            method: None,
            route_prefix: None,
            limit,
            after: None,
        }
    }

    /// Проверяет запрос: `limit` ≥ 1, `since` раньше `until`, условия `actor`, `method`, `route_prefix` не пусты;
    /// иначе [`Error::Invalid`].
    pub fn validate(&self) -> Result<()> {
        if self.limit == 0 {
            return Err(Error::invalid("audit: limit must be at least 1"));
        }
        if let (Some(s), Some(u)) = (self.since, self.until)
            && s >= u
        {
            return Err(Error::invalid("audit: since must be before until"));
        }
        for (name, value) in [
            ("actor", &self.actor),
            ("method", &self.method),
            ("route_prefix", &self.route_prefix),
        ] {
            if value.as_deref().is_some_and(str::is_empty) {
                return Err(Error::invalid(format!("audit: {name} is empty")));
            }
        }
        Ok(())
    }

    /// Запись удовлетворяет условиям запроса по самой записи (интервал, администратор, метод, маршрут; без
    /// организации и курсора) — правило выборки для реализаций.
    pub fn matches(&self, r: &AdminAuditRecord) -> bool {
        self.since.is_none_or(|s| r.at >= s)
            && self.until.is_none_or(|u| r.at < u)
            && self.actor.as_deref().is_none_or(|a| {
                r.actor_login.to_lowercase() == a.to_lowercase() || r.actor.to_string() == a
            })
            && self.method.as_deref().is_none_or(|m| r.method == m)
            && self
                .route_prefix
                .as_deref()
                .is_none_or(|p| r.route.starts_with(p))
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

    /// Записи организации `tenant` (без уровня инсталляции и без чужих организаций) по возрастанию `seq`,
    /// удовлетворяющие условиям ([`AdminAuditQuery::matches`]: `at` в интервале [`since`, `until`), администратор,
    /// метод, начало маршрута), `seq` больше `after`, не более `limit`. Отбор — в хранилище: страница с условием
    /// полная, пока подходящие записи есть. Неверный запрос ([`AdminAuditQuery::validate`]) — [`Error::Invalid`].
    async fn search(&self, tenant: TenantId, query: &AdminAuditQuery) -> Result<AdminAuditPage>;

    /// Выгрузка в SIEM: записи всех организаций и уровня инсталляции с номером больше `after` по возрастанию
    /// `seq`, не более `limit` (≥ 1, иначе [`Error::Invalid`]). Видимые записи — непрерывный префикс порядка
    /// добавления: запись с меньшим номером не появляется после записи с бо́льшим, поэтому отметка «доставлено до
    /// N» ничего не пропускает.
    async fn export_page(&self, after: u64, limit: usize) -> Result<Vec<AdminAuditEntry>>;

    /// Отметка доставки приёмника выгрузки `sink` (`syslog`, `otlp`): номер последней доставленной записи; 0 —
    /// ничего не доставлено.
    async fn export_mark(&self, sink: &str) -> Result<u64>;

    /// Сдвигает отметку `sink` с `from` на `to` (`to` > `from`, иначе [`Error::Invalid`]), если она всё ещё
    /// `from`; `false` — отметку уже сдвинул другой выгружающий узел, ничего не изменено.
    async fn advance_export_mark(&self, sink: &str, from: u64, to: u64) -> Result<bool>;
}

/// Проверка аргументов [`AuditLog::advance_export_mark`] для реализаций.
pub fn check_export_mark(sink: &str, from: u64, to: u64) -> Result<()> {
    if sink.is_empty() {
        return Err(Error::invalid("audit export: sink is empty"));
    }
    if to <= from {
        return Err(Error::invalid("audit export: mark must move forward"));
    }
    Ok(())
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
            ..AdminAuditQuery::first(1)
        };
        assert_eq!(q.validate(), Ok(()));
        let empty = AdminAuditQuery {
            limit: 0,
            ..q.clone()
        };
        assert!(matches!(empty.validate(), Err(Error::Invalid(_))));
        let reversed = AdminAuditQuery {
            until: Some(now),
            ..q.clone()
        };
        assert!(matches!(reversed.validate(), Err(Error::Invalid(_))));
        let blank = [
            AdminAuditQuery {
                actor: Some(String::new()),
                ..q.clone()
            },
            AdminAuditQuery {
                method: Some(String::new()),
                ..q.clone()
            },
            AdminAuditQuery {
                route_prefix: Some(String::new()),
                ..q
            },
        ];
        for b in blank {
            assert!(matches!(b.validate(), Err(Error::Invalid(_))), "{b:?}");
        }
    }

    #[test]
    fn query_matches() {
        let r = AdminAuditRecord {
            at: OffsetDateTime::UNIX_EPOCH,
            tenant: Some(TenantId::DEFAULT),
            actor_tenant: TenantId::DEFAULT,
            actor: AccountId::new(),
            actor_login: "Admin@Example.org".into(),
            roles: Vec::new(),
            method: "PUT".into(),
            route: "/api/v1/orgs/{org}/transport/routes/{pattern}".into(),
            path: "/api/v1/orgs/x/transport/routes/*".into(),
            status: 200,
            content_messages: None,
        };
        let with = |f: &dyn Fn(&mut AdminAuditQuery)| {
            let mut q = AdminAuditQuery::first(1);
            f(&mut q);
            q.matches(&r)
        };
        assert!(with(&|_| {}));
        assert!(with(&|q| q.actor = Some("admin@example.ORG".into())));
        assert!(with(&|q| q.actor = Some(r.actor.to_string())));
        assert!(!with(&|q| q.actor = Some("admin".into())));
        assert!(with(&|q| q.method = Some("PUT".into())));
        assert!(!with(&|q| q.method = Some("DELETE".into())));
        assert!(with(
            &|q| q.route_prefix = Some("/api/v1/orgs/{org}/transport".into())
        ));
        assert!(!with(
            &|q| q.route_prefix = Some("/api/v1/orgs/{org}/accounts".into())
        ));
        assert!(!with(&|q| q.since = Some(r.at + time::Duration::SECOND)));
        assert!(!with(&|q| q.until = Some(r.at)));
    }

    #[test]
    fn export_mark_moves_forward() {
        assert_eq!(check_export_mark("syslog", 0, 1), Ok(()));
        for (sink, from, to) in [("", 0, 1), ("syslog", 3, 3), ("syslog", 3, 2)] {
            assert!(matches!(
                check_export_mark(sink, from, to),
                Err(Error::Invalid(_))
            ));
        }
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
