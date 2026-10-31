//! TLS сессии (SRS 6.2, RFC 3207, RFC 8314): STARTTLS на 25 и 587, неявный TLS на 465.
//!
//! Рукопожатие и сертификаты — `tp-platform` (rustls, TLS 1.3 по умолчанию, перечитывание без перезапуска):
//! неявный TLS выполняет [`tp_platform::listen::Incoming::establish`], STARTTLS — [`Conn::starttls`]
//! после ответа `220`. Сессия после рукопожатия забывает состояние открытого канала и ждёт нового `EHLO`.

use tokio::io::AsyncWriteExt;
use tp_platform::Conn;

use super::{Exit, Session, SessionCtx};

/// Ведёт сессию SMTP соединения `conn` до завершения, выполняя STARTTLS по запросу клиента.
pub async fn serve_conn(ctx: SessionCtx, mut conn: Conn) {
    let mut session = Session::new(ctx, conn.info, conn.can_starttls());
    let mut greet = true;
    loop {
        match session.serve(&mut conn.stream, greet).await {
            Ok(Exit::StartTls) => match conn.starttls().await {
                Ok(tls) => {
                    conn = tls;
                    session.tls_established();
                    greet = false;
                }
                Err(e) => {
                    tracing::debug!(client = %session.conn.client, error = %e, "smtp: STARTTLS не удался");
                    return;
                }
            },
            Ok(Exit::Quit | Exit::Closed) => break,
            Err(e) => {
                tracing::debug!(client = %session.conn.client, error = %e, "smtp: соединение прервано");
                return;
            }
        }
    }
    let _ = conn.stream.shutdown().await;
}
