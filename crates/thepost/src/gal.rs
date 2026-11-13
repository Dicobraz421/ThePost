//! Книга GAL «Организация» (APP-49, решение 72): роль maintenance строит её из каталога организации и записывает в
//! хранилище DAV ([`tp_dav::gal::sync`] → `DavStore::sync_gal`); CardDAV и JMAP Contacts читают её оттуда. Каталог
//! один, по конфигурации (APP-51): с секцией `auth.ldap` — LDAP, без неё — учётные записи и группы PostgreSQL,
//! заведённые в консоли или через Admin API (`impl Directory for PgMeta`, META-65).
//!
//! Первый проход — при запуске роли, затем раз в `maintenance.gal_interval`. Сигнала об изменении каталога нет:
//! LDAP опрашивается, своей синхронизации каталога в узле нет, Admin API об изменении учётных записей и групп не
//! сообщает, вида «каталог изменился» у шины нет. Поэтому новая,
//! изменённая, удалённая или скрытая из GAL учётная запись (группа) видна в книге не позже чем через период.
//! Неизменный каталог книгу не меняет: `sync_gal` с прежним набором ничего не пишет и `GalChanged` не публикует.
//!
//! В кластере книгу организации пишет один узел — держатель лизы [`GalSync::lease_name`]; остальные узлы с ролью
//! maintenance ждут её истечения. Ошибка каталога или хранилища — в журнал, книга прежняя, повтор — через период.
//!
//! JMAP push подписан только на сигналы учётной записи, а `GalChanged` — системный. [`GalPush`] — шина для JMAP
//! узла: системный `GalChanged` организации приходит в подписку каждой её учётной записи как `DavChanged` книги
//! «Организация» (она — среди адресных книг каждой учётной записи), и клиент получает `StateChange` контактов.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::{StreamExt, stream};
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;
use tp_contracts::bus::{Event, EventBus, EventKind, EventStream};
use tp_contracts::dav::{CollectionKind, DavStore, dav_changed};
use tp_contracts::lease::{Lease, LeaseStore};
use tp_contracts::meta::AccountScope;
use tp_contracts::role::CancellationToken;
use tp_contracts::{Result, TenantId};
use tp_dav::{DavConfig, GalDeps};

/// Наименьший срок лизы: короткий период не должен отдавать книгу другому узлу на каждой паузе сети.
const MIN_LEASE_TTL: Duration = Duration::from_secs(30);

/// Сигналов `GalChanged` в очереди подписки [`GalPush`]; отставшая подписка получает `Resync`.
const PUSH_BUFFER: usize = 64;

/// Заполнение книги GAL организации роли maintenance.
pub struct GalSync {
    source: GalDeps,
    dav: Arc<dyn DavStore>,
    tenant: TenantId,
    cfg: DavConfig,
    leases: Arc<dyn LeaseStore>,
    holder: String,
    interval: Duration,
}

impl std::fmt::Debug for GalSync {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GalSync")
            .field("tenant", &self.tenant)
            .field("holder", &self.holder)
            .field("interval", &self.interval)
            .finish_non_exhaustive()
    }
}

impl GalSync {
    /// Книга организации `tenant` из каталога и групп `source` в `dav` (имя и размер порции — секция `dav`), раз в
    /// `interval`, под лизой держателя `holder`.
    pub fn new(
        source: GalDeps,
        dav: Arc<dyn DavStore>,
        tenant: TenantId,
        cfg: DavConfig,
        leases: Arc<dyn LeaseStore>,
        holder: String,
        interval: Duration,
    ) -> Self {
        Self {
            source,
            dav,
            tenant,
            cfg,
            leases,
            holder,
            interval,
        }
    }

    /// Имя лизы: одна запись книги на организацию (`gal/<организация>`).
    pub fn lease_name(&self) -> String {
        format!("gal/{}", self.tenant)
    }

    /// Срок лизы — три периода (не меньше [`MIN_LEASE_TTL`]): держатель продлевает её каждым проходом.
    fn lease_ttl(&self) -> Duration {
        self.interval.saturating_mul(3).max(MIN_LEASE_TTL)
    }

    /// Проходы до остановки: первый сразу, затем раз в период; после остановки лиза отдаётся.
    pub async fn run(&self, shutdown: CancellationToken) {
        let name = self.lease_name();
        let mut lease: Option<Lease> = None;
        let mut tick = tokio::time::interval(self.interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = shutdown.cancelled() => break,
                _ = tick.tick() => {}
            }
            if !self.lead(&name, &mut lease).await {
                continue;
            }
            tokio::select! {
                () = shutdown.cancelled() => break,
                done = tp_dav::gal::sync(&self.source, self.dav.as_ref(), self.tenant, &self.cfg) => {
                    if let Err(e) = done {
                        tracing::warn!(error = %e, "книга GAL не обновлена: повтор через maintenance.gal_interval");
                    }
                }
            }
        }
        if let Some(held) = lease
            && let Err(e) = self.leases.release(&held).await
        {
            tracing::warn!(lease = %name, error = %e, "книга GAL: лиза не отдана");
        }
    }

    /// Держит лизу книги: продлевает её, без лизы — пытается получить. `false` — книгу пишет другой узел или лиза
    /// недоступна.
    async fn lead(&self, name: &str, lease: &mut Option<Lease>) -> bool {
        let ttl = self.lease_ttl();
        if let Some(held) = lease.take() {
            match self.leases.renew(&held, ttl).await {
                Ok(Some(next)) => {
                    *lease = Some(next);
                    return true;
                }
                Ok(None) => tracing::warn!(lease = %name, "книга GAL: лиза потеряна"),
                Err(e) => {
                    tracing::warn!(lease = %name, error = %e, "книга GAL: лиза не продлена");
                    *lease = Some(held);
                    return false;
                }
            }
        }
        match self.leases.try_acquire(name, &self.holder, ttl).await {
            Ok(Some(next)) => {
                tracing::info!(lease = %name, "книга GAL: заполняет этот узел");
                *lease = Some(next);
                true
            }
            Ok(None) => false,
            Err(e) => {
                tracing::warn!(lease = %name, error = %e, "книга GAL: лиза не получена");
                false
            }
        }
    }
}

/// Шина JMAP узла (APP-49): сигналы учётной записи и публикация — из `inner`, плюс системный `GalChanged` её
/// организации в виде `DavChanged` книги «Организация» этой учётной записи. Системные сигналы узел читает одной
/// подпиской ([`GalPush::run`]) и раздаёт подпискам учётных записей в памяти; отставшая подписка получает `Resync`.
/// Поток подписки завершается вместе с потоком учётной записи `inner` (потеря связи с шиной).
pub struct GalPush {
    inner: Arc<dyn EventBus>,
    tx: broadcast::Sender<Event>,
}

impl std::fmt::Debug for GalPush {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GalPush")
            .field("subscribers", &self.tx.receiver_count())
            .finish_non_exhaustive()
    }
}

impl GalPush {
    /// Шина поверх `inner`.
    pub fn new(inner: Arc<dyn EventBus>) -> Self {
        let (tx, _) = broadcast::channel(PUSH_BUFFER);
        Self { inner, tx }
    }

    /// Системные сигналы `GalChanged` до остановки; поток прервался или подписка не удалась — новая через `retry`.
    pub async fn run(self: Arc<Self>, retry: Duration, shutdown: CancellationToken) {
        while !shutdown.is_cancelled() {
            match self.inner.subscribe_system().await {
                Ok(mut events) => loop {
                    let ev = tokio::select! {
                        () = shutdown.cancelled() => return,
                        ev = events.next() => ev,
                    };
                    match ev {
                        Some(ev) if matches!(ev.kind, EventKind::GalChanged { .. }) => {
                            // Подписчиков нет — сигнал некому передать.
                            let _ = self.tx.send(ev);
                        }
                        Some(_) => {}
                        None => break,
                    }
                },
                Err(e) => {
                    tracing::warn!(error = %e, "книга GAL: подписка на системные сигналы не удалась")
                }
            }
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = tokio::time::sleep(retry) => {}
            }
        }
    }
}

/// `GalChanged` организации `scope` — изменение книги «Организация» учётной записи `scope`.
fn for_account(ev: Event, scope: AccountScope) -> Option<Event> {
    match ev.kind {
        EventKind::GalChanged { collection, modseq } if ev.tenant == scope.tenant => Some(
            dav_changed(scope, CollectionKind::AddressBook, collection, modseq),
        ),
        _ => None,
    }
}

#[async_trait]
impl EventBus for GalPush {
    async fn publish(&self, event: Event) -> Result<()> {
        self.inner.publish(event).await
    }

    async fn subscribe(&self, scope: AccountScope) -> Result<EventStream> {
        let gal = self.tx.subscribe();
        let own = self.inner.subscribe(scope).await?;
        let gal = stream::unfold(gal, move |mut rx| async move {
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        if let Some(ev) = for_account(ev, scope) {
                            return Some((ev, rx));
                        }
                    }
                    Err(RecvError::Lagged(_)) => {
                        let resync = Event {
                            tenant: scope.tenant,
                            account: Some(scope.account),
                            kind: EventKind::Resync,
                        };
                        return Some((resync, rx));
                    }
                    Err(RecvError::Closed) => return None,
                }
            }
        });
        // Конец потока учётной записи (`None`) завершает и общий поток.
        let own = own.map(Some).chain(stream::once(async { None }));
        Ok(stream::select(own, gal.map(Some))
            .take_while(|ev| std::future::ready(ev.is_some()))
            .filter_map(std::future::ready)
            .boxed())
    }

    async fn subscribe_system(&self) -> Result<EventStream> {
        self.inner.subscribe_system().await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use tp_contracts::auth::{DirAccount, DirOrg};
    use tp_contracts::dav::{CollectionId, gal_changed};
    use tp_contracts::{AccountId, ModSeq, TenantId};
    use tp_testkit::{FakeDirectory, MemAddressBook, MemDav, MemGroups, MemLease};

    use super::*;

    fn account(id: &str, name: &str, hidden: bool) -> DirAccount {
        DirAccount {
            external_id: id.to_owned(),
            login: format!("{id}@example.org"),
            display_name: name.to_owned(),
            emails: vec![format!("{id}@example.org")],
            enabled: true,
            attributes: BTreeMap::new(),
            org: DirOrg::default(),
            hidden_from_gal: hidden,
        }
    }

    /// Имена карточек книги GAL, как их видит учётная запись организации; книги нет — `None`.
    async fn book(dav: &MemDav, tenant: TenantId) -> Option<Vec<String>> {
        let scope = AccountScope {
            tenant,
            account: AccountId::new(),
        };
        let gal = dav
            .list_collections(scope, CollectionKind::AddressBook)
            .await
            .expect("книги")
            .into_iter()
            .find(|c| c.is_gal())?;
        let mut names: Vec<String> = dav
            .list_objects(scope, gal.id)
            .await
            .expect("карточки")
            .into_iter()
            .map(|o| o.name)
            .collect();
        names.sort();
        Some(names)
    }

    /// Ждёт, пока книга не станет `want`.
    async fn wait_book(dav: &MemDav, tenant: TenantId, want: &[&str]) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let got = book(dav, tenant).await;
            if got.as_deref().is_some_and(|g| g == want) {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "книга GAL {got:?}, ожидалась {want:?}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// APP-49: после запуска книга содержит учётные записи организации без скрытой из GAL; изменение каталога
    /// (новая учётная запись, скрытие) видно в книге через период; пока лизу держит другой узел, книга не пишется.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fills_book_from_directory() {
        let tenant = TenantId::new();
        let dir = FakeDirectory::default();
        dir.upsert_account(tenant, account("u-ivan", "Иван Петров", false));
        dir.upsert_account(tenant, account("u-maria", "Мария Иванова", true));
        let dav = MemDav::new();
        let leases = MemLease::default();
        let sync = Arc::new(GalSync::new(
            GalDeps {
                directory: Arc::new(dir.clone()),
                groups: Some(Arc::new(MemGroups::new())),
                contacts: Some(Arc::new(MemAddressBook::new())),
            },
            Arc::new(dav.clone()),
            tenant,
            DavConfig::default(),
            Arc::new(leases.clone()),
            "node-a/1".into(),
            Duration::from_millis(20),
        ));

        // Лиза у другого узла — книгу этот узел не пишет.
        let other = leases
            .try_acquire(&sync.lease_name(), "node-b/1", Duration::from_secs(60))
            .await
            .expect("лиза")
            .expect("лиза свободна");
        let shutdown = CancellationToken::new();
        let task = tokio::spawn({
            let (sync, shutdown) = (sync.clone(), shutdown.clone());
            async move { sync.run(shutdown).await }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(book(&dav, tenant).await, None, "книгу пишет держатель лизы");

        leases.release(&other).await.expect("лиза");
        wait_book(&dav, tenant, &["u-ivan.vcf"]).await;

        dir.upsert_account(tenant, account("u-oleg", "Олег Смирнов", false));
        wait_book(&dav, tenant, &["u-ivan.vcf", "u-oleg.vcf"]).await;

        dir.upsert_account(tenant, account("u-ivan", "Иван Петров", true));
        dir.upsert_account(tenant, account("u-maria", "Мария Иванова", false));
        wait_book(&dav, tenant, &["u-maria.vcf", "u-oleg.vcf"]).await;

        shutdown.cancel();
        task.await.expect("join");
        assert!(
            leases
                .try_acquire(&sync.lease_name(), "node-b/1", Duration::from_secs(60))
                .await
                .expect("лиза")
                .is_some(),
            "после остановки лиза отдана"
        );
    }

    /// APP-49: системный `GalChanged` организации приходит в подписку её учётной записи как `DavChanged` адресной
    /// книги (JMAP push — `StateChange` контактов), учётной записи другой организации — нет; сигналы учётной записи
    /// проходят как были.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn gal_changed_reaches_account_push() {
        let inner: Arc<dyn EventBus> = Arc::new(tp_cluster::LocalBus::new(
            &tp_cluster::LocalBusConfig::default(),
        ));
        let push = Arc::new(GalPush::new(inner.clone()));
        let shutdown = CancellationToken::new();
        tokio::spawn(
            push.clone()
                .run(Duration::from_millis(10), shutdown.clone()),
        );
        let tenant = TenantId::new();
        let scope = AccountScope {
            tenant,
            account: AccountId::new(),
        };
        let stranger = AccountScope {
            tenant: TenantId::new(),
            account: AccountId::new(),
        };
        let mut mine = push.subscribe(scope).await.expect("подписка");
        let mut other = push.subscribe(stranger).await.expect("подписка");
        let collection = CollectionId::new();

        // Системная подписка `run` могла ещё не начаться: сигнал повторяется до получения.
        let got = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                inner
                    .publish(gal_changed(tenant, collection, ModSeq(7)))
                    .await
                    .expect("publish");
                if let Ok(Some(ev)) =
                    tokio::time::timeout(Duration::from_millis(50), mine.next()).await
                {
                    return ev;
                }
            }
        })
        .await
        .expect("GalChanged не пришёл");
        assert_eq!(
            got,
            dav_changed(scope, CollectionKind::AddressBook, collection, ModSeq(7))
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), other.next())
                .await
                .is_err(),
            "сигнал чужой организации"
        );

        let own = Event {
            tenant,
            account: Some(scope.account),
            kind: EventKind::MailboxListChanged,
        };
        inner.publish(own.clone()).await.expect("publish");
        let ev = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let ev = mine.next().await.expect("поток");
                if ev.kind != got.kind {
                    return ev;
                }
            }
        })
        .await
        .expect("сигнал учётной записи");
        assert_eq!(ev, own);
        shutdown.cancel();
    }
}
