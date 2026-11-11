//! Локальный сокет узла и команда `thepost admin init` (APP-19, решение владельца #193; обработчик — `tp_admin::init`,
//! ADM-11).
//!
//! - [`bind`] — `serve` открывает сокет `admin.init.socket` до наложения Landlock (`startup::prepare_serve`): старый
//!   файл сокета удаляется, права — `0600` учётной записи службы. Только если на узле есть роль admin.
//! - [`attach`] — Admin API получает настройки транспорта (`PgMeta`: принятые домены для `admin init` и
//!   учётных записей) и, когда будет реализация, хранилище локальных токенов.
//! - [`serve`] — роль admin начинает приём на открытом сокете: соединения принимаются от root и от учётной
//!   записи службы (владельца сокета), остальные — отказ (`SO_PEERCRED`, `tp_admin::init::serve_local`).
//! - [`command`] — `thepost admin init [--email] [--name] [--force]`: запрос узлу по сокету, пароль и токен —
//!   один раз в stdout.

use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use tp_admin::init::{InitReply, InitRequest};
use tp_contracts::role::Role;
use tp_contracts::{Error, Result};
use tp_platform::Config;

use crate::sections::{self, ADMIN, AdminSection};
use crate::services::Services;

/// Сокет, открытый до Landlock: слушатель и `uid` владельца (учётная запись службы).
struct Bound {
    listener: UnixListener,
    owner: u32,
    path: PathBuf,
}

/// Открытый сокет ждёт роль admin (одна на процесс).
static PENDING: Mutex<Option<Bound>> = Mutex::new(None);

fn pending() -> std::sync::MutexGuard<'static, Option<Bound>> {
    PENDING.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Открывает локальный сокет `admin.init.socket` (до Landlock); без роли admin на узле — ничего.
pub fn bind(cfg: &Config) -> Result<()> {
    if !cfg.roles().contains(Role::Admin) {
        return Ok(());
    }
    let section = cfg.section::<AdminSection>(ADMIN)?;
    let path = section.0.init.socket.clone();
    let fail = |what: &str, e: std::io::Error| {
        Error::Internal(format!("admin.init.socket {}: {what}: {e}", path.display()))
    };
    match std::fs::symlink_metadata(&path) {
        Ok(m) if m.file_type().is_socket() => {
            std::fs::remove_file(&path).map_err(|e| fail("удаление старого сокета", e))?;
        }
        Ok(_) => {
            return Err(Error::Internal(format!(
                "admin.init.socket {}: файл существует и это не сокет",
                path.display()
            )));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(fail("проверка", e)),
    }
    let listener = UnixListener::bind(&path).map_err(|e| fail("открытие", e))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| fail("права 0600", e))?;
    let owner = std::fs::metadata(&path)
        .map_err(|e| fail("владелец", e))?
        .uid();
    *pending() = Some(Bound {
        listener,
        owner,
        path,
    });
    Ok(())
}

/// Admin API с настройками транспорта и (когда будет реализация) хранилищем локальных токенов.
pub fn attach(admin: tp_admin::Admin, services: &Services) -> tp_admin::Admin {
    // TODO(META): `.with_local_tokens(Arc::new(services.meta.clone()))` — реализации `LocalAdminTokens` в PgMeta ещё
    // нет; до неё `admin init` отвечает «узел не готов» (503), in-memory на узле недопустим (токен — вход
    // администратора инсталляции).
    admin.with_transport(Arc::new(services.meta.clone()))
}

/// Начинает приём на сокете, открытом [`bind`]; не открыт (тесты, нет роли admin) — ничего.
pub fn serve(admin: Arc<tp_admin::Admin>) {
    let Some(bound) = pending().take() else {
        return;
    };
    let Bound {
        listener,
        owner,
        path,
    } = bound;
    let listener = match listener
        .set_nonblocking(true)
        .and_then(|()| tokio::net::UnixListener::from_std(listener))
    {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = %e, path = %path.display(), "admin init: локальный сокет не принят рантаймом");
            return;
        }
    };
    tracing::info!(path = %path.display(), "admin init: локальный сокет открыт");
    tokio::spawn(async move {
        if let Err(e) = tp_admin::init::serve_local(admin, listener, Some(owner)).await {
            tracing::error!(error = %e, "admin init: приём на локальном сокете остановлен");
        }
    });
}

/// Параметры `thepost admin init`.
#[derive(Debug, Clone)]
pub struct InitArgs {
    /// Адрес администратора; нет — `admin@<domain>`.
    pub email: Option<String>,
    /// Отображаемое имя.
    pub name: Option<String>,
    /// Повтор при заведённом администраторе.
    pub force: bool,
    /// Путь сокета вместо `admin.init.socket`.
    pub socket: Option<PathBuf>,
}

/// `thepost admin init`: код возврата (0 — заведён, 1 — отказ узла или ошибка).
pub async fn command(config: &Path, args: InitArgs) -> i32 {
    match init(config, args).await {
        Ok(text) => {
            print!("{text}");
            0
        }
        Err(msg) => {
            eprintln!("thepost: admin init: {msg}");
            1
        }
    }
}

async fn init(config: &Path, args: InitArgs) -> std::result::Result<String, String> {
    let cfg = sections::registry()
        .and_then(|r| r.load(config))
        .map_err(|e| format!("{}: {e}", config.display()))?;
    let section = cfg
        .section::<AdminSection>(ADMIN)
        .map_err(|e| e.to_string())?;
    let init = &section.0.init;
    let socket = args.socket.unwrap_or_else(|| init.socket.clone());
    let email = args
        .email
        .unwrap_or_else(|| format!("admin@{}", cfg.domain()));
    let stream = tokio::net::UnixStream::connect(&socket)
        .await
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => format!(
                "сокет {} не открыт: узел не запущен или на нём нет роли admin ({e})",
                socket.display()
            ),
            std::io::ErrorKind::PermissionDenied => format!(
                "нет доступа к сокету {}: запустите от root или от учётной записи службы ({e})",
                socket.display()
            ),
            _ => format!("сокет {}: {e}", socket.display()),
        })?;
    let req = InitRequest {
        email,
        display_name: args.name,
        force: args.force,
    };
    let reply = tp_admin::init::request(stream, &req, init.max_request_bytes.max(64 * 1024))
        .await
        .map_err(|e| e.to_string())?;
    match reply {
        InitReply::Granted(g) => {
            // Срок — как в ответе узла (RFC 3339).
            let expires = serde_json::to_value(&g)
                .ok()
                .and_then(|v| v["expires_at"].as_str().map(str::to_owned))
                .unwrap_or_else(|| g.expires_at.to_string());
            Ok(format!(
                "Администратор заведён: {login} (организация {tenant}, учётная запись {account}).\n\
                 Пароль (IMAP, SMTP, JMAP; показывается один раз): {password}\n\
                 Токен Admin API (одноразовый, действует до {expires}): {token}\n\
                 Обмен на сессию Admin API: POST /api/v1/session с заголовком «Authorization: Bearer <токен>»;\n\
                 в ответе — значение сессии (tps_…) для следующих запросов.\n",
                login = g.login,
                tenant = g.tenant,
                account = g.account,
                password = g.password,
                token = g.token,
            ))
        }
        InitReply::Refused { code, reason } => Err(format!("отказ узла ({code}): {reason}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "
domain: example.test
profile: dev
service_http:
  allow_plaintext: true
database:
  url: postgres://thepost@localhost/thepost?sslmode=disable
  tls: { allow_plaintext: true }
blob:
  url: file:///var/lib/thepost/blob
";

    /// Сокет с правами 0600, старый файл сокета заменяется, чужой файл — ошибка; без роли admin — не открывается.
    #[test]
    fn bind_socket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("admin.sock");
        let load = |roles: &str| {
            let yaml = format!(
                "{BASE}roles: [{roles}]\nadmin:\n  init: {{ socket: \"{}\" }}\n",
                socket.display()
            );
            sections::registry()
                .and_then(|r| r.parse(&yaml, Vec::new()))
                .expect("config")
        };

        bind(&load("imap")).expect("без роли admin");
        assert!(!socket.exists());

        let cfg = load("admin");
        bind(&cfg).expect("bind");
        let mode = std::fs::metadata(&socket)
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        // Повторный запуск: старый сокет заменяется.
        bind(&cfg).expect("rebind");
        assert!(socket.exists());
        pending().take();

        std::fs::remove_file(&socket).expect("rm");
        std::fs::write(&socket, b"x").expect("file");
        assert!(bind(&cfg).is_err());
    }

    #[test]
    fn cli_parses_admin_init() {
        use clap::Parser;

        use crate::cli::{AdminAction, Cli, Command};

        let cli =
            Cli::try_parse_from(["thepost", "admin", "init", "--email", "a@b.test", "--force"])
                .expect("parse");
        let Command::Admin {
            action: AdminAction::Init { email, force, .. },
        } = cli.command
        else {
            panic!("{:?}", cli.command)
        };
        assert_eq!((email.as_deref(), force), (Some("a@b.test"), true));
        assert!(Cli::try_parse_from(["thepost", "admin", "init", "--mail", "x"]).is_err());
    }

    /// Клиент: запрос строкой JSON, ответ узла — текст с паролем и токеном или ошибка с причиной.
    #[tokio::test]
    async fn client_prints_reply() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("admin.sock");
        let config = dir.path().join("config.yaml");
        std::fs::write(&config, format!("{BASE}roles: [admin]\n")).expect("config");
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind");
        let server = tokio::spawn(async move {
            let mut seen = Vec::new();
            for reply in [
                serde_json::json!({
                    "status": "granted", "tenant": tp_contracts::TenantId::DEFAULT,
                    "account": tp_contracts::AccountId::new(), "login": "admin@example.test",
                    "password": "pw-once", "token": "tpa_once", "expires_at": "2026-11-11T12:00:00Z"
                }),
                serde_json::json!({"status": "refused", "code": 409, "reason": "уже заведён"}),
            ] {
                let (stream, _) = listener.accept().await.expect("accept");
                let (r, mut w) = tokio::io::split(stream);
                let mut line = String::new();
                BufReader::new(r).read_line(&mut line).await.expect("read");
                seen.push(serde_json::from_str::<InitRequest>(&line).expect("request"));
                let mut out = reply.to_string().into_bytes();
                out.push(b'\n');
                w.write_all(&out).await.expect("write");
                w.shutdown().await.expect("shutdown");
            }
            seen
        });
        let args = |force| InitArgs {
            email: None,
            name: Some("Админ".into()),
            force,
            socket: Some(socket.clone()),
        };
        let text = init(&config, args(false)).await.expect("granted");
        assert!(text.contains("admin@example.test"), "{text}");
        assert!(
            text.contains("pw-once") && text.contains("tpa_once"),
            "{text}"
        );
        assert!(text.contains("2026-11-11T12:00:00Z"), "{text}");
        let err = init(&config, args(true)).await.expect_err("refused");
        assert!(err.contains("409") && err.contains("уже заведён"), "{err}");
        let seen = server.await.expect("server");
        assert_eq!(
            seen[0].email, "admin@example.test",
            "по умолчанию — admin@<domain>"
        );
        assert_eq!(seen[0].display_name.as_deref(), Some("Админ"));
        assert!(seen[1].force);

        let missing = InitArgs {
            socket: Some(dir.path().join("nope.sock")),
            ..args(false)
        };
        let err = init(&config, missing).await.expect_err("no socket");
        assert!(err.contains("не открыт"), "{err}");
    }
}
