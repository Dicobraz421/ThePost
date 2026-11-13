//! Сервисы ролей ядра (SRS 2.1): maintenance — GC и «сироты» BLOB под лизой лидера, истечение срока корзины
//! (`tp-mailstore`: таймер на ближайший срок → точка принятия решений → окончательное удаление), подписчики
//! журнала событий (по сигналам, [`crate::log_reader`]), очистка по сроку надгробий DAV, журнала автоответов,
//! журнала доставки, токенов `admin init` и применённых сообщений входящих планирования
//! ([`Purge`], решение 2026-11-11 п. 65, APP-53), проверка целостности BLOB по расписанию под лизой лидера
//! ([`ScheduledVerify`], APP-44), однократный пересчёт поисковых строк старых контактов в фоне (APP-63),
//! окончательное удаление отключённых ящиков по сроку под лизой лидера ([`run_disabled`], APP-67); роли протоколов,
//! ещё не реализованные в этой сборке, — заглушки, готовые сразу (Ф0).

use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use tp_blob::{
    GcConfig, GcWorker, Now, OrphanConfig, OrphanMetrics, OrphanSweeper, Verifier, VerifyReport,
    system_now,
};
use tp_contracts::admin_token::LocalAdminTokens;
use tp_contracts::blob::{BlobIndex, BlobStore};
use tp_contracts::dav::CardSearch;
use tp_contracts::events::EventSink;
use tp_contracts::lease::LeaseStore;
use tp_contracts::meta::{AccountScope, AccountStore};
use tp_contracts::role::{CancellationToken, Role, RoleCtx, RoleService};
use tp_contracts::{Result, TenantId};
use tp_mailstore::DisabledMailboxes;
use tp_meta::PgMeta;
use tp_meta::events::EventConsumer;

use crate::log_reader::{LogReader, LogSignals};
use crate::sections::VerifySchedule;

/// Роль ядра, реализация которой ещё не вошла в сборку (протоколы — этап Ф1): объявляет готовность
/// и ждёт остановки, чтобы узел с пресетом `all` доходил до ready.
#[derive(Debug, Clone, Copy)]
pub struct PendingRole(pub Role);

#[async_trait]
impl RoleService for PendingRole {
    fn name(&self) -> &str {
        self.0.as_str()
    }

    async fn run(&self, ctx: RoleCtx) -> Result<()> {
        tracing::warn!(role = %self.0, "роль ещё не реализована в этой сборке: соединений не принимает");
        ctx.ready.set(true);
        ctx.shutdown.cancelled().await;
        Ok(())
    }
}

/// Воркер поиска «сирот» роли maintenance для `/metrics` (APP-34): создаётся при первом запуске роли и
/// переживает её перезапуски — счётчики не обнуляются. До запуска роли снимок нулевой. Клон — тот же воркер.
#[derive(Clone, Default)]
pub struct OrphanProbe(Arc<OnceLock<Arc<OrphanSweeper>>>);

impl OrphanProbe {
    /// Снимок метрик воркера ([`OrphanSweeper::metrics`]).
    pub fn metrics(&self) -> OrphanMetrics {
        self.0.get().map(|s| s.metrics()).unwrap_or_default()
    }
}

impl std::fmt::Debug for OrphanProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrphanProbe")
            .field("started", &self.0.get().is_some())
            .finish()
    }
}

/// Зависимости роли maintenance.
pub struct Maintenance {
    /// Учёт объектов BLOB по всем шардам справочника (`PgMeta::with_shards`, FIX-04): GC и поиск «сирот» не
    /// удаляют объект, на который ссылается любой шард.
    pub index: Arc<dyn BlobIndex>,
    /// Хранилище BLOB.
    pub store: Arc<dyn BlobStore>,
    /// Лизы лидера GC и поиска «сирот».
    pub leases: Arc<dyn LeaseStore>,
    /// Держатель лиз: узел и процесс.
    pub holder: String,
    /// Обработчик журнала событий с назначением разделов узлу.
    pub events: EventConsumer,
    /// Подписчики журнала событий (модули).
    pub sinks: Vec<Arc<dyn EventSink>>,
    /// Сигналы, будящие подписчиков журнала (опроса нет).
    pub signals: LogSignals,
    /// Сборка мусора объектов (`blob.gc`).
    pub gc: GcConfig,
    /// Поиск «сирот» (`blob.orphans`).
    pub orphans: OrphanConfig,
    /// Воркер поиска «сирот»: его метрики читает `/metrics` (`crate::metrics::register_orphans`).
    pub orphan_probe: OrphanProbe,
    /// Почтовое хранилище для истечения срока корзины (`TrashStore::expired` → `before_final_delete` →
    /// `expunge_final`, секция `mailstore.trash`); `None` — истечение не выполняется (тесты).
    pub trash: Option<tp_mailstore::Store>,
    /// Окончательное удаление отключённых ящиков по сроку (`DisabledMailboxes::run`, секция `mailstore.disabled`,
    /// APP-67) под лизой [`DISABLED_LEASE`]; `None` — не выполняется (тесты).
    pub disabled: Option<DisabledMailboxes>,
    /// Очистка по сроку надгробий DAV и журнала автоответов; `None` — не выполняется (тесты без PostgreSQL).
    pub purge: Option<Purge>,
    /// Проверка целостности BLOB по расписанию; `None` — выключена (`maintenance.verify.enabled: false`).
    pub verify: Option<ScheduledVerify>,
    /// Выгрузка журнала аудита в SIEM (`admin.audit`, APP-59); `None` — не выполняется (тесты без PostgreSQL).
    pub audit_export: Option<Arc<crate::audit_export::AuditExport>>,
    /// Пересчёт поисковых строк контактов, записанных до миграции 0030 (`PgMeta::reindex_cards`, META-78, APP-63):
    /// один раз в фоне при запуске роли; `None` — не выполняется (тесты без PostgreSQL).
    pub card_reindex: Option<PgMeta>,
    /// Заполнение книги GAL «Организация» из каталога — LDAP или PostgreSQL (`maintenance.gal_interval`, APP-49,
    /// APP-51); `None` — не выполняется (тесты без PostgreSQL).
    pub gal: Option<Arc<crate::gal::GalSync>>,
}

/// Показатели проверки целостности BLOB по расписанию для `/metrics` (`crate::metrics::register_verify`).
/// Расхождения — итог последнего завершённого прохода: после исправления (`thepost verify --fix`) следующий
/// проход их обнуляет.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct VerifyMetrics {
    /// Завершённые проходы.
    pub passes: u64,
    /// Проходы, прерванные ошибкой (листинг хранилища, СУБД) или потерей лизы.
    pub failed: u64,
    /// Записи учёта в последнем проходе, у которых объекта нет в хранилище.
    pub missing: u64,
    /// Записи учёта в последнем проходе, у которых размер объекта не совпадает с учётом.
    pub size_mismatch: u64,
    /// «Сироты» в последнем проходе: объекты хранилища без записи учёта.
    pub orphans: u64,
    /// Объекты и записи, которые в последнем проходе проверить не удалось.
    pub errors: u64,
    /// Время последнего прохода, секунд.
    pub duration_secs: u64,
    /// Момент завершения последнего прохода, секунд от начала эпохи Unix; 0 — проходов не было.
    pub finished_at: u64,
}

/// Держатель показателей проверки целостности: пишет воркер роли, читает `/metrics`. Клон — те же показатели;
/// перезапуск роли их не обнуляет.
#[derive(Debug, Clone, Default)]
pub struct VerifyProbe(Arc<Mutex<VerifyMetrics>>);

impl VerifyProbe {
    /// Снимок показателей.
    pub fn metrics(&self) -> VerifyMetrics {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn update(&self, f: impl FnOnce(&mut VerifyMetrics)) {
        f(&mut self.0.lock().unwrap_or_else(PoisonError::into_inner));
    }
}

/// Итог попытки прохода [`ScheduledVerify::pass`].
#[derive(Debug)]
pub enum VerifyPass {
    /// Проход завершён: отчёт сверки.
    Done(VerifyReport),
    /// Лиза у другого узла: проход выполняет он.
    NotLeader,
    /// Проход не завершён: ошибка сверки или лизы, потеря лизы.
    Failed,
}

/// Проверка целостности BLOB по расписанию в роли maintenance (APP-44): сверка `thepost verify` без `--fix` под
/// лизой лидера. Ничего не исправляет: расхождения — `ERROR` в журнал узла и показатели, исправление — вручную.
#[derive(Clone)]
pub struct ScheduledVerify {
    /// Сверка хранилища и учёта (та же, что у `thepost verify`).
    pub verifier: Arc<Verifier>,
    /// Расписание и лиза (`maintenance.verify`).
    pub schedule: VerifySchedule,
    /// Показатели для `/metrics`.
    pub probe: VerifyProbe,
    /// Часы расписания (UTC).
    pub now: Now,
}

impl std::fmt::Debug for ScheduledVerify {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScheduledVerify")
            .field("schedule", &self.schedule)
            .finish_non_exhaustive()
    }
}

impl ScheduledVerify {
    /// Проходы по расписанию до остановки; остановка прерывает и идущий проход (сверка только читает).
    pub async fn run(
        self,
        leases: Arc<dyn LeaseStore>,
        holder: String,
        shutdown: CancellationToken,
    ) {
        loop {
            let nanos = u64::try_from((self.now)().unix_timestamp_nanos()).unwrap_or_default();
            let since_epoch = Duration::from_nanos(nanos);
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = tokio::time::sleep(self.schedule.until_next(since_epoch)) => {}
            }
            tokio::select! {
                () = shutdown.cancelled() => return,
                _ = self.pass(&*leases, &holder) => {}
            }
        }
    }

    /// Один проход под лизой: лиза продлевается, пока идёт сверка, и после прохода не отдаётся — истекает сама
    /// (`lease_ttl`), чтобы узел с отстающими часами не повторил проход. Потеря лизы прерывает проход.
    pub async fn pass(&self, leases: &dyn LeaseStore, holder: &str) -> VerifyPass {
        let (name, ttl) = (&self.schedule.lease_name, self.schedule.lease_ttl);
        let mut lease = match leases.try_acquire(name, holder, ttl).await {
            Ok(Some(lease)) => lease,
            Ok(None) => {
                tracing::debug!(lease = %name, "проверка целостности BLOB: проход выполняет другой узел");
                return VerifyPass::NotLeader;
            }
            Err(e) => {
                tracing::warn!(lease = %name, error = %e, "проверка целостности BLOB: лиза не получена, проход пропущен");
                self.probe.update(|m| m.failed += 1);
                return VerifyPass::Failed;
            }
        };
        let started = tokio::time::Instant::now();
        let mut renew =
            tokio::time::interval_at(started + ttl / 3, (ttl / 3).max(Duration::from_millis(1)));
        renew.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let sweep = self.verifier.run(false);
        tokio::pin!(sweep);
        let result = loop {
            tokio::select! {
                result = &mut sweep => break result,
                _ = renew.tick() => match leases.renew(&lease, ttl).await {
                    Ok(Some(renewed)) => lease = renewed,
                    Ok(None) => {
                        tracing::warn!(lease = %name, "проверка целостности BLOB: лиза потеряна, проход прерван");
                        self.probe.update(|m| m.failed += 1);
                        return VerifyPass::Failed;
                    }
                    // Сверка только читает: сбой продления не повод её бросать — повтор на следующем такте.
                    Err(e) => tracing::warn!(lease = %name, error = %e, "проверка целостности BLOB: лиза не продлена"),
                },
            }
        };
        let report = match result {
            Ok(report) => report,
            Err(e) => {
                tracing::error!(error = %e, "проверка целостности BLOB: проход прерван ошибкой");
                self.probe.update(|m| m.failed += 1);
                return VerifyPass::Failed;
            }
        };
        let duration_secs = started.elapsed().as_secs();
        self.probe.update(|m| {
            m.passes += 1;
            m.missing = report.missing;
            m.size_mismatch = report.size_mismatch;
            m.orphans = report.orphans;
            m.errors = report.errors;
            m.duration_secs = duration_secs;
            m.finished_at = u64::try_from(report.finished_at).unwrap_or_default();
        });
        for d in &report.damaged_blobs {
            tracing::error!(
                path = %d.path,
                damage = ?d.damage,
                expected_size = d.expected_size,
                actual_size = ?d.actual_size,
                "проверка целостности BLOB: запись учёта повреждена"
            );
        }
        for path in &report.orphan_paths {
            tracing::error!(path = %path, "проверка целостности BLOB: объект без записи учёта («сирота»)");
        }
        let (objects, records, missing, size_mismatch, orphans, errors, truncated) = (
            report.objects_listed,
            report.records_checked,
            report.missing,
            report.size_mismatch,
            report.orphans,
            report.errors,
            report.truncated,
        );
        if missing + size_mismatch + orphans + errors == 0 {
            tracing::info!(
                objects,
                records,
                duration_secs,
                "проверка целостности BLOB: расхождений нет"
            );
        } else {
            tracing::error!(
                objects,
                records,
                missing,
                size_mismatch,
                orphans,
                errors,
                truncated,
                duration_secs,
                "проверка целостности BLOB: найдены расхождения; исправление — вручную: thepost verify --fix"
            );
        }
        VerifyPass::Done(report)
    }
}

/// Очистка по сроку в роли maintenance (APP-32, APP-39): надгробия DAV старше `meta.dav.tombstone_ttl`, записи
/// журнала автоответов старше `meta.sieve.vacation_log_ttl`, журнал доставки старше `meta.delivery.retention`,
/// завершённые и отменённые отправки JMAP старше `meta.submissions.retention`, аудит ящиков старше
/// `meta.mailbox_audit.retention` — сроки знает [`PgMeta`]; токены `admin init`, истёкшие раньше `maintenance.admin_token_retention` назад;
/// применённые сервером сообщения входящих планирования старше `dav.schedule_processed_ttl` ([`ItipPurge`], APP-53).
/// Удаление по сроку идемпотентно, повтор удаляет только оставшееся: как и истечение корзины, работает на каждом
/// узле с ролью без лизы.
#[derive(Clone)]
pub struct Purge {
    /// Метаданные: шаги `purge_dav_tombstones`, `purge_vacation_log`, `purge_expired_delivery`,
    /// `expire_old_submissions`, `purge_expired_mailbox_audit` и `LocalAdminTokens::prune`.
    pub meta: PgMeta,
    /// Период (`maintenance.purge_interval`); первый проход — при запуске роли.
    pub interval: Duration,
    /// Срок хранения токенов `admin init` после истечения (`maintenance.admin_token_retention`).
    pub admin_token_retention: Duration,
    /// Показатели шагов для `/metrics` (`crate::metrics::register_purge`, APP-52).
    pub probe: PurgeProbe,
    /// Очистка входящих планирования (DAV-32, APP-53); `None` — шага нет (`dav.schedule_keep_processed: false`:
    /// применённые сообщения во входящих не хранятся).
    pub itip: Option<ItipPurge>,
}

/// Шаг очистки входящих планирования (APP-53): у каждой учётной записи организации удаляет применённые сервером
/// сообщения iTIP, полученные раньше `dav.schedule_processed_ttl` назад (`tp_dav::ItipInbox::purge_processed`).
#[derive(Debug, Clone)]
pub struct ItipPurge {
    /// Организация узла.
    pub tenant: TenantId,
    /// Приём iTIP узла: тот же, что у CalDAV, JMAP и доставки (`Services::itip`).
    pub inbox: Arc<tp_dav::ItipInbox>,
}

/// Учётных записей в одной странице обхода [`ItipPurge`].
const ITIP_PURGE_PAGE: usize = 500;

impl ItipPurge {
    /// Обходит учётные записи организации страницами `AccountStore::list`; ошибка одной учётной записи — в журнал,
    /// обход продолжается, итог шага — первая ошибка (повтор — в следующий проход).
    async fn step(&self, accounts: &dyn AccountStore) -> Result<u64> {
        let now = (system_now())();
        let mut removed = 0u64;
        let mut failed = None;
        let mut after: Option<String> = None;
        loop {
            let page = accounts
                .list(self.tenant, "", after.as_deref(), ITIP_PURGE_PAGE)
                .await?;
            for account in &page {
                let scope = AccountScope {
                    tenant: self.tenant,
                    account: account.id,
                };
                match self.inbox.purge_processed(scope, now).await {
                    Ok(n) => removed += n as u64,
                    Err(e) => {
                        tracing::warn!(
                            account = %account.id,
                            error = %e,
                            "входящие планирования: применённые сообщения не удалены"
                        );
                        failed.get_or_insert(e);
                    }
                }
            }
            match page.last() {
                Some(last) if page.len() == ITIP_PURGE_PAGE => {
                    after = Some(last.login.to_lowercase());
                }
                _ => break,
            }
        }
        match failed {
            Some(e) => Err(e),
            None => Ok(removed),
        }
    }
}

/// Итог шага очистки: число удалённых записей (`None` — журнал удаляется секциями по суткам, числа нет) или ошибка.
pub type PurgeOutcome<'a> = std::result::Result<Option<u64>, &'a tp_contracts::Error>;

/// Итог одного прохода [`Purge::step`]: число удалённых записей каждого шага.
#[derive(Debug)]
pub struct Purged {
    /// Надгробия DAV.
    pub dav_tombstones: Result<u64>,
    /// Записи журнала автоответов.
    pub vacation_log: Result<u64>,
    /// Журнал доставки: удаляется секциями по суткам, числа записей нет.
    pub delivery_log: Result<()>,
    /// Отправки JMAP (`SubmissionStore::expire_submissions`).
    pub submissions: Result<u64>,
    /// Аудит ящиков: удаляется секциями по суткам, числа записей нет.
    pub mailbox_audit: Result<()>,
    /// Токены `admin init`.
    pub admin_tokens: Result<u64>,
    /// Применённые сообщения входящих планирования ([`ItipPurge`]); `None` — шага нет.
    pub itip_processed: Option<Result<u64>>,
}

impl Purged {
    /// Шаги прохода — значения метки `step` показателей [`PurgeProbe`], в порядке [`Purged::steps`].
    pub const STEPS: [&'static str; 7] = [
        "purge_dav_tombstones",
        "purge_vacation_log",
        "purge_expired_delivery",
        "expire_old_submissions",
        "purge_expired_mailbox_audit",
        "prune_admin_tokens",
        "purge_processed_itip",
    ];

    /// Итог каждого выполненного шага прохода с его именем (шага без зависимостей — нет).
    pub fn steps(&self) -> Vec<(&'static str, PurgeOutcome<'_>)> {
        fn counted(r: &Result<u64>) -> PurgeOutcome<'_> {
            r.as_ref().map(|n| Some(*n))
        }
        fn sections(r: &Result<()>) -> PurgeOutcome<'_> {
            r.as_ref().map(|()| None)
        }
        let [dav, vacation, delivery, submissions, audit, tokens, itip] = Self::STEPS;
        let mut steps = vec![
            (dav, counted(&self.dav_tombstones)),
            (vacation, counted(&self.vacation_log)),
            (delivery, sections(&self.delivery_log)),
            (submissions, counted(&self.submissions)),
            (audit, sections(&self.mailbox_audit)),
            (tokens, counted(&self.admin_tokens)),
        ];
        steps.extend(self.itip_processed.as_ref().map(|r| (itip, counted(r))));
        steps
    }
}

/// Метка шага очистки (`step`).
pub(crate) type StepLabel = [(&'static str, &'static str); 1];

/// Показатели очистки по сроку для `/metrics` (APP-52): по шагу ([`Purged::STEPS`]) — выполнения, ошибки, удалённые
/// записи (у шагов с числом) и момент последнего успешного выполнения. Пишет [`Purge::step`], регистрирует
/// `crate::metrics::register_purge`; клон — те же показатели, перезапуск роли их не обнуляет.
#[derive(Debug, Clone, Default)]
pub struct PurgeProbe {
    pub(crate) runs: Family<StepLabel, Counter>,
    pub(crate) errors: Family<StepLabel, Counter>,
    pub(crate) purged: Family<StepLabel, Counter>,
    pub(crate) last_success: Family<StepLabel, Gauge>,
}

impl PurgeProbe {
    /// Показатели с нулевыми значениями для каждого шага: ряды есть до первого прохода.
    pub fn new() -> Self {
        let probe = Self::default();
        for step in Purged::STEPS {
            let label = [("step", step)];
            probe.runs.get_or_create_owned(&label);
            probe.errors.get_or_create_owned(&label);
            probe.last_success.get_or_create_owned(&label);
        }
        probe
    }

    /// Итог шага `step`; `at` — момент прохода, секунд от начала эпохи Unix.
    pub fn record(&self, step: &'static str, outcome: PurgeOutcome<'_>, at: i64) {
        let label = [("step", step)];
        self.runs.get_or_create(&label).inc();
        match outcome {
            Ok(deleted) => {
                if let Some(n) = deleted {
                    self.purged.get_or_create(&label).inc_by(n);
                }
                self.last_success.get_or_create(&label).set(at);
            }
            Err(_) => {
                self.errors.get_or_create(&label).inc();
            }
        }
    }
}

impl std::fmt::Debug for Purge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Purge")
            .field("interval", &self.interval)
            .field("admin_token_retention", &self.admin_token_retention)
            .field("itip", &self.itip.is_some())
            .finish_non_exhaustive()
    }
}

impl Purge {
    /// Один проход: шаги независимы — ошибка одного не отменяет остальные и ждёт следующего прохода.
    pub async fn step(&self) -> Purged {
        let done = Purged {
            dav_tombstones: self.meta.purge_dav_tombstones().await,
            vacation_log: self.meta.purge_vacation_log().await,
            delivery_log: self.meta.purge_expired_delivery().await,
            submissions: self.meta.expire_old_submissions().await,
            mailbox_audit: self.meta.purge_expired_mailbox_audit().await,
            admin_tokens: self.prune_admin_tokens().await,
            itip_processed: match &self.itip {
                Some(itip) => Some(itip.step(&self.meta).await),
                None => None,
            },
        };
        let at = (system_now())().unix_timestamp();
        for (step, outcome) in done.steps() {
            match outcome {
                Ok(Some(0)) => tracing::debug!(step, "очистка по сроку: удалять нечего"),
                Ok(Some(deleted)) => tracing::info!(step, deleted, "очистка по сроку"),
                // Об удалённых секциях журналов пишет сам `tp-meta`.
                Ok(None) => {}
                Err(e) => tracing::warn!(
                    step,
                    error = %e,
                    "очистка по сроку не выполнена: повтор в следующий проход"
                ),
            }
            self.probe.record(step, outcome, at);
        }
        done
    }

    /// Удаляет токены `admin init`, истёкшие раньше чем [`Purge::admin_token_retention`] назад.
    async fn prune_admin_tokens(&self) -> Result<u64> {
        let now = (system_now())();
        let before = match TryFrom::try_from(self.admin_token_retention) {
            Ok(retention) => now.checked_sub(retention),
            Err(_) => None,
        }
        .ok_or_else(|| {
            tp_contracts::Error::invalid("maintenance.admin_token_retention вне диапазона")
        })?;
        LocalAdminTokens::prune(&self.meta, before).await
    }

    /// Проходы с периодом [`Purge::interval`] до остановки; остановка прерывает и идущий проход (незавершённая
    /// транзакция откатывается, следующий запуск удалит оставшееся).
    pub async fn run(self, shutdown: CancellationToken) {
        let mut tick = tokio::time::interval(self.interval.max(Duration::from_millis(1)));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = shutdown.cancelled() => return,
                _ = tick.tick() => {}
            }
            tokio::select! {
                () = shutdown.cancelled() => return,
                _ = self.step() => {}
            }
        }
    }
}

impl std::fmt::Debug for Maintenance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Maintenance")
            .field("holder", &self.holder)
            .field("trash", &self.trash.is_some())
            .field("purge", &self.purge)
            .field("gal", &self.gal.is_some())
            .field(
                "sinks",
                &self.sinks.iter().map(|s| s.name()).collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl RoleService for Maintenance {
    fn name(&self) -> &str {
        Role::Maintenance.as_str()
    }

    async fn run(&self, ctx: RoleCtx) -> Result<()> {
        // Ленивая инициализация (SRS 2.7): воркеры создаются при запуске роли.
        let gc = GcWorker::new(
            self.index.clone(),
            self.store.clone(),
            self.gc.clone(),
            system_now(),
        );
        let orphans = self
            .orphan_probe
            .0
            .get_or_init(|| {
                Arc::new(OrphanSweeper::new(
                    self.index.clone(),
                    self.store.clone(),
                    self.orphans.clone(),
                    system_now(),
                ))
            })
            .clone();
        let (leases, holder, token) = (
            self.leases.clone(),
            self.holder.clone(),
            ctx.shutdown.clone(),
        );
        let gc = tokio::spawn(async move {
            gc.run(&*leases, &holder, token.cancelled_owned()).await;
        });
        let (leases, holder, token) = (
            self.leases.clone(),
            self.holder.clone(),
            ctx.shutdown.clone(),
        );
        let orphans = tokio::spawn(async move {
            orphans
                .run(&*leases, &holder, token.cancelled_owned())
                .await;
        });
        // Окончательное удаление необратимо, повтор безопасен (`expunge_final` пропускает удалённое): воркер
        // работает на каждом узле с ролью maintenance без лизы.
        let trash = self.trash.clone().map(|store| {
            let token = ctx.shutdown.clone();
            tokio::spawn(async move { store.run_trash_expiry(token.cancelled_owned()).await })
        });
        // Отключённые ящики: удаление по сроку ведёт один узел кластера — держатель лизы (APP-67).
        let disabled = self.disabled.clone().map(|disabled| {
            tokio::spawn(run_disabled(
                disabled,
                self.leases.clone(),
                self.holder.clone(),
                ctx.shutdown.clone(),
            ))
        });
        let purge = self
            .purge
            .clone()
            .map(|purge| tokio::spawn(purge.run(ctx.shutdown.clone())));
        let verify = self.verify.clone().map(|verify| {
            tokio::spawn(verify.run(
                self.leases.clone(),
                self.holder.clone(),
                ctx.shutdown.clone(),
            ))
        });
        // Выгрузка журнала аудита: один узел кластера — держатель лизы (APP-59).
        let audit_export = self.audit_export.clone().map(|export| {
            let token = ctx.shutdown.clone();
            tokio::spawn(async move { export.run(token).await })
        });
        // Пересчёт поисковых строк контактов (APP-63): старт роли не ждёт, ошибка — в журнал, прерванный
        // пересчёт продолжит следующий запуск.
        let card_reindex = self.card_reindex.clone().map(|meta| {
            let token = ctx.shutdown.clone();
            tokio::spawn(async move {
                tokio::select! {
                    () = token.cancelled() => {}
                    done = meta.reindex_cards(card_search) => {
                        if let Err(e) = done {
                            tracing::warn!(
                                error = %e,
                                "поисковые строки контактов не пересчитаны: повтор при следующем запуске роли"
                            );
                        }
                    }
                }
            })
        });
        // Книга GAL «Организация» (APP-49): первый проход при запуске, затем по периоду; один узел — держатель лизы.
        let gal = self.gal.clone().map(|gal| {
            let token = ctx.shutdown.clone();
            tokio::spawn(async move { gal.run(token).await })
        });
        ctx.ready.set(true);
        let gal = async move {
            match gal {
                Some(task) => task.await,
                None => Ok(()),
            }
        };
        let card_reindex = async move {
            match card_reindex {
                Some(task) => task.await,
                None => Ok(()),
            }
        };
        let audit_export = async move {
            match audit_export {
                Some(task) => task.await,
                None => Ok(()),
            }
        };
        let verify = async move {
            match verify {
                Some(task) => task.await,
                None => Ok(()),
            }
        };
        let trash = async move {
            match trash {
                Some(task) => task.await,
                None => Ok(()),
            }
        };
        let disabled = async move {
            match disabled {
                Some(task) => task.await,
                None => Ok(()),
            }
        };
        let purge = async move {
            match purge {
                Some(task) => task.await,
                None => Ok(()),
            }
        };
        let (gc, orphans, trash, disabled, purge, verify, audit_export, card_reindex, gal, ()) = tokio::join!(
            gc,
            orphans,
            trash,
            disabled,
            purge,
            verify,
            audit_export,
            card_reindex,
            gal,
            consume(
                self.events.clone(),
                self.sinks.clone(),
                self.signals.clone(),
                ctx.shutdown.clone(),
            ),
        );
        for (what, done) in [
            ("blob-gc", gc),
            ("blob-orphans", orphans),
            ("trash-expiry", trash),
            ("disabled-mailboxes", disabled),
            ("purge", purge),
            ("blob-verify", verify),
            ("audit-export", audit_export),
            ("card-reindex", card_reindex),
            ("gal", gal),
        ] {
            if let Err(e) = done {
                return Err(tp_contracts::Error::Internal(format!("{what}: {e}")));
            }
        }
        Ok(())
    }
}

/// Лиза окончательного удаления отключённых ящиков (APP-67): удаление ведёт один узел кластера.
pub const DISABLED_LEASE: &str = "disabled-mailboxes";

/// Срок лизы [`DISABLED_LEASE`]; держатель продлевает её каждую треть срока, без лизы узел пробует её получить с тем
/// же шагом.
const DISABLED_LEASE_TTL: Duration = Duration::from_secs(30);

/// Окончательное удаление отключённых ящиков по сроку в роли maintenance (APP-67): воркер
/// `DisabledMailboxes::run` работает, пока узел держит лизу [`DISABLED_LEASE`]. Потеря лизы (её получил другой узел
/// или продлить её не удалось до истечения) останавливает воркер — идущий проход прерывается, удаление ящика продолжит
/// следующий держатель (проход повторяем). Остановка роли останавливает воркер, после неё лиза отдаётся.
pub async fn run_disabled(
    disabled: DisabledMailboxes,
    leases: Arc<dyn LeaseStore>,
    holder: String,
    shutdown: CancellationToken,
) {
    let step = DISABLED_LEASE_TTL / 3;
    loop {
        let acquired = leases
            .try_acquire(DISABLED_LEASE, &holder, DISABLED_LEASE_TTL)
            .await;
        let mut lease = match acquired {
            Ok(Some(lease)) => lease,
            Ok(None) => {
                tracing::debug!(
                    lease = DISABLED_LEASE,
                    "отключённые ящики: удаление ведёт другой узел"
                );
                tokio::select! {
                    () = shutdown.cancelled() => return,
                    () = tokio::time::sleep(step) => continue,
                }
            }
            Err(e) => {
                tracing::warn!(lease = DISABLED_LEASE, error = %e, "отключённые ящики: лиза не получена");
                tokio::select! {
                    () = shutdown.cancelled() => return,
                    () = tokio::time::sleep(step) => continue,
                }
            }
        };
        tracing::info!(
            lease = DISABLED_LEASE,
            "отключённые ящики: удаление по сроку ведёт этот узел"
        );
        let worker = disabled.run(shutdown.clone().cancelled_owned());
        tokio::pin!(worker);
        let mut renew = tokio::time::interval_at(tokio::time::Instant::now() + step, step);
        renew.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let lost = loop {
            tokio::select! {
                () = &mut worker => break false,
                _ = renew.tick() => match leases.renew(&lease, DISABLED_LEASE_TTL).await {
                    Ok(Some(next)) => lease = next,
                    Ok(None) => {
                        tracing::warn!(lease = DISABLED_LEASE, "отключённые ящики: лиза потеряна, воркер остановлен");
                        break true;
                    }
                    // Повтор на следующем такте; лиза истекла — её мог получить другой узел.
                    Err(e) => {
                        tracing::warn!(lease = DISABLED_LEASE, error = %e, "отключённые ящики: лиза не продлена");
                        if (system_now())() >= lease.expires_at {
                            break true;
                        }
                    }
                },
            }
        };
        if !lost {
            // Воркер завершается только с остановкой роли: лиза отдаётся другим узлам.
            if let Err(e) = leases.release(&lease).await {
                tracing::warn!(lease = DISABLED_LEASE, error = %e, "отключённые ящики: лиза не отдана");
            }
            return;
        }
    }
}

/// Поисковые строки тела контакта для пересчёта — по правилу индекса записи (`tp_dav::index::card`); тело не
/// разбирается — `None`.
fn card_search(body: &str) -> Option<CardSearch> {
    tp_dav::index::card(body).ok().map(|i| i.search)
}

/// Чтение журнала событий подписчиками по сигналам до остановки; затем лизы разделов отдаются другим узлам.
async fn consume(
    events: EventConsumer,
    sinks: Vec<Arc<dyn EventSink>>,
    signals: LogSignals,
    shutdown: CancellationToken,
) {
    if sinks.is_empty() {
        return;
    }
    let mut reader = LogReader::new(events, signals);
    while let Some(work) = reader.wait(&shutdown).await {
        for sink in &sinks {
            if let Err(e) = reader.read(sink.as_ref(), &work).await {
                tracing::warn!(subscriber = sink.name(), error = %e, "журнал событий: пачка не обработана");
            }
        }
    }
    for sink in &sinks {
        reader.release(sink.as_ref()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use tp_contracts::events::{CoreEvent, CoreEventKind, EventLog, LoggedEvent};
    use tp_contracts::role::{ReadyHandle, fixed_config};
    use tp_contracts::{AccountId, TenantId};
    use tp_testkit::{Clock, MemBlobIndex, MemBlobStore, MemEventLog, MemLease, SystemClock};

    #[derive(Default)]
    struct Counting(AtomicUsize);

    #[async_trait]
    impl EventSink for Counting {
        fn name(&self) -> &str {
            "counting"
        }

        async fn handle(&self, batch: &[LoggedEvent]) -> Result<()> {
            self.0.fetch_add(batch.len(), Ordering::SeqCst);
            Ok(())
        }
    }

    fn ctx() -> (RoleCtx, CancellationToken, ReadyHandle) {
        let shutdown = CancellationToken::new();
        let ready = ReadyHandle::default();
        let ctx = RoleCtx {
            config: fixed_config(()),
            shutdown: shutdown.clone(),
            drain_deadline: Duration::from_secs(5),
            ready: ready.clone(),
            load: tp_contracts::load::fixed_load(tp_contracts::load::LoadLevel::Normal),
        };
        (ctx, shutdown, ready)
    }

    #[tokio::test]
    async fn pending_role_is_ready_until_shutdown() {
        let (ctx, shutdown, ready) = ctx();
        let task = tokio::spawn(async move { PendingRole(Role::Imap).run(ctx).await });
        while !ready.is_ready() {
            tokio::task::yield_now().await;
        }
        shutdown.cancel();
        task.await.expect("join").expect("run");
        assert_eq!(PendingRole(Role::Imap).name(), "imap");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn maintenance_consumes_events_and_stops() {
        let log = MemEventLog::new(4);
        let leases = MemLease::default();
        let sink = Arc::new(Counting::default());
        log.append(CoreEvent {
            tenant: TenantId::DEFAULT,
            account: AccountId::new(),
            mailbox: None,
            kind: CoreEventKind::MailboxCreated,
            at: SystemClock.now(),
        });
        let events = EventConsumer::new(
            Arc::new(log.clone()) as Arc<dyn EventLog>,
            Arc::new(leases.clone()),
            "node-a/1",
            tp_meta::events::EventsConfig::default(),
        );
        let role = Maintenance {
            index: Arc::new(MemBlobIndex::default()),
            store: Arc::new(MemBlobStore::default()),
            leases: Arc::new(leases),
            holder: "node-a/1".into(),
            events,
            sinks: vec![sink.clone()],
            signals: LogSignals {
                bus: Arc::new(tp_cluster::LocalBus::new(
                    &tp_cluster::LocalBusConfig::default(),
                )),
                members: None,
                lease_ttl: Duration::from_secs(30),
                resubscribe: Duration::from_secs(1),
            },
            gc: GcConfig::default(),
            orphans: OrphanConfig::default(),
            orphan_probe: OrphanProbe::default(),
            trash: None,
            disabled: None,
            purge: None,
            verify: None,
            audit_export: None,
            card_reindex: None,
            gal: None,
        };
        let (ctx, shutdown, ready) = ctx();
        let task = tokio::spawn(async move { role.run(ctx).await });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while sink.0.load(Ordering::SeqCst) == 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "событие не обработано"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(ready.is_ready());
        shutdown.cancel();
        task.await.expect("join").expect("run");
    }

    /// APP-44: проход по расписанию находит повреждённую запись учёта BLOB (объект пропал из хранилища) и
    /// «сироту», показатели `/metrics` растут; ничего не исправляется; узел без лизы проход не выполняет.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scheduled_verify_finds_damaged_blob() {
        use bytes::Bytes;
        use tp_blob::{BlobWriter, VerifyConfig, WriterConfig};
        use tp_contracts::{BlobKey, Gen};
        use tp_testkit::ManualClock;

        const GRACE: u64 = 3600;
        let clock = ManualClock::default();
        let index = MemBlobIndex::new(clock.shared());
        let store = MemBlobStore::new(clock.shared());
        let writer = BlobWriter::new(
            Arc::new(index.clone()),
            Arc::new(store.clone()),
            WriterConfig::default(),
        );
        let inventory = Arc::new(crate::testutil::MemInventory::new(index.clone()));
        let mut keys = Vec::new();
        for body in [&b"ok"[..], b"lost"] {
            let key = writer
                .write(TenantId::DEFAULT, Bytes::from_static(body), 1)
                .await
                .expect("write");
            keys.push(key);
        }
        inventory
            .keys
            .lock()
            .expect("lock")
            .extend(keys.iter().copied());
        let (ok, lost) = (keys[0], keys[1]);
        store.delete(&lost).await.expect("delete");
        let orphan = BlobKey {
            generation: Gen(7),
            ..ok
        };
        store
            .put(&orphan, Bytes::from_static(b"ok"))
            .await
            .expect("put");
        clock.advance(Duration::from_secs(GRACE + 1));

        let metrics = tp_platform::Metrics::default();
        let probe = crate::metrics::register_verify(&metrics);
        let c = clock.clone();
        let verify = ScheduledVerify {
            verifier: Arc::new(Verifier::new(
                Arc::new(index.clone()),
                inventory.clone(),
                Arc::new(store.clone()),
                VerifyConfig {
                    grace_secs: GRACE,
                    ..VerifyConfig::default()
                },
                Arc::new(move || c.now()),
            )),
            // Сетка проходов — каждые 200 мс.
            schedule: VerifySchedule {
                at: Duration::ZERO,
                interval: Duration::from_millis(200),
                ..VerifySchedule::default()
            },
            probe: probe.clone(),
            now: system_now(),
        };
        let text = metrics.encode().expect("encode");
        assert!(text.contains("tp_blob_verify_passes_total 0"), "{text}");
        assert!(text.contains("tp_blob_verify_missing 0"), "{text}");
        assert!(text.contains("tp_blob_verify_size_mismatch 0"), "{text}");

        // Лиза у другого узла: прохода нет.
        let leases = MemLease::default();
        let lease_name = verify.schedule.lease_name.clone();
        let other = leases
            .try_acquire(&lease_name, "node-b/1", Duration::from_secs(60))
            .await
            .expect("acquire")
            .expect("free");
        assert!(matches!(
            verify.pass(&leases, "node-a/1").await,
            VerifyPass::NotLeader
        ));
        assert_eq!(probe.metrics(), VerifyMetrics::default());
        leases.release(&other).await.expect("release");

        // GC и поиск «сирот» роли — на своём пустом хранилище: «сироту» этого теста они бы удалили.
        let role = Maintenance {
            index: Arc::new(MemBlobIndex::default()),
            store: Arc::new(MemBlobStore::default()),
            leases: Arc::new(leases.clone()),
            holder: "node-a/1".into(),
            events: EventConsumer::new(
                Arc::new(MemEventLog::new(1)) as Arc<dyn EventLog>,
                Arc::new(leases.clone()),
                "node-a/1",
                tp_meta::events::EventsConfig::default(),
            ),
            sinks: Vec::new(),
            signals: LogSignals {
                bus: Arc::new(tp_cluster::LocalBus::new(
                    &tp_cluster::LocalBusConfig::default(),
                )),
                members: None,
                lease_ttl: Duration::from_secs(30),
                resubscribe: Duration::from_secs(1),
            },
            gc: GcConfig::default(),
            orphans: OrphanConfig::default(),
            orphan_probe: OrphanProbe::default(),
            trash: None,
            disabled: None,
            purge: None,
            verify: Some(verify),
            audit_export: None,
            card_reindex: None,
            gal: None,
        };
        let (ctx, shutdown, _ready) = ctx();
        let task = tokio::spawn(async move { role.run(ctx).await });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        // Второй проход — по расписанию снова, тем же держателем лизы.
        while probe.metrics().passes < 2 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "проход по расписанию не выполнен"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        shutdown.cancel();
        task.await.expect("join").expect("run");

        let m = probe.metrics();
        assert_eq!(
            (m.missing, m.size_mismatch, m.orphans, m.errors, m.failed),
            (1, 0, 1, 0, 0)
        );
        assert!(m.finished_at > 0);
        let text = metrics.encode().expect("encode");
        assert!(text.contains("tp_blob_verify_missing 1"), "{text}");
        assert!(text.contains("tp_blob_verify_size_mismatch 0"), "{text}");
        assert!(text.contains("tp_blob_verify_orphans 1"), "{text}");
        assert!(!text.contains("tp_blob_verify_passes_total 0"), "{text}");
        assert_eq!(leases.holder(&lease_name).as_deref(), Some("node-a/1"));
        // Без --fix: «сирота» на месте, запись не помечена.
        assert!(store.contains(&orphan));
        assert!(inventory.damaged.lock().expect("lock").is_empty());
    }

    /// APP-32, APP-39: проход очистки удаляет надгробия DAV, записи журнала автоответов и журнала доставки старше
    /// срока из `meta` и токены `admin init`, истёкшие раньше `admin_token_retention`; свежие остаются; роль maintenance выполняет проход при запуске. Отдельная база: очистка — по всем организациям.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn purge_removes_expired_records() {
        use tp_contracts::admin_token::LocalAdminToken;
        use tp_contracts::dav::{Collection, CollectionKind, DavStore};
        use tp_contracts::delivery::{
            DeliveryDecision, DeliveryEvent, DeliveryEventKind, DeliveryLog, DeliveryQuery,
        };
        use tp_contracts::membership::NodeId;
        use tp_contracts::meta::{AccountKind, AccountScope, AccountStore, AccountUpsert};
        use tp_contracts::sieve::VacationLog;
        use tp_contracts::{DomainId, TenantId};

        let Some(url) = tp_testkit::pg_url() else {
            eprintln!("TEST_DATABASE_URL не задан — тест пропущен");
            return;
        };
        let url = crate::testutil::dev_url(&url);
        let (db_name, url_db) = crate::testutil::create_database(&url).await;
        let pool = crate::testutil::db_pool(&url_db).await;
        tp_meta::schema::migrate(&pool).await.expect("migrate");
        let ttl = Duration::from_secs(2);
        let mut cfg = tp_meta::MetaConfig::default();
        cfg.dav.tombstone_ttl = ttl;
        cfg.sieve.vacation_log_ttl = ttl;
        let meta = PgMeta::new(pool.clone(), cfg);

        let (tenant, domain) = (TenantId::new(), DomainId::new());
        let mut tx = tp_meta::schema::begin(&pool, tenant).await.expect("begin");
        sqlx::query("INSERT INTO tenants (tenant_id, name) VALUES ($1, 'test')")
            .bind(tenant.as_uuid())
            .execute(&mut *tx)
            .await
            .expect("tenant");
        sqlx::query(
            "INSERT INTO domains (tenant_id, domain_id, name) VALUES ($1, $2, 'purge.test')",
        )
        .bind(tenant.as_uuid())
        .bind(domain.as_uuid())
        .execute(&mut *tx)
        .await
        .expect("domain");
        tx.commit().await.expect("commit");
        let account = meta
            .upsert(
                tenant,
                AccountUpsert {
                    domain,
                    login: "user@purge.test".into(),
                    display_name: "User".into(),
                    external_id: None,
                    enabled: true,
                    aliases: Vec::new(),
                    kind: AccountKind::User,
                },
            )
            .await
            .expect("upsert");
        let scope = AccountScope {
            tenant,
            account: account.id,
        };
        // Надгробие — удалённая коллекция; запись журнала — отметка об автоответе отправителю.
        let tombstone = |name: &'static str| {
            let meta = meta.clone();
            async move {
                let book = Collection::new(CollectionKind::AddressBook, name);
                let id = book.id;
                meta.create_collection(scope, book).await.expect("create");
                meta.delete_collection(scope, id).await.expect("delete");
            }
        };
        let tombstones = || {
            let pool = pool.clone();
            async move {
                let mut tx = tp_meta::schema::begin(&pool, tenant).await.expect("begin");
                let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM dav_tombstones")
                    .fetch_one(&mut *tx)
                    .await
                    .expect("count");
                n
            }
        };
        let day = Duration::from_secs(24 * 3600);
        tombstone("old").await;
        assert!(
            !meta
                .seen(scope, "old@example.net", "", day)
                .await
                .expect("seen")
        );
        tokio::time::sleep(ttl + Duration::from_millis(500)).await;
        tombstone("fresh").await;
        assert!(
            !meta
                .seen(scope, "fresh@example.net", "", day)
                .await
                .expect("seen")
        );
        assert_eq!(tombstones().await, 2);

        // APP-39: журнал доставки (срок `meta.delivery.retention` — 30 сут) и токены `admin init`.
        let now = (system_now())();
        let delivery = |at, sender: &str| DeliveryEvent {
            tenant,
            at,
            node: NodeId::new("node-a").expect("node"),
            kind: DeliveryEventKind::Receive,
            sender: sender.into(),
            recipients: vec!["user@purge.test".into()],
            message_id: None,
            queue_id: None,
            decision: DeliveryDecision::Accept,
            reason: None,
        };
        DeliveryLog::record(&meta, delivery(now - 40 * day, "old@example.net"))
            .await
            .expect("record");
        DeliveryLog::record(&meta, delivery(now, "fresh@example.net"))
            .await
            .expect("record");
        let delivery_senders = || {
            let meta = meta.clone();
            async move {
                DeliveryLog::search(&meta, tenant, &DeliveryQuery::all(10))
                    .await
                    .expect("search")
                    .events
                    .into_iter()
                    .map(|e| e.sender)
                    .collect::<Vec<_>>()
            }
        };
        assert_eq!(delivery_senders().await.len(), 2);
        // Токены: истёк 31 сут назад (погашен), истёк вчера, действующий.
        let token = |id: &str, hash: &str, expires_at| LocalAdminToken {
            id: id.into(),
            tenant,
            account: account.id,
            hash: hash.repeat(64),
            created_at: now - 60 * day,
            expires_at,
            used_at: None,
            revoked_at: None,
        };
        for t in [
            token("old", "a", now - 31 * day),
            token("recent", "b", now - day),
            token("active", "c", now + day),
        ] {
            LocalAdminTokens::issue(&meta, t).await.expect("issue");
        }
        let token_ids = || {
            let meta = meta.clone();
            async move {
                LocalAdminTokens::list(&meta, tenant)
                    .await
                    .expect("list")
                    .into_iter()
                    .map(|t| t.id)
                    .collect::<std::collections::BTreeSet<_>>()
            }
        };
        assert_eq!(token_ids().await.len(), 3);

        let purge = Purge {
            meta: meta.clone(),
            interval: Duration::from_secs(3600),
            admin_token_retention: 30 * day,
            probe: PurgeProbe::new(),
            itip: None,
        };
        let done = purge.step().await;
        // APP-52: успешный шаг — удалённые записи и момент прохода в показателях.
        let label = [("step", "purge_dav_tombstones")];
        assert_eq!(purge.probe.purged.get_or_create(&label).get(), 1);
        assert!(purge.probe.last_success.get_or_create(&label).get() > 0);
        assert_eq!(purge.probe.errors.get_or_create(&label).get(), 0);
        assert_eq!(done.dav_tombstones.expect("dav"), 1);
        assert_eq!(done.vacation_log.expect("vacation"), 1);
        done.delivery_log.expect("delivery");
        assert_eq!(done.admin_tokens.expect("tokens"), 1);
        assert_eq!(
            delivery_senders().await,
            ["fresh@example.net"],
            "старая запись журнала доставки удалена, свежая на месте"
        );
        assert_eq!(
            token_ids().await,
            ["active", "recent"].map(String::from).into(),
            "токен, истёкший раньше срока хранения, удалён; остальные на месте"
        );
        assert_eq!(tombstones().await, 1, "свежее надгробие на месте");
        assert!(
            meta.seen(scope, "fresh@example.net", "", day)
                .await
                .expect("seen"),
            "свежая запись журнала на месте"
        );
        assert!(
            !meta
                .seen(scope, "old@example.net", "", day)
                .await
                .expect("seen"),
            "старая запись удалена: отправителю снова отвечают"
        );

        // Роль: проход при запуске — после срока уходит и второе надгробие.
        tokio::time::sleep(ttl + Duration::from_millis(500)).await;
        let leases = MemLease::default();
        let role = Maintenance {
            index: Arc::new(MemBlobIndex::default()),
            store: Arc::new(MemBlobStore::default()),
            leases: Arc::new(leases.clone()),
            holder: "node-a/1".into(),
            events: EventConsumer::new(
                Arc::new(MemEventLog::new(1)) as Arc<dyn EventLog>,
                Arc::new(leases),
                "node-a/1",
                tp_meta::events::EventsConfig::default(),
            ),
            sinks: Vec::new(),
            signals: LogSignals {
                bus: Arc::new(tp_cluster::LocalBus::new(
                    &tp_cluster::LocalBusConfig::default(),
                )),
                members: None,
                lease_ttl: Duration::from_secs(30),
                resubscribe: Duration::from_secs(1),
            },
            gc: GcConfig::default(),
            orphans: OrphanConfig::default(),
            orphan_probe: OrphanProbe::default(),
            trash: None,
            disabled: None,
            purge: Some(purge),
            verify: None,
            audit_export: None,
            card_reindex: None,
            gal: None,
        };
        let (ctx, shutdown, _ready) = ctx();
        let task = tokio::spawn(async move { role.run(ctx).await });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while tombstones().await != 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "роль не выполнила очистку"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        shutdown.cancel();
        task.await.expect("join").expect("run");
        pool.close().await;
        crate::testutil::drop_database(&url, &db_name).await;
    }
}
