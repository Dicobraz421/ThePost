//! Правила Sieve: скрипты, автоответ, точка правил доставки (CTR-46; CONTRACTS.md#Sieve; SRS 4.3).
//!
//! Интерпретатор — свой крейт `tp-sieve` (sieve-rs под AGPL-3.0 не подходит, SRS 4.3): он проверяет текст скрипта
//! до [`SieveStore::put`] и реализует [`DeliveryRules`]; `tp-smtp` вызывает правила для каждого локального получателя.
//! ManageSieve (RFC 5804) работает в роли `imap` (как POP3), его секция конфигурации — `sieve` крейта `tp-sieve`;
//! роли не меняются.
//!
//! Порядок при доставке (реализация `tp-sieve`): скрипт организации ([`SieveStore::org_script`]), автоответ
//! ([`Vacation`]), активный скрипт пользователя.

use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::bus::{Event, EventKind};
use crate::error::{Error, Result};
use crate::ids::{MailboxId, TenantId};
use crate::meta::{AccountScope, Flags, Mailbox};

/// Наибольшая длина имени скрипта, символов.
pub const MAX_SCRIPT_NAME_LEN: usize = 128;

/// Наибольшая длина темы автоответа, символов.
pub const MAX_VACATION_SUBJECT_LEN: usize = 998;

/// Наибольшая длина текста автоответа (текстового и HTML отдельно), байт.
pub const MAX_VACATION_BODY_LEN: usize = 64 * 1024;

/// Имя скрипта (RFC 5804 §1.6): 1–[`MAX_SCRIPT_NAME_LEN`] символов без управляющих (в том числе U+0080–U+009F),
/// без U+2028 и U+2029.
pub fn valid_script_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= MAX_SCRIPT_NAME_LEN
        && !name
            .chars()
            .any(|c| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}'))
}

/// Скрипт в списке учётной записи.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptInfo {
    /// Имя.
    pub name: String,
    /// Размер текста, байт.
    pub size: u64,
    /// Активный (выполняется при доставке); активный не больше одного.
    pub active: bool,
}

/// Активный скрипт учётной записи.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveScript {
    /// Имя.
    pub name: String,
    /// Текст.
    pub text: String,
}

/// Лимиты хранилища — из конфигурации реализации (`meta`); ManageSieve (`HAVESPACE`, `NO (QUOTA/MAXSIZE)`) и JMAP Sieve
/// (`maxNumberScripts`, `maxSizeScript`) публикуют их. Превышение — `Error::QuotaExceeded`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SieveLimits {
    /// Скриптов учётной записи.
    pub max_scripts: usize,
    /// Размер текста скрипта (и скрипта организации), байт.
    pub max_script_size: usize,
}

impl Default for SieveLimits {
    /// 16 скриптов по 128 КиБ.
    fn default() -> Self {
        Self {
            max_scripts: 16,
            max_script_size: 128 * 1024,
        }
    }
}

/// Автоответ учётной записи (модель JMAP `VacationResponse`, RFC 8621 §8); выполняется до активного скрипта.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Vacation {
    /// Включён.
    pub enabled: bool,
    /// Начало действия (UTC, `fromDate`); `None` — сразу.
    pub from: Option<OffsetDateTime>,
    /// Конец действия (UTC, `toDate`, не включая); `None` — бессрочно.
    pub to: Option<OffsetDateTime>,
    /// Тема; `None` — по умолчанию `tp-sieve` («Автоответ: <тема письма>»).
    pub subject: Option<String>,
    /// Текст ответа.
    pub text_body: Option<String>,
    /// Текст ответа HTML.
    pub html_body: Option<String>,
}

impl Vacation {
    /// Проверяет поля: `from` не позже `to`; тема — до [`MAX_VACATION_SUBJECT_LEN`] символов без управляющих;
    /// тексты — до [`MAX_VACATION_BODY_LEN`] байт. Иначе — `Error::Invalid`.
    pub fn validate(&self) -> Result<()> {
        if let (Some(from), Some(to)) = (self.from, self.to)
            && from > to
        {
            return Err(Error::invalid("начало автоответа позже конца"));
        }
        if self.subject.as_deref().is_some_and(|s| {
            s.chars().count() > MAX_VACATION_SUBJECT_LEN || s.chars().any(char::is_control)
        }) {
            return Err(Error::invalid("недопустимая тема автоответа"));
        }
        if [&self.text_body, &self.html_body]
            .into_iter()
            .flatten()
            .any(|b| b.len() > MAX_VACATION_BODY_LEN)
        {
            return Err(Error::invalid("текст автоответа слишком длинный"));
        }
        Ok(())
    }

    /// Автоответ действует в момент `now`: включён и `from ≤ now < to`.
    pub fn is_active(&self, now: OffsetDateTime) -> bool {
        self.enabled && self.from.is_none_or(|f| f <= now) && self.to.is_none_or(|t| now < t)
    }
}

/// Сигнал об изменении скриптов или автоответа учётной записи `scope` ([`EventKind::SieveChanged`]): публикуется после
/// фиксации каждого изменения [`SieveStore`], кроме скрипта организации.
pub fn sieve_changed(scope: AccountScope) -> Event {
    Event {
        tenant: scope.tenant,
        account: Some(scope.account),
        kind: EventKind::SieveChanged,
    }
}

/// Скрипты Sieve и автоответ. Реализация — `tp-meta` (META-49); in-memory — `tp-testkit`. Текст скрипта хранилище не
/// разбирает: его проверяет вызывающий (`tp-sieve`). Удаление учётной записи удаляет её скрипты и автоответ.
#[async_trait]
pub trait SieveStore: Send + Sync + 'static {
    /// Лимиты хранилища.
    fn limits(&self) -> SieveLimits;

    /// Скрипты учётной записи по имени (побайтно).
    async fn list(&self, scope: AccountScope) -> Result<Vec<ScriptInfo>>;

    /// Текст скрипта.
    async fn get(&self, scope: AccountScope, name: &str) -> Result<Option<String>>;

    /// Активный скрипт (для доставки — одним запросом).
    async fn active(&self, scope: AccountScope) -> Result<Option<ActiveScript>>;

    /// Создаёт или заменяет текст скрипта (активность сохраняется). Недопустимое имя ([`valid_script_name`]) —
    /// `Invalid`; текст больше [`SieveLimits::max_script_size`] или новый скрипт сверх [`SieveLimits::max_scripts`] —
    /// `QuotaExceeded`. Возвращает новое состояние ([`SieveStore::state`]).
    async fn put(&self, scope: AccountScope, name: &str, text: String) -> Result<u64>;

    /// Переименовывает скрипт (активность сохраняется): нет `from` — `NotFound`, `to` занято другим — `Conflict`,
    /// недопустимое `to` — `Invalid`; `from == to` — без изменений. Возвращает состояние.
    async fn rename(&self, scope: AccountScope, from: &str, to: &str) -> Result<u64>;

    /// Удаляет скрипт: нет — `NotFound`, активный — `Conflict` (ManageSieve `ACTIVE`, JMAP `scriptIsActive`).
    /// Возвращает новое состояние.
    async fn delete(&self, scope: AccountScope, name: &str) -> Result<u64>;

    /// Делает активным скрипт `name` (прежний активный — неактивен) или, при `None`, снимает активность со всех;
    /// нет скрипта — `NotFound`. Возвращает новое состояние.
    async fn activate(&self, scope: AccountScope, name: Option<&str>) -> Result<u64>;

    /// Скрипт организации: выполняется до скрипта пользователя у всех учётных записей организации.
    async fn org_script(&self, tenant: TenantId) -> Result<Option<String>>;

    /// Задаёт (`Some`) или удаляет (`None`) скрипт организации; больше [`SieveLimits::max_script_size`] —
    /// `QuotaExceeded`. Сигнала нет: `tp-sieve` сверяет скомпилированную программу с текстом.
    async fn set_org_script(&self, tenant: TenantId, text: Option<String>) -> Result<()>;

    /// Автоответ; не задан — `Vacation::default()` (выключен).
    async fn vacation(&self, scope: AccountScope) -> Result<Vacation>;

    /// Заменяет автоответ; недопустимые поля ([`Vacation::validate`]) — `Invalid`. Возвращает новое состояние.
    async fn set_vacation(&self, scope: AccountScope, vacation: Vacation) -> Result<u64>;

    /// Состояние скриптов и автоответа учётной записи (JMAP `state`, версия для кэша программ): растёт при каждом
    /// изменении, у новой учётной записи — 0.
    async fn state(&self, scope: AccountScope) -> Result<u64>;
}

/// Журнал отправленных автоответов (RFC 5230 `:days`, `:handle`). Реализация — `tp-meta` (META-50); in-memory —
/// `tp-testkit`. Старые записи очищает реализация.
#[async_trait]
pub trait VacationLog: Send + Sync + 'static {
    /// Атомарно: если учётная запись `scope` уже отвечала `sender` по `handle` меньше `period` назад — `true`
    /// (не отвечать); иначе отмечает ответ текущим моментом и возвращает `false`. Из параллельных вызовов `false`
    /// получает один. `sender` сравнивается побайтно: адрес нормализует вызывающий.
    async fn seen(
        &self,
        scope: AccountScope,
        sender: &str,
        handle: &str,
        period: Duration,
    ) -> Result<bool>;
}

// ---------- Точка правил доставки ----------

/// Письмо одному локальному получателю на входе правил.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RulesInput {
    /// Учётная запись получателя.
    pub scope: AccountScope,
    /// Адрес получателя из конверта с подадресом (`anna+news@example.org`, RFC 5233).
    pub recipient: String,
    /// Отправитель конверта (`MAIL FROM`); пустой — уведомление о недоставке.
    pub mail_from: String,
    /// Письмо целиком (RFC 5322); тесты тела читают не больше лимита секции `sieve`.
    pub message: Bytes,
    /// Папки учётной записи (`fileinto`, `mailboxexists`); путь — имена от корня через `/`.
    pub mailboxes: Vec<Mailbox>,
}

/// Папка доставки.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MailboxRef {
    /// «Входящие» (implicit keep, `keep`).
    Inbox,
    /// Существующая папка из [`RulesInput::mailboxes`].
    Id(MailboxId),
    /// Папка по пути (`fileinto`): нет и `create` (`:create`, RFC 5490) — создать; нет без `create` — «Входящие».
    Name {
        /// Путь через `/`.
        name: String,
        /// Создать, если нет.
        create: bool,
    },
}

/// Доставка в папку с флагами (`keep`, `fileinto`, `:flags` RFC 5232).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeepTo {
    /// Папка.
    pub mailbox: MailboxRef,
    /// Начальные флаги.
    pub flags: Flags,
}

/// Текст автоответа.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReplyBody {
    /// `text/plain`.
    Text(String),
    /// `multipart/alternative`: HTML и, если есть, текстовый вариант.
    Html {
        /// Текстовый вариант.
        text: Option<String>,
        /// HTML.
        html: String,
    },
    /// Готовая MIME-сущность из скрипта (`vacation :mime`): заголовки `Content-*` и тело.
    Mime(String),
}

/// Автоответ, решённый правилами (период `:days` уже проверен через [`VacationLog::seen`]). Письмо собирает `tp-smtp`
/// (RFC 3834: `MAIL FROM:<>`, `Auto-Submitted: auto-replied`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoReply {
    /// Кому — отправитель исходного письма.
    pub to: String,
    /// От кого — адрес получателя или `:from`.
    pub from: String,
    /// Тема.
    pub subject: String,
    /// Текст.
    pub body: ReplyBody,
    /// `Message-ID` исходного письма (`In-Reply-To`).
    pub in_reply_to: Option<String>,
    /// `References` ответа (цепочка исходного письма и его `Message-ID`).
    pub references: Vec<String>,
}

/// Итог правил для получателя.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RulesOutcome {
    /// Доставки (по одной на папку); пусто — письмо в ящик не кладётся.
    pub keep: Vec<KeepTo>,
    /// Адреса перенаправления (`redirect`, с `:copy` и без: отмена implicit keep уже учтена в `keep`).
    pub redirect: Vec<String>,
    /// Письмо отброшено (`discard`) — для журнала доставки; `keep` при этом пуст.
    pub discard: bool,
    /// Автоответ.
    pub vacation: Option<AutoReply>,
    /// Ошибки выполнения (без содержимого письма) для журнала; при ошибке правила делают implicit keep.
    pub errors: Vec<String>,
}

impl RulesOutcome {
    /// Implicit keep: во «Входящие» без флагов (нет правил).
    pub fn implicit_keep() -> Self {
        Self {
            keep: vec![KeepTo {
                mailbox: MailboxRef::Inbox,
                flags: Flags::new(),
            }],
            ..Self::default()
        }
    }

    /// Implicit keep с ошибкой `error` (ошибка скрипта).
    pub fn failed(error: impl Into<String>) -> Self {
        Self {
            errors: vec![error.into()],
            ..Self::implicit_keep()
        }
    }
}

/// Точка правил доставки: скрипт организации, автоответ, активный скрипт пользователя. Реализация — `tp-sieve`
/// (`Rules`, SIEVE-07); фейк — `tp-testkit::FakeRules`. Вызывает `tp-smtp` для каждого локального получателя после
/// правил потока и до `MailStore::ingest`, в том числе для внутренней почты.
#[async_trait]
pub trait DeliveryRules: Send + Sync + 'static {
    /// Применяет правила получателя. Ошибка скрипта — `Ok` с implicit keep и [`RulesOutcome::errors`]; `Err` — сбой
    /// хранилища: вызывающий доставляет во «Входящие» ([`RulesOutcome::implicit_keep`]) и пишет в журнал.
    async fn evaluate(&self, input: &RulesInput) -> Result<RulesOutcome>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn object_safe() {
        fn _store(_: Arc<dyn SieveStore>) {}
        fn _log(_: Arc<dyn VacationLog>) {}
        fn _rules(_: Arc<dyn DeliveryRules>) {}
    }

    #[test]
    fn script_names() {
        for ok in ["main", "Правила", "a b"] {
            assert!(valid_script_name(ok), "{ok:?}");
        }
        for bad in [
            "",
            "a\nb",
            "a\u{85}",
            "a\u{2028}",
            &"x".repeat(MAX_SCRIPT_NAME_LEN + 1),
        ] {
            assert!(!valid_script_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn vacation() {
        let t = |h: i64| OffsetDateTime::UNIX_EPOCH + time::Duration::hours(h);
        let v = Vacation {
            enabled: true,
            from: Some(t(10)),
            to: Some(t(20)),
            subject: Some("В отпуске".into()),
            text_body: Some("Вернусь позже".into()),
            html_body: None,
        };
        assert_eq!(v.validate(), Ok(()));
        assert!(!v.is_active(t(9)));
        assert!(v.is_active(t(10)));
        assert!(!v.is_active(t(20)));
        assert!(
            !Vacation {
                enabled: false,
                ..v.clone()
            }
            .is_active(t(15))
        );
        assert!(
            Vacation {
                from: None,
                to: None,
                ..v.clone()
            }
            .is_active(t(0))
        );
        assert!(!Vacation::default().is_active(t(0)));
        for bad in [
            Vacation {
                from: Some(t(21)),
                ..v.clone()
            },
            Vacation {
                subject: Some("a\nb".into()),
                ..v.clone()
            },
            Vacation {
                html_body: Some("x".repeat(MAX_VACATION_BODY_LEN + 1)),
                ..v.clone()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn outcomes() {
        let keep = RulesOutcome::implicit_keep();
        assert_eq!(keep.keep.len(), 1);
        assert_eq!(keep.keep[0].mailbox, MailboxRef::Inbox);
        assert!(!keep.discard && keep.errors.is_empty());
        let failed = RulesOutcome::failed("ошибка");
        assert_eq!(failed.keep, keep.keep);
        assert_eq!(failed.errors, ["ошибка"]);
        let s = serde_json::to_string(&failed).expect("json");
        assert_eq!(
            serde_json::from_str::<RulesOutcome>(&s).expect("parse"),
            failed
        );
    }
}
