//! Заблокированная учётная запись (JMAP-41): сигнал `AccountDisabled` (CTR-64) закрывает соединения push
//! учётной записи — EventSource завершается, WebSocket получает Close 1008. Обычные запросы API и новые
//! соединения push отклоняет вход (`Authenticator`).
//!
//! Соединение push после входа регистрируется в [`Sessions`] сервера ([`Jmap::push_session`]); первая
//! регистрация запускает слушатель системных сигналов шины до остановки узла ([`Jmap::shutdown`]). Шина
//! at-most-once: после подписки и переподписки и по `Resync` открытые соединения сверяются с хранилищем учётных
//! записей (`Deps::accounts`) — нет записи или вход запрещён, соединения закрываются.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::watch;
use tp_contracts::bus::{EventBus, EventKind};
use tp_contracts::meta::{AccountScope, AccountStore};

use crate::core::Jmap;

/// Пауза перед переподпиской на системные сигналы после обрыва потока или ошибки подписки.
const RESUBSCRIBE: Duration = Duration::from_secs(1);

/// Открытые соединения push сервера по учётным записям.
#[derive(Clone, Default)]
pub(crate) struct Sessions(Arc<Inner>);

#[derive(Default)]
struct Inner {
    open: Mutex<Open>,
    /// Слушатель сигналов запущен.
    watching: AtomicBool,
}

#[derive(Default)]
struct Open {
    next: u64,
    by_account: HashMap<AccountScope, HashMap<u64, watch::Sender<bool>>>,
}

/// Регистрация соединения push после входа; снимается при закрытии соединения.
pub(crate) struct Registration {
    sessions: Sessions,
    scope: AccountScope,
    id: u64,
    disabled: watch::Receiver<bool>,
}

impl Registration {
    /// Вход учётной записи уже запрещён.
    pub(crate) fn is_disabled(&self) -> bool {
        *self.disabled.borrow()
    }

    /// Завершается, когда вход учётной записи запрещён.
    pub(crate) async fn disabled(&mut self) {
        // Отправитель живёт в реестре, пока жива регистрация; закрытый канал — не запрет.
        let closed = self.disabled.wait_for(|d| *d).await.is_err();
        if closed {
            std::future::pending::<()>().await;
        }
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        let mut open = self.sessions.open();
        if let Some(ids) = open.by_account.get_mut(&self.scope) {
            ids.remove(&self.id);
            if ids.is_empty() {
                open.by_account.remove(&self.scope);
            }
        }
    }
}

impl Sessions {
    fn open(&self) -> MutexGuard<'_, Open> {
        self.0.open.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Регистрирует соединение учётной записи `scope`.
    fn register(&self, scope: AccountScope) -> Registration {
        let (tx, disabled) = watch::channel(false);
        let mut open = self.open();
        open.next += 1;
        let id = open.next;
        open.by_account.entry(scope).or_default().insert(id, tx);
        drop(open);
        Registration {
            sessions: self.clone(),
            scope,
            id,
            disabled,
        }
    }

    /// Закрывает соединения учётной записи `scope`.
    fn disable(&self, scope: AccountScope) {
        let open = self.open();
        if let Some(ids) = open.by_account.get(&scope) {
            tracing::info!(account = %scope.account, sessions = ids.len(), "jmap: вход запрещён, соединения push закрываются");
            for tx in ids.values() {
                tx.send_replace(true);
            }
        }
    }

    /// Сверка открытых соединений с хранилищем: нет записи или вход запрещён — соединения закрываются.
    async fn reconcile(&self, accounts: &dyn AccountStore) {
        let scopes: Vec<AccountScope> = self.open().by_account.keys().copied().collect();
        for scope in scopes {
            match accounts.get(scope.tenant, scope.account).await {
                Ok(Some(account)) if account.enabled => {}
                Ok(_) => self.disable(scope),
                Err(e) => tracing::debug!(error = %e, "jmap: учётная запись не проверена"),
            }
        }
    }

    /// Запускает слушатель системных сигналов `bus` (один на сервер) до остановки узла `stop`.
    fn start(
        &self,
        bus: Arc<dyn EventBus>,
        accounts: Arc<dyn AccountStore>,
        mut stop: watch::Receiver<bool>,
    ) {
        if self.0.watching.swap(true, Ordering::SeqCst) {
            return;
        }
        let sessions = self.clone();
        tokio::spawn(async move {
            tokio::select! {
                () = async { let _ = stop.wait_for(|s| *s).await; } => {}
                () = sessions.watch(bus, accounts) => {}
            }
        });
    }

    async fn watch(&self, bus: Arc<dyn EventBus>, accounts: Arc<dyn AccountStore>) {
        loop {
            match bus.subscribe_system().await {
                Ok(mut events) => {
                    // Сигналы до подписки могли потеряться.
                    self.reconcile(accounts.as_ref()).await;
                    while let Some(ev) = events.next().await {
                        match ev.kind {
                            EventKind::AccountDisabled { account } => self.disable(AccountScope {
                                tenant: ev.tenant,
                                account,
                            }),
                            EventKind::Resync => self.reconcile(accounts.as_ref()).await,
                            _ => {}
                        }
                    }
                    tracing::debug!("jmap: поток системных сигналов оборвался, переподписка");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "jmap: подписка на системные сигналы не удалась");
                }
            }
            tokio::time::sleep(RESUBSCRIBE).await;
        }
    }
}

impl Jmap {
    /// Регистрирует соединение push учётной записи `scope`; при первой регистрации запускает слушатель сигналов.
    pub(crate) fn push_session(&self, scope: AccountScope) -> Registration {
        self.push
            .start(self.d.bus.clone(), self.d.accounts.clone(), self.stopping());
        self.push.register(scope)
    }
}
