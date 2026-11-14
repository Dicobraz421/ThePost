//! Запуск интерфейса в терминале: crossterm, клиент Admin API (`tp-admin-client`) и цикл событий; тема по
//! окружению, знак бренда протоколом графики терминала, палитра консоли Linux на время работы.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{self, Event, KeyEventKind};
use ratatui::DefaultTerminal;
use ratatui_image::picker::Picker;
use serde_json::Value;
use tokio::sync::mpsc;
use tp_admin::server::WhoAmI;
use tp_admin_client::oidc::{DeviceLogin, prompt_text};
use tp_admin_client::{Client, ENV_TOKEN, ENV_URL, Options, base_url, setting};

use crate::app::{App, Effect, LoginField, LoginForm, LoginRequest, Page, Stage};
use crate::engine::Request;
use crate::node::{self, ConfigFile, NodeConfig, SaveError, Saved};
use crate::schema::Response;
use crate::theme::{ConsolePalette, Theme, console_palette_needed};
use crate::ui;

/// Как часто поток ввода проверяет, не закрыт ли интерфейс (ожидание клавиши не дольше).
const INPUT_POLL: Duration = Duration::from_millis(200);

/// Подключение к Admin API — те же значения, что у CLI `thepost admin` (адрес, CA, `--insecure`, токен).
#[derive(Debug, Default)]
pub struct Connect {
    /// Адрес Admin API; нет — `THEPOST_ADMIN_URL` (поле формы входа можно поправить).
    pub url: Option<String>,
    /// Токен: сессия `tps_…` или токен OIDC; нет — `THEPOST_ADMIN_TOKEN`. Есть — вход без формы.
    pub token: Option<String>,
    /// Имя входа для формы входа по паролю.
    pub user: Option<String>,
    /// Канал: корневые сертификаты, `--insecure`, предельное время запроса.
    pub options: Options,
    /// Вход с устройства через OIDC (настройки `auth.oidc` и `cli.oidc` читает вызывающий код); нет — способа
    /// «OIDC» в форме входа нет.
    pub oidc: Option<DeviceLogin>,
    /// Конфигурация узла, когда интерфейс запущен на самом узле с правом на `config.yaml`; нет — раздел
    /// «Конфигурация узла» только поясняет, где её править.
    pub node: Option<NodeConfig>,
    /// Режим темы (`--theme`): `auto`, `dark`, `light`, `256`, `16`, `mono`; нет — `THEPOST_TUI_THEME` и
    /// автоопределение по `NO_COLOR`, `COLORTERM`, `TERM`.
    pub theme: Option<String>,
}

enum Msg {
    Term(Event),
    Login(Result<(Client, WhoAmI, String), String>),
    Device(String),
    Done(u64, Result<Page, tp_admin_client::Error>),
    Config(Result<ConfigFile, String>),
    ConfigSaved(Result<Saved, SaveError>),
    /// `SIGTERM` или закрытие окна: выйти штатно (терминал и палитра консоли возвращаются).
    Quit,
}

/// Текстовый интерфейс `thepost admin tui`: до выхода по F10; терминал возвращается в прежний режим и при ошибке.
pub async fn run(mut connect: Connect) -> Result<(), String> {
    let theme = Theme::from_env(connect.theme.take().as_deref())?;
    let mut terminal = ratatui::try_init().map_err(|e| format!("терминал: {e}"))?;
    let palette = console_palette(&theme);
    // Опрос терминала — до потока ввода (ответ приходит в stdin).
    let theme = if theme.mode().graphics() {
        theme.with_mark(&Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks()))
    } else {
        theme
    };
    let result = event_loop(&mut terminal, connect, theme).await;
    let restored = ratatui::try_restore().map_err(|e| format!("терминал: {e}"));
    drop(palette);
    result.and(restored)
}

/// Палитра бренда на консоли Linux в 16 цветах; возвращается при сбросе и при панике.
fn console_palette(theme: &Theme) -> Option<ConsolePalette<io::Stdout>> {
    let term = std::env::var("TERM").ok();
    if !console_palette_needed(term.as_deref(), theme.mode()) {
        return None;
    }
    ConsolePalette::<io::Stdout>::reset_on_panic();
    ConsolePalette::set(io::stdout(), theme.console_palette()).ok()
}

async fn event_loop(
    terminal: &mut DefaultTerminal,
    connect: Connect,
    theme: Theme,
) -> Result<(), String> {
    let url = setting(connect.url, ENV_URL).unwrap_or_default();
    let token = setting(connect.token, ENV_TOKEN);
    let mut login = LoginForm::new(url.clone(), connect.oidc.is_some());
    if let Some(user) = connect.user {
        login.user = user;
        login.focus = LoginField::Secret;
    }
    let mut app = App::new(theme, login);
    app.node = connect.node.is_some();
    let node = connect.node;
    let options = connect.options;
    let oidc = connect.oidc.map(Arc::new);
    let (tx, mut rx) = mpsc::unbounded_channel();
    spawn_input(tx.clone());
    spawn_signal(tx.clone());
    let mut client: Option<Client> = None;
    if let Some(token) = token {
        let effects = vec![Effect::Login(LoginRequest::Token { url, token })];
        if let Stage::Login(f) = &mut app.stage {
            f.busy = true;
        }
        apply(
            effects,
            &tx,
            &options,
            &oidc,
            client.as_ref(),
            node.as_ref(),
        );
    }
    loop {
        terminal
            .draw(|f| ui::draw(f, &app))
            .map_err(|e| format!("терминал: {e}"))?;
        let Some(msg) = rx.recv().await else {
            return Ok(());
        };
        let effects = match msg {
            Msg::Term(Event::Key(k)) if k.kind != KeyEventKind::Release => app.on_key(k),
            Msg::Term(_) => Vec::new(),
            Msg::Login(Ok((c, who, server))) => {
                client = Some(c);
                app.on_login(Ok((who, server)))
            }
            Msg::Login(Err(e)) => app.on_login(Err(e)),
            Msg::Device(text) => {
                app.on_device(text);
                Vec::new()
            }
            Msg::Done(id, r) => app.on_result(id, r),
            Msg::Config(r) => {
                app.on_config(r);
                Vec::new()
            }
            Msg::ConfigSaved(r) => app.on_config_saved(r),
            Msg::Quit => return Ok(()),
        };
        if effects.contains(&Effect::Quit) {
            return Ok(());
        }
        apply(
            effects,
            &tx,
            &options,
            &oidc,
            client.as_ref(),
            node.as_ref(),
        );
    }
}

/// Выполнить действия интерфейса: вход, запросы и файл конфигурации узла — задачами, ответы — сообщениями в
/// цикл.
fn apply(
    effects: Vec<Effect>,
    tx: &mpsc::UnboundedSender<Msg>,
    options: &Options,
    oidc: &Option<Arc<DeviceLogin>>,
    client: Option<&Client>,
    node: Option<&NodeConfig>,
) {
    for e in effects {
        match e {
            Effect::Quit => {}
            Effect::ConfigLoad => {
                let tx = tx.clone();
                let node = node.cloned();
                tokio::task::spawn_blocking(move || {
                    let r = node
                        .ok_or_else(|| "конфигурация узла недоступна".to_owned())
                        .and_then(|n| node::load(&n));
                    let _ = tx.send(Msg::Config(r));
                });
            }
            Effect::ConfigSave {
                path,
                value,
                loaded,
            } => {
                let tx = tx.clone();
                let node = node.cloned();
                tokio::task::spawn_blocking(move || {
                    let r = match node {
                        Some(n) => node::save(&n, &loaded, &path, value),
                        None => Err(SaveError {
                            at_field: false,
                            message: "конфигурация узла недоступна".to_owned(),
                        }),
                    };
                    let _ = tx.send(Msg::ConfigSaved(r));
                });
            }
            Effect::Login(req) => {
                let tx = tx.clone();
                let options = options.clone();
                let oidc = oidc.clone();
                tokio::spawn(async move {
                    let r = login(req, &options, oidc, &tx).await;
                    let _ = tx.send(Msg::Login(r));
                });
            }
            Effect::Call(id, req) => {
                let tx = tx.clone();
                let Some(client) = client.cloned() else {
                    let _ = tx.send(Msg::Done(
                        id,
                        Err(tp_admin_client::Error::Client("нет входа".to_owned())),
                    ));
                    continue;
                };
                tokio::spawn(async move {
                    let r = call(&client, req).await;
                    let _ = tx.send(Msg::Done(id, r));
                });
            }
        }
    }
}

/// Поток ввода: события терминала в цикл, пока интерфейс открыт.
fn spawn_input(tx: mpsc::UnboundedSender<Msg>) {
    std::thread::spawn(move || {
        while !tx.is_closed() {
            match event::poll(INPUT_POLL) {
                Ok(true) => match event::read() {
                    Ok(ev) => {
                        if tx.send(Msg::Term(ev)).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                },
                Ok(false) => {}
                Err(_) => break,
            }
        }
    });
}

/// `SIGTERM` (в Windows — закрытие окна консоли) — сообщением в цикл: выход штатный, с возвратом терминала и
/// палитры.
fn spawn_signal(tx: mpsc::UnboundedSender<Msg>) {
    tokio::spawn(async move {
        #[cfg(unix)]
        let stop = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate());
        #[cfg(windows)]
        let stop = tokio::signal::windows::ctrl_close();
        if let Ok(mut stop) = stop {
            stop.recv().await;
            let _ = tx.send(Msg::Quit);
        }
    });
}

/// Вход тем же клиентом, что у CLI: по паролю (`POST /api/v1/session`), токеном или с устройства (OIDC); затем
/// `whoami`.
async fn login(
    req: LoginRequest,
    options: &Options,
    oidc: Option<Arc<DeviceLogin>>,
    tx: &mpsc::UnboundedSender<Msg>,
) -> Result<(Client, WhoAmI, String), String> {
    let (url, token) = match req {
        LoginRequest::Password {
            url,
            user,
            password,
        } => {
            let anon = Client::anonymous(base_url(&url)?, options)?;
            (url, anon.login(&user, password).await?.session)
        }
        LoginRequest::Token { url, token } => (url, tp_admin_client::token(Some(token))?),
        LoginRequest::Oidc { url } => {
            let oidc =
                oidc.ok_or("вход через OIDC не настроен (auth.oidc и cli.oidc в config.yaml)")?;
            let token = oidc
                .login(|code| {
                    let _ = tx.send(Msg::Device(prompt_text(code)));
                })
                .await?;
            (url, token)
        }
    };
    let base = base_url(&url)?;
    let server = match (base.host_str(), base.port()) {
        (Some(h), Some(p)) => format!("{h}:{p}"),
        (Some(h), None) => h.to_owned(),
        _ => url.clone(),
    };
    let client = Client::new(base, token, options)?;
    let who = client.whoami().await?;
    Ok((client, who, server))
}

/// Запрос экрана к Admin API; следующая страница — по ссылке `Link: rel="next"`.
async fn call(client: &Client, req: Request) -> Result<Page, tp_admin_client::Error> {
    let op = req.op.op();
    let params: Vec<&str> = req.params.iter().map(String::as_str).collect();
    if req.op.response == Response::Empty {
        client
            .call_empty(op, &params, &req.query, req.body.as_ref())
            .await?;
        return Ok(Page {
            value: Value::Null,
            next: None,
        });
    }
    let (value, link): (Value, Option<String>) = client
        .call_page(op, &params, &req.query, req.body.as_ref())
        .await?;
    let next = link.map(|l| client.link_query(&l)).transpose()?;
    Ok(Page { value, next })
}
