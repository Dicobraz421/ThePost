//! Заблокированная учётная запись (IMAP-22): сигнал `AccountDisabled` (CTR-64) закрывает открытые сеансы
//! учётной записи — IMAP (в том числе в `IDLE`) получает `* BYE`, POP3 — `-ERR [SYS/PERM]`; соединение
//! закрывается. Новый вход отклоняет `Authenticator`.
//!
//! Сессия после входа регистрируется в [`Sessions`] своего сервера ([`Server`](crate::Server),
//! [`Pop3Server`](crate::Pop3Server)); сервер при первом `serve` запускает слушатель системных сигналов шины
//! ([`Sessions::start`]) и отменяет признак сессий учётной записи. Шина at-most-once: после переподписки и по
//! `Resync` открытые сессии сверяются с хранилищем учётных записей, если оно подключено (IMAP —
//! `Extensions::accounts`, POP3 — `Pop3Server::with_accounts`): нет записи или вход запрещён — сессия
//! закрывается. Без хранилища потерянный сигнал сессию не закрывает — вход отклонится при следующем соединении.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use futures::StreamExt;
use tokio_util::sync::CancellationToken;
use tp_contracts::bus::{EventBus, EventKind};
use tp_contracts::meta::{AccountScope, AccountStore};

/// Открытые сессии сервера по учётным записям.
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
    by_account: HashMap<AccountScope, HashMap<u64, CancellationToken>>,
}

/// Регистрация сессии после входа; снимается при завершении сессии.
pub(crate) struct Registration {
    sessions: Sessions,
    scope: AccountScope,
    id: u64,
    token: CancellationToken,
}

impl Registration {
    /// Отменяется, когда вход учётной записи запрещён.
    pub(crate) fn token(&self) -> &CancellationToken {
        &self.token
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

    /// Регистрирует сессию учётной записи `scope`.
    pub(crate) fn register(&self, scope: AccountScope) -> Registration {
        let token = CancellationToken::new();
        let mut open = self.open();
        open.next += 1;
        let id = open.next;
        open.by_account
            .entry(scope)
            .or_default()
            .insert(id, token.clone());
        drop(open);
        Registration {
            sessions: self.clone(),
            scope,
            id,
            token,
        }
    }

    /// Закрывает сессии учётной записи `scope`.
    pub(crate) fn disable(&self, scope: AccountScope) {
        let open = self.open();
        if let Some(ids) = open.by_account.get(&scope) {
            tracing::info!(account = %scope.account, sessions = ids.len(), "imap: вход запрещён, сессии закрываются");
            for token in ids.values() {
                token.cancel();
            }
        }
    }

    /// Сверка открытых сессий с хранилищем: нет записи или вход запрещён — сессии закрываются.
    async fn reconcile(&self, accounts: &dyn AccountStore) {
        let scopes: Vec<AccountScope> = self.open().by_account.keys().copied().collect();
        for scope in scopes {
            match accounts.get(scope.tenant, scope.account).await {
                Ok(Some(account)) if account.enabled => {}
                Ok(_) => self.disable(scope),
                Err(e) => tracing::debug!(error = %e, "imap: учётная запись не проверена"),
            }
        }
    }

    /// Запускает слушатель системных сигналов `bus` (один на сервер) до остановки узла `shutdown`;
    /// `accounts` — сверка по `Resync`, `resubscribe` — пауза перед переподпиской.
    pub(crate) fn start(
        &self,
        bus: Arc<dyn EventBus>,
        accounts: Option<Arc<dyn AccountStore>>,
        resubscribe: Duration,
        shutdown: CancellationToken,
    ) {
        if self.0.watching.swap(true, Ordering::SeqCst) {
            return;
        }
        let sessions = self.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = shutdown.cancelled() => {}
                () = sessions.watch(bus, accounts, resubscribe) => {}
            }
        });
    }

    async fn watch(
        &self,
        bus: Arc<dyn EventBus>,
        accounts: Option<Arc<dyn AccountStore>>,
        resubscribe: Duration,
    ) {
        loop {
            match bus.subscribe_system().await {
                Ok(mut events) => {
                    // Сигналы до подписки могли потеряться.
                    if let Some(accounts) = accounts.as_deref() {
                        self.reconcile(accounts).await;
                    }
                    while let Some(ev) = events.next().await {
                        match ev.kind {
                            EventKind::AccountDisabled { account } => self.disable(AccountScope {
                                tenant: ev.tenant,
                                account,
                            }),
                            EventKind::Resync => {
                                if let Some(accounts) = accounts.as_deref() {
                                    self.reconcile(accounts).await;
                                }
                            }
                            _ => {}
                        }
                    }
                    tracing::debug!("imap: поток системных сигналов оборвался, переподписка");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "imap: подписка на системные сигналы не удалась")
                }
            }
            tokio::time::sleep(resubscribe).await;
        }
    }
}
