//! TLS сессии (SRS 6.2, RFC 3207, RFC 8314): STARTTLS на 25 и 587, неявный TLS на 465.
//!
//! Рукопожатие и сертификаты — `tp-platform` (rustls, TLS 1.3 по умолчанию, перечитывание без перезапуска):
//! неявный TLS выполняет [`tp_platform::listen::Incoming::establish`], STARTTLS — [`Conn::starttls`]
//! после ответа `220`. Сессия после рукопожатия забывает состояние открытого канала и ждёт нового `EHLO`.

use std::sync::Arc;

use tokio::io::AsyncWriteExt;
use tp_contracts::transport::TrustedSource;
use tp_platform::Conn;

use super::{Exit, Session, SessionCtx};
use crate::screen::{Probe, Violation};
use crate::sources::Sources;

/// Ведёт сессию SMTP соединения `conn` до завершения, выполняя STARTTLS по запросу клиента.
pub async fn serve_conn(ctx: SessionCtx, conn: Conn) {
    serve_screened(ctx, conn, None, None, None).await;
}

/// [`serve_conn`] с проверками экрана `probe` и доверенным источником `source` (после STARTTLS источник
/// определяется заново по сертификату клиента — `sources`); итог проверок — [`Session::screen_outcome`].
pub(crate) async fn serve_screened(
    ctx: SessionCtx,
    mut conn: Conn,
    probe: Option<Probe>,
    source: Option<Arc<TrustedSource>>,
    sources: Option<&Sources>,
) -> Option<Result<(), Violation>> {
    let mut session = Session::new(ctx, conn.info, conn.can_starttls());
    session.set_source(source);
    if let Some(p) = probe {
        session.screening(p);
    }
    let mut greet = true;
    loop {
        match session.serve(&mut conn.stream, greet).await {
            Ok(Exit::StartTls) => match conn.starttls().await {
                Ok(tls) => {
                    conn = tls;
                    session.tls_established();
                    if let Some(s) = sources {
                        session.set_source(s.of(&conn).await);
                    }
                    greet = false;
                }
                Err(e) => {
                    tracing::debug!(client = %session.conn.client, error = %e, "smtp: STARTTLS не удался");
                    return session.screen_outcome();
                }
            },
            Ok(Exit::Quit | Exit::Closed) => break,
            Err(e) => {
                tracing::debug!(client = %session.conn.client, error = %e, "smtp: соединение прервано");
                return session.screen_outcome();
            }
        }
    }
    let _ = conn.stream.shutdown().await;
    session.screen_outcome()
}
