//! Хранение отключённых ящиков (SRS 4.7; решение 2026-11-04-exchange-parity «Удалённые ящики»; CONTRACTS.md#Metadata).
//!
//! Ящик учётной записи, исчезнувшей из каталога, отключает синхронизация каталога (`AccountStore::disable_account`)
//! и хранит до срока `AccountState::Disabled::until`. [`DisabledMailboxes`] — что происходит с ним дальше:
//!
//! - окончательное удаление по сроку ([`DisabledMailboxes::expire`]; воркер [`DisabledMailboxes::run`] роли
//!   maintenance): `AccountStore::expired_accounts` пакетами → для каждого письма ящика, включая корзину и скрытые,
//!   `DecisionHooks::before_final_delete` (`FinalDelete::mailbox` — папка письма, для корзины — исходная) →
//!   письма папок переносятся в корзину и удаляются `MessageStore::expunge_final` (счётчики ссылок объектов
//!   уменьшает он; объекты удаляет GC BLOB) → `AccountStore::remove_disabled`. Запрет хука хотя бы для одного письма
//!   (Legal Hold, политика хранения) или его ошибка — ящик не трогается, срок переносится на
//!   [`DisabledConfig::defer`] (`AccountStore::disable_account`): удаление необратимо, при сомнении ящик сохраняется.
//!   Проход повторяем: прерванное удаление продолжается со следующего прохода (письма, уже перенесённые в корзину,
//!   удаляются из неё), ящик, восстановленный до начала удаления, пропускается;
//! - восстановление ([`DisabledMailboxes::restore`]) и подключение к другой учётной записи
//!   ([`DisabledMailboxes::attach`]): после `AccountStore::attach_account` письма могли оказаться в корзине
//!   подключённого ящика, поэтому публикуется системный сигнал `TrashChanged` (таймер истечения корзины).
//!
//! Воркер не делает периодических проходов (SRS 2.8): один таймер на ближайший срок хранения. Срок сверяется при
//! запуске, после прохода, после `restore` и `attach` этого объекта (и его клонов) и не реже [`DisabledConfig::max_wait`]:
//! сигнала шины об отключении ящика в контрактах нет, отключение другим узлом или синхронизацией каталога воркер
//! замечает при очередной сверке. После ошибки хранилища — повтор не раньше [`DisabledConfig::retry`].

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use time::OffsetDateTime;
use tp_contracts::config::duration;
use tp_contracts::ext::{Decision, FinalDelete};
use tp_contracts::meta::{
    Account, AccountScope, AccountState, AccountStore, ExistingContent, MailboxStore, MessageQuery,
    SpecialUse,
};
use tp_contracts::retention::{TrashItem, TrashObject, trash_changed};
use tp_contracts::{AccountId, Error, MailboxId, MessageId, Result, TenantId, Uid};

use crate::Store;
use crate::trash::{FAR, SystemTimer, Timer, after};

/// Хранение отключённых ящиков (секция `disabled` конфигурации хранилища).
///
/// ```yaml
/// max_wait: 1h
/// retry: 5m
/// batch: 100
/// page: 500
/// defer: 1d
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DisabledConfig {
    /// Наибольшее ожидание таймера: сверка ближайшего срока хранения не реже (отключение ящика сигналом не сообщается).
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub max_wait: Duration,
    /// Пауза перед повтором после ошибки хранилища (ящики с истёкшим сроком не обработаны).
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub retry: Duration,
    /// Ящиков за один запрос `AccountStore::expired_accounts`.
    pub batch: usize,
    /// Писем за один запрос к хранилищу при проверке и удалении писем ящика.
    pub page: usize,
    /// Отсрочка окончательного удаления ящика после запрета или ошибки точки принятия решений.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub defer: Duration,
}

impl Default for DisabledConfig {
    fn default() -> Self {
        Self {
            max_wait: Duration::from_secs(3600),
            retry: Duration::from_secs(300),
            batch: 100,
            page: 500,
            defer: Duration::from_secs(86_400),
        }
    }
}

/// Итог прохода окончательного удаления отключённых ящиков.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DisabledPass {
    /// Получено ящиков с истёкшим сроком хранения.
    pub expired: usize,
    /// Удалено окончательно.
    pub removed: usize,
    /// Отложено (запрет или ошибка точки принятия решений).
    pub deferred: usize,
    /// Пропущено: ящик восстановлен, подключён или его срок перенесён после запроса.
    pub skipped: usize,
    /// Не обработано из-за ошибки хранилища — повторятся в следующем проходе.
    pub failed: usize,
}

/// Что сделано с одним ящиком с истёкшим сроком.
enum Outcome {
    Removed,
    Deferred,
    Skipped,
}

/// Отключённые ящики: окончательное удаление по сроку, восстановление, подключение к другой учётной записи.
#[derive(Clone)]
pub struct DisabledMailboxes {
    store: Store,
    accounts: Arc<dyn AccountStore>,
    mailboxes: Arc<dyn MailboxStore>,
    /// Сигнал воркеру: `restore` или `attach` выполнены, ближайший срок мог измениться (общий для клонов).
    wake: Arc<tokio::sync::Notify>,
}

impl std::fmt::Debug for DisabledMailboxes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DisabledMailboxes")
            .field("store", &self.store)
            .finish_non_exhaustive()
    }
}

impl DisabledMailboxes {
    /// Поверх хранилища `store` (письма, корзина, точки принятия решений, шина, конфигурация `disabled`),
    /// учётных записей и папок.
    pub fn new(
        store: Store,
        accounts: Arc<dyn AccountStore>,
        mailboxes: Arc<dyn MailboxStore>,
    ) -> Self {
        Self {
            store,
            accounts,
            mailboxes,
            wake: Arc::default(),
        }
    }

    fn cfg(&self) -> &DisabledConfig {
        &self.store.cfg.disabled
    }

    /// Восстанавливает отключённый ящик (`AccountStore::restore_account`: действующий — без изменений,
    /// нет учётной записи — `Error::NotFound`); воркер сверяет ближайший срок.
    pub async fn restore(&self, tenant: TenantId, account: AccountId) -> Result<()> {
        self.accounts.restore_account(tenant, account).await?;
        self.wake.notify_one();
        Ok(())
    }

    /// Подключает отключённый ящик `disabled` к учётной записи `to` (`AccountStore::attach_account`; содержимое
    /// ящика `to` — по `existing`, ошибки — его же). Возвращает учётную запись подключённого ящика. После фиксации —
    /// системный сигнал `TrashChanged` этого ящика: в его корзине могли появиться письма ящика `to` со своими сроками.
    pub async fn attach(
        &self,
        tenant: TenantId,
        disabled: AccountId,
        to: AccountId,
        existing: Option<ExistingContent>,
    ) -> Result<Account> {
        let account = self
            .accounts
            .attach_account(tenant, disabled, to, existing)
            .await?;
        let scope = AccountScope {
            tenant,
            account: account.id,
        };
        self.store.trash_wake.notify_one();
        if let Err(e) = self.store.d.bus.publish(trash_changed(scope)).await {
            tracing::warn!(error = %e, "mailstore: сигнал корзины не опубликован");
        }
        self.wake.notify_one();
        Ok(account)
    }

    /// Проход окончательного удаления на момент `now`: пакеты ящиков со сроком хранения не позже `now`, пока они есть
    /// и проход продвигается. Ошибка — только у запроса `expired_accounts`; ошибки отдельных ящиков —
    /// в [`DisabledPass::failed`].
    pub async fn expire(&self, now: OffsetDateTime) -> Result<DisabledPass> {
        let batch = self.cfg().batch.max(1);
        let mut pass = DisabledPass::default();
        loop {
            let scopes = self.accounts.expired_accounts(now, batch).await?;
            let full = scopes.len() >= batch;
            let failed = pass.failed;
            pass.expired += scopes.len();
            for scope in scopes {
                match self.expire_account(now, scope).await {
                    Ok(Outcome::Removed) => {
                        tracing::info!(account = %scope.account, "mailstore: отключённый ящик удалён окончательно");
                        pass.removed += 1;
                    }
                    Ok(Outcome::Deferred) => pass.deferred += 1,
                    Ok(Outcome::Skipped) => pass.skipped += 1,
                    Err(e) => {
                        tracing::warn!(account = %scope.account, error = %e, "mailstore: отключённый ящик не удалён");
                        pass.failed += 1;
                    }
                }
            }
            // Необработанные ящики остаются в выборке: без продвижения следующий пакет был бы тем же.
            if !full || pass.failed > failed {
                return Ok(pass);
            }
        }
    }

    /// Воркер окончательного удаления до завершения `shutdown`: один таймер на ближайший срок хранения.
    pub async fn run(&self, shutdown: impl Future<Output = ()>) {
        self.worker(&SystemTimer, shutdown).await;
    }

    pub(crate) async fn worker(&self, timer: &impl Timer, shutdown: impl Future<Output = ()>) {
        tokio::pin!(shutdown);
        // После ошибки хранилища — не раньше этого момента (иначе необработанный ящик с истёкшим сроком
        // взводил бы таймер в прошлое).
        let mut not_before: Option<OffsetDateTime> = None;
        loop {
            let due = match self.next_expiry().await {
                Ok(next) => next.map(|at| not_before.map_or(at, |nb| at.max(nb))),
                Err(e) => {
                    tracing::warn!(error = %e, "mailstore: ближайший срок отключённых ящиков не получен");
                    let at = after(timer.now(), self.cfg().retry);
                    not_before = Some(at);
                    Some(at)
                }
            };
            // Не дольше `max_wait`: по его истечении — только сверка срока.
            let cap = after(timer.now(), self.cfg().max_wait);
            let (at, pass) = match due {
                Some(at) if at <= cap => (at, true),
                _ => (cap, false),
            };
            let fire = tokio::select! {
                () = &mut shutdown => return,
                // Восстановление или подключение этим узлом: сверить срок и перевзвести таймер.
                () = self.wake.notified() => false,
                () = timer.sleep_until(at) => pass,
            };
            if fire {
                let now = timer.now();
                not_before = match self.expire(now).await {
                    Ok(p) => {
                        if p.expired > 0 {
                            tracing::info!(
                                removed = p.removed,
                                deferred = p.deferred,
                                skipped = p.skipped,
                                failed = p.failed,
                                "mailstore: истечение срока отключённых ящиков"
                            );
                        }
                        (p.failed > 0).then(|| after(now, self.cfg().retry))
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "mailstore: удаление отключённых ящиков не выполнено");
                        Some(after(now, self.cfg().retry))
                    }
                };
            }
        }
    }

    /// Ближайший срок хранения среди отключённых ящиков всех организаций; их нет — `None`.
    async fn next_expiry(&self) -> Result<Option<OffsetDateTime>> {
        let first = self.accounts.expired_accounts(FAR, 1).await?;
        let Some(scope) = first.first() else {
            return Ok(None);
        };
        match self.until(*scope).await? {
            Some(until) => Ok(Some(until)),
            // Ящик восстановлен или удалён между запросами: состояние изменилось — сверить заново.
            None => Err(Error::Conflict),
        }
    }

    /// Срок хранения отключённого ящика; ящик действует или его нет — `None`.
    async fn until(&self, scope: AccountScope) -> Result<Option<OffsetDateTime>> {
        match self
            .accounts
            .account_state(scope.tenant, scope.account)
            .await
        {
            Ok(AccountState::Disabled { until }) => Ok(Some(until)),
            Ok(AccountState::Active) | Err(Error::NotFound) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn expire_account(&self, now: OffsetDateTime, scope: AccountScope) -> Result<Outcome> {
        let boxes: Vec<MailboxId> = self
            .mailboxes
            .list(scope)
            .await?
            .into_iter()
            .filter(|m| m.role != Some(SpecialUse::Trash))
            .map(|m| m.id)
            .collect();
        if !self.allowed(scope, &boxes).await? {
            let until = time::Duration::try_from(self.cfg().defer)
                .ok()
                .and_then(|d| now.checked_add(d))
                .ok_or_else(|| Error::internal("срок отсрочки удаления ящика вне диапазона"))?;
            self.accounts
                .disable_account(scope.tenant, scope.account, until)
                .await?;
            return Ok(Outcome::Deferred);
        }
        // Перед необратимым шагом: ящик всё ещё отключён и срок не перенесён (восстановление, подключение).
        if self.until(scope).await?.is_none_or(|until| until > now) {
            return Ok(Outcome::Skipped);
        }
        for mailbox in boxes {
            self.drain(scope, Source::Mailbox(mailbox)).await?;
        }
        self.drain(scope, Source::Trash).await?;
        self.drain(scope, Source::Hidden).await?;
        self.accounts
            .remove_disabled(scope.tenant, scope.account)
            .await?;
        Ok(Outcome::Removed)
    }

    /// Точка принятия решений для каждого письма ящика: `true` — удаление разрешено для всех. Запрет или ошибка
    /// хука — `false` (остальные письма не спрашиваются); ошибка хранилища — ошибка.
    async fn allowed(&self, scope: AccountScope, boxes: &[MailboxId]) -> Result<bool> {
        let page = self.cfg().page.max(1);
        for mailbox in boxes {
            let mut from = Some(Uid::MIN);
            while let Some(start) = from {
                let query = MessageQuery::Uids(vec![start..=Uid(u32::MAX)]);
                let found = self
                    .store
                    .d
                    .messages
                    .list(scope, *mailbox, query, page)
                    .await?;
                for m in &found {
                    let op = FinalDelete {
                        scope,
                        mailbox: *mailbox,
                        message: m.id,
                        received_at: m.received_at,
                    };
                    if !self.ask(&op).await {
                        return Ok(false);
                    }
                }
                from = match found.last() {
                    Some(last) if found.len() >= page => last.uid.next(),
                    _ => None,
                };
            }
        }
        for hidden in [false, true] {
            let mut offset = 0;
            loop {
                let trash = &self.store.d.trash;
                let items = if hidden {
                    trash.list_hidden(scope, offset, page).await?
                } else {
                    trash.list(scope, offset, page).await?
                };
                for item in &items {
                    let TrashObject::Message(message) = item.object else {
                        // Объекты, появившиеся позже Ф1, этот воркер удалять не умеет: ящик сохраняется.
                        tracing::warn!(account = %scope.account, "mailstore: в корзине отключённого ящика неизвестный объект");
                        return Ok(false);
                    };
                    let op = FinalDelete {
                        scope,
                        mailbox: item.origin,
                        message,
                        received_at: item.received_at,
                    };
                    if !self.ask(&op).await {
                        return Ok(false);
                    }
                }
                if items.len() < page {
                    break;
                }
                offset += items.len();
            }
        }
        Ok(true)
    }

    /// Решение точки принятия решений по письму: `false` — запрет или ошибка хука.
    async fn ask(&self, op: &FinalDelete) -> bool {
        match self.store.d.hooks.before_final_delete(op).await {
            Ok(Decision::Deny(reason)) => {
                tracing::info!(account = %op.scope.account, %reason, "mailstore: удаление отключённого ящика отложено точкой принятия решений");
                false
            }
            Ok(_) => true,
            Err(e) => {
                tracing::warn!(account = %op.scope.account, error = %e, "mailstore: точка принятия решений не ответила, удаление ящика отложено");
                false
            }
        }
    }

    /// Страницами выбирает письма источника и убирает их, пока выборка не опустеет: письма папки переносятся
    /// в корзину, письма корзины (видимые и скрытые) удаляются окончательно. Выборка, не изменившаяся после шага, —
    /// ошибка (иначе цикл не завершился бы).
    async fn drain(&self, scope: AccountScope, source: Source) -> Result<()> {
        let page = self.cfg().page.max(1);
        let messages = &self.store.d.messages;
        let trash = &self.store.d.trash;
        let mut prev: Option<MessageId> = None;
        loop {
            let ids: Vec<MessageId> = match source {
                Source::Mailbox(mailbox) => messages
                    .list(scope, mailbox, MessageQuery::All, page)
                    .await?
                    .into_iter()
                    .map(|m| m.id)
                    .collect(),
                Source::Trash => message_ids(trash.list(scope, 0, page).await?),
                Source::Hidden => message_ids(trash.list_hidden(scope, 0, page).await?),
            };
            let Some(first) = ids.first().copied() else {
                return Ok(());
            };
            if prev == Some(first) {
                return Err(Error::internal(
                    "отключённый ящик: письма не убираются из выборки",
                ));
            }
            prev = Some(first);
            match source {
                Source::Mailbox(_) => messages.to_trash(scope, &ids).await?,
                Source::Trash | Source::Hidden => messages.expunge_final(scope, &ids).await?,
            };
        }
    }
}

/// Откуда выбираются письма удаляемого ящика.
#[derive(Clone, Copy)]
enum Source {
    /// Папка (не корзина).
    Mailbox(MailboxId),
    /// Корзина, видимые письма.
    Trash,
    /// Корзина, скрытые письма.
    Hidden,
}

/// Письма элементов корзины (объекты других видов `allowed` до удаления не допускает).
fn message_ids(items: Vec<TrashItem>) -> Vec<MessageId> {
    items
        .into_iter()
        .filter_map(|i| match i.object {
            TrashObject::Message(id) => Some(id),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use futures::StreamExt;
    use tp_contracts::bus::{EventBus, EventKind};
    use tp_contracts::ext::{DecisionHook, DecisionHooks};
    use tp_contracts::mailstore::{Delivery, IngestRequest, MailStore};
    use tp_contracts::meta::{Flags, MessageStore};
    use tp_contracts::retention::TrashStore;

    use super::*;
    use crate::testenv::{Env, TestTimer, stream, until};

    fn days(n: i64) -> time::Duration {
        time::Duration::days(n)
    }

    fn disabled(env: &Env) -> DisabledMailboxes {
        DisabledMailboxes::new(
            env.store.clone(),
            Arc::new(env.meta.clone()),
            Arc::new(env.meta.clone()),
        )
    }

    async fn inbox(env: &Env, scope: AccountScope) -> MailboxId {
        env.meta
            .mailbox_by_role(scope, SpecialUse::Inbox)
            .expect("inbox")
    }

    /// Доставляет письмо корпуса во «Входящие».
    async fn deliver(env: &Env, scope: AccountScope, at: OffsetDateTime) -> MessageId {
        let raw = tp_testkit::corpus::corpus_mail("mixed-pdf")
            .expect("mail")
            .raw;
        let done = env
            .store
            .ingest(IngestRequest {
                tenant: scope.tenant,
                mail_from: String::new(),
                raw: stream(&raw, 4096),
                deliveries: vec![Delivery {
                    account: scope.account,
                    mailbox: inbox(env, scope).await,
                    extra_headers: vec![],
                    flags: Flags::new(),
                }],
                received_at: at,
            })
            .await
            .expect("ingest");
        done.delivered[0].message
    }

    /// Счётчики ссылок объектов письма.
    async fn refs(env: &Env, scope: AccountScope, id: MessageId) -> Vec<u32> {
        let m = MessageStore::get(&env.meta, scope, id)
            .await
            .expect("get")
            .expect("msg");
        std::iter::once(m.skeleton)
            .chain(m.parts.iter().map(|p| p.blob))
            .map(|k| env.index.row(k.tenant, &k.hash).expect("row").refcnt)
            .collect()
    }

    async fn state(env: &Env, scope: AccountScope) -> Result<AccountState> {
        env.meta.account_state(scope.tenant, scope.account).await
    }

    /// Ящик с письмом во «Входящих», письмом в корзине и скрытым письмом; отключён до `now + 30 суток`.
    async fn disabled_mailbox(env: &Env, now: OffsetDateTime) -> (AccountScope, [MessageId; 3]) {
        let scope = env.meta.account("gone@example.org").await.expect("acc");
        let ids = [
            deliver(env, scope, now).await,
            deliver(env, scope, now).await,
            deliver(env, scope, now).await,
        ];
        env.store
            .delete_to_trash(scope, &ids[1..])
            .await
            .expect("trash");
        let out = env
            .trash
            .purge(scope, &ids[2..], &DecisionHooks::default())
            .await
            .expect("hide");
        assert_eq!(out.hidden, [ids[2]]);
        env.meta
            .disable_account(scope.tenant, scope.account, now + days(30))
            .await
            .expect("disable");
        (scope, ids)
    }

    /// Приёмка MS-17: по истечении срока ящик удаляется окончательно — письма папок, корзины и скрытые,
    /// ссылки на объекты сняты, учётной записи нет.
    #[tokio::test]
    async fn removed_at_deadline() {
        let mut cfg = crate::Config::default();
        // Письма выбираются страницами по одному, ящики — пакетами по одному.
        cfg.disabled.page = 1;
        cfg.disabled.batch = 1;
        let env = Env::with_config(cfg, false);
        let now = OffsetDateTime::now_utc();
        let other = env.meta.account("stays@example.org").await.expect("acc");
        let kept = deliver(&env, other, now).await;
        let (scope, _) = disabled_mailbox(&env, now).await;
        assert!(refs(&env, other, kept).await.iter().all(|&r| r == 4));
        let d = disabled(&env);

        let early = d.expire(now + days(29)).await;
        assert_eq!(early, Ok(DisabledPass::default()));
        assert!(matches!(
            state(&env, scope).await,
            Ok(AccountState::Disabled { .. })
        ));

        let pass = d.expire(now + days(30)).await.expect("expire");
        assert_eq!(
            pass,
            DisabledPass {
                expired: 1,
                removed: 1,
                ..DisabledPass::default()
            }
        );
        assert_eq!(state(&env, scope).await, Err(Error::NotFound));
        assert_eq!(
            AccountStore::get(&env.meta, scope.tenant, scope.account).await,
            Ok(None)
        );
        assert!(
            refs(&env, other, kept).await.iter().all(|&r| r == 1),
            "ссылки писем удалённого ящика сняты"
        );
        assert_eq!(state(&env, other).await, Ok(AccountState::Active));

        // Повтор безопасен.
        let again = d.expire(now + days(30)).await;
        assert_eq!(again, Ok(DisabledPass::default()));
    }

    /// Legal Hold на одно письмо: запрещает его окончательное удаление и запоминает запросы.
    struct Hold {
        held: Mutex<Option<MessageId>>,
        calls: Mutex<Vec<FinalDelete>>,
    }

    #[async_trait::async_trait]
    impl DecisionHook for Hold {
        fn name(&self) -> &str {
            "hold"
        }

        async fn before_final_delete(&self, op: &FinalDelete) -> Result<Decision> {
            self.calls.lock().expect("lock").push(op.clone());
            Ok(if *self.held.lock().expect("lock") == Some(op.message) {
                Decision::Deny("legal hold".into())
            } else {
                Decision::Allow
            })
        }
    }

    /// Приёмка MS-17: запрет точки принятия решений — ящик цел, срок перенесён; запрет снят — ящик удаляется.
    #[tokio::test]
    async fn deny_defers() {
        let hold = Arc::new(Hold {
            held: Mutex::new(None),
            calls: Mutex::new(Vec::new()),
        });
        let env = Env::with_hooks(DecisionHooks::new(vec![hold.clone()]));
        let now = OffsetDateTime::now_utc();
        let (scope, ids) = disabled_mailbox(&env, now).await;
        // Под запретом — скрытое письмо: спрашиваются все письма ящика, а не только видимые.
        *hold.held.lock().expect("lock") = Some(ids[2]);
        let before = refs(&env, scope, ids[0]).await;
        let d = disabled(&env);

        let at = now + days(30);
        let pass = d.expire(at).await.expect("expire");
        assert_eq!(
            pass,
            DisabledPass {
                expired: 1,
                deferred: 1,
                ..DisabledPass::default()
            }
        );
        assert_eq!(
            state(&env, scope).await,
            Ok(AccountState::Disabled {
                until: at + days(1)
            }),
            "отсрочка по конфигурации"
        );
        assert_eq!(refs(&env, scope, ids[0]).await, before, "письма целы");
        let hidden = env.trash.list_hidden(scope, 0, 10).await.expect("hidden");
        assert_eq!(hidden.len(), 1);
        let calls = hold.calls.lock().expect("lock").clone();
        let inbox = inbox(&env, scope).await;
        assert_eq!(
            calls
                .iter()
                .map(|c| (c.message, c.mailbox))
                .collect::<Vec<_>>(),
            [(ids[0], inbox), (ids[1], inbox), (ids[2], inbox)],
            "папка письма корзины — исходная"
        );

        // До конца отсрочки ящик не запрашивается.
        assert_eq!(d.expire(at).await, Ok(DisabledPass::default()));
        assert_eq!(hold.calls.lock().expect("lock").len(), 3);

        *hold.held.lock().expect("lock") = None;
        let pass = d.expire(at + days(1)).await.expect("expire");
        assert_eq!((pass.expired, pass.removed), (1, 1));
        assert_eq!(state(&env, scope).await, Err(Error::NotFound));
    }

    /// Приёмка MS-17: отключённый ящик подключается к другой учётной записи и больше не удаляется по сроку.
    #[tokio::test]
    async fn attach_to_other_account() {
        let env = Env::new();
        let now = OffsetDateTime::now_utc();
        let (scope, ids) = disabled_mailbox(&env, now).await;
        let target = env.meta.account("new@example.org").await.expect("acc");
        let own = deliver(&env, target, now).await;
        let d = disabled(&env);
        let mut system = env.bus.subscribe_system().await.expect("sub");

        // Ящик учётной записи не пуст: без режима — отказ, ничего не меняется.
        let refused = d
            .attach(scope.tenant, scope.account, target.account, None)
            .await;
        assert!(matches!(refused, Err(Error::Invalid(_))), "{refused:?}");

        let account = d
            .attach(
                scope.tenant,
                scope.account,
                target.account,
                Some(ExistingContent::Delete),
            )
            .await
            .expect("attach");
        assert_eq!(
            (account.id, account.login.as_str()),
            (scope.account, "new@example.org")
        );
        assert_eq!(state(&env, scope).await, Ok(AccountState::Active));
        assert_eq!(state(&env, target).await, Err(Error::NotFound));
        // Письма ящика на месте, письмо учётной записи — в корзине подключённого ящика.
        let kept = MessageStore::get(&env.meta, scope, ids[0])
            .await
            .expect("get")
            .expect("msg");
        assert_eq!(kept.mailbox, inbox(&env, scope).await);
        let trashed: Vec<TrashObject> = env
            .trash
            .list(scope, 0, 10)
            .await
            .expect("trash")
            .into_iter()
            .map(|i| i.object)
            .collect();
        assert!(trashed.contains(&TrashObject::Message(own)));
        let ev = tokio::time::timeout(Duration::from_secs(5), system.next())
            .await
            .expect("signal")
            .expect("event");
        assert_eq!(
            ev.kind,
            EventKind::TrashChanged {
                account: scope.account
            }
        );

        // Подключённый ящик действует: срок хранения его не касается.
        assert_eq!(d.expire(now + days(31)).await, Ok(DisabledPass::default()));
        assert!(
            MessageStore::get(&env.meta, scope, ids[0])
                .await
                .expect("get")
                .is_some()
        );
    }

    /// Воркер на часах testkit: таймер взведён на срок хранения, восстановление снимает его, удаление — в срок.
    #[tokio::test]
    async fn worker_arms_nearest_deadline() {
        let clock = tp_testkit::ManualClock::default();
        let t0 = tp_testkit::Clock::now(&clock);
        let mut env = Env::with_clock(clock.shared());
        env.store.cfg.disabled.max_wait = Duration::from_secs(40 * 86_400);
        let (first, _) = disabled_mailbox(&env, t0).await;
        let second = env.meta.account("later@example.org").await.expect("acc");
        deliver(&env, second, t0).await;
        env.meta
            .disable_account(second.tenant, second.account, t0 + days(35))
            .await
            .expect("disable");
        let d = disabled(&env);

        let armed = Arc::new(Mutex::new(Vec::new()));
        let timer = TestTimer {
            clock: clock.clone(),
            armed: armed.clone(),
        };
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let worker = {
            let d = d.clone();
            tokio::spawn(async move {
                d.worker(&timer, async {
                    let _ = stopped.await;
                })
                .await;
            })
        };
        let last = || armed.lock().expect("lock").last().copied();
        until(
            "таймер на ближайший срок",
            async || last() == Some(t0 + days(30)),
        )
        .await;

        // Восстановление ближайшего ящика: таймер перевзведён на следующий срок.
        d.restore(first.tenant, first.account)
            .await
            .expect("restore");
        until("таймер перевзведён", async || {
            last() == Some(t0 + days(35))
        })
        .await;
        assert_eq!(state(&env, first).await, Ok(AccountState::Active));

        clock.advance(Duration::from_secs(35 * 86_400));
        until("ящик удалён в срок", async || {
            state(&env, second).await == Err(Error::NotFound)
        })
        .await;
        assert_eq!(state(&env, first).await, Ok(AccountState::Active));
        // Отключённых ящиков нет: только сверка не реже `max_wait`.
        until("таймер сверки", async || {
            last() == Some(t0 + days(35) + days(40))
        })
        .await;

        stop.send(()).expect("stop");
        tokio::time::timeout(Duration::from_secs(5), worker)
            .await
            .expect("worker stopped")
            .expect("join");
    }
}
