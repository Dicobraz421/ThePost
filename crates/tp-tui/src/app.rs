//! Состояние интерфейса и клавиши (TUI-01): вход, две панели (дерево ресурсов и список/карточка), F1–F10.
//! Сети здесь нет: действия возвращают [`Effect`], ответы Admin API приходят в [`App::on_result`] — поэтому экран
//! проверяется тестами без узла.

use std::collections::BTreeMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Map, Value};
use tp_admin::server::WhoAmI;

use crate::cli;
use crate::engine::{self, Context, Form, Request, ScreenKind};
use crate::schema::{Kind, Operation, catalog};
use crate::theme::Theme;
use crate::tree::{ORG_ROWS, SECTIONS};

/// Строк списка в правой панели.
pub const LIST_ROWS: usize = 14;
/// Строк карточки, формы и меню в правой панели.
pub const BODY_ROWS: usize = 15;

/// Способ входа.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// Имя входа и пароль (решение 117).
    Password,
    /// OIDC, вход с устройства (RFC 8628).
    Oidc,
    /// Готовый токен: сессия `tps_…` или токен OIDC.
    Token,
}

impl Method {
    /// Подпись.
    pub fn label(self) -> &'static str {
        match self {
            Self::Password => "пароль",
            Self::Oidc => "OIDC",
            Self::Token => "токен",
        }
    }
}

/// Поле формы входа.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginField {
    /// Адрес Admin API.
    Url,
    /// Способ входа.
    Method,
    /// Имя входа.
    User,
    /// Пароль или токен.
    Secret,
}

/// Форма входа.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginForm {
    /// Адрес Admin API.
    pub url: String,
    /// Способ входа.
    pub method: Method,
    /// Имя входа.
    pub user: String,
    /// Пароль или токен (на экране — точки).
    pub secret: String,
    /// Поле под курсором.
    pub focus: LoginField,
    /// Вход через OIDC настроен.
    pub oidc: bool,
    /// Идёт вход.
    pub busy: bool,
    /// Адрес и код входа с устройства.
    pub device: Option<String>,
    /// Ошибка последней попытки.
    pub error: Option<String>,
}

impl LoginForm {
    /// Форма с адресом `url`; `oidc` — вход через OIDC настроен.
    pub fn new(url: String, oidc: bool) -> Self {
        Self {
            url,
            method: Method::Password,
            user: String::new(),
            secret: String::new(),
            focus: LoginField::User,
            oidc,
            busy: false,
            device: None,
            error: None,
        }
    }

    /// Поля способа входа по порядку.
    pub fn fields(&self) -> &'static [LoginField] {
        match self.method {
            Method::Password => &[
                LoginField::Url,
                LoginField::Method,
                LoginField::User,
                LoginField::Secret,
            ],
            Method::Oidc => &[LoginField::Url, LoginField::Method],
            Method::Token => &[LoginField::Url, LoginField::Method, LoginField::Secret],
        }
    }

    fn methods(&self) -> Vec<Method> {
        let mut m = vec![Method::Password];
        if self.oidc {
            m.push(Method::Oidc);
        }
        m.push(Method::Token);
        m
    }

    fn key(&mut self, key: KeyEvent) -> Option<Effect> {
        if self.busy {
            return None;
        }
        let fields = self.fields();
        let at = fields.iter().position(|f| *f == self.focus).unwrap_or(0);
        match key.code {
            KeyCode::Up | KeyCode::BackTab => {
                self.focus = fields[(at + fields.len() - 1) % fields.len()];
            }
            KeyCode::Down | KeyCode::Tab => self.focus = fields[(at + 1) % fields.len()],
            KeyCode::Left | KeyCode::Right if self.focus == LoginField::Method => {
                let ms = self.methods();
                let i = ms.iter().position(|m| *m == self.method).unwrap_or(0);
                let step = if key.code == KeyCode::Left {
                    ms.len() - 1
                } else {
                    1
                };
                self.method = ms[(i + step) % ms.len()];
                self.secret.clear();
            }
            KeyCode::Enter => {
                let url = self.url.trim().to_owned();
                let req = match self.method {
                    Method::Password if self.user.trim().is_empty() => {
                        self.focus = LoginField::User;
                        self.error = Some("введите имя входа".to_owned());
                        return None;
                    }
                    Method::Password => LoginRequest::Password {
                        url,
                        user: self.user.trim().to_owned(),
                        password: self.secret.clone(),
                    },
                    Method::Oidc => LoginRequest::Oidc { url },
                    Method::Token => LoginRequest::Token {
                        url,
                        token: self.secret.trim().to_owned(),
                    },
                };
                self.busy = true;
                self.error = None;
                return Some(Effect::Login(req));
            }
            KeyCode::Backspace => {
                if let Some(s) = self.text() {
                    s.pop();
                }
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(s) = self.text() {
                    s.push(c);
                }
            }
            _ => {}
        }
        None
    }

    fn text(&mut self) -> Option<&mut String> {
        match self.focus {
            LoginField::Url => Some(&mut self.url),
            LoginField::User => Some(&mut self.user),
            LoginField::Secret => Some(&mut self.secret),
            LoginField::Method => None,
        }
    }
}

/// Вход, который выполняет запускающий код.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginRequest {
    /// По паролю.
    Password {
        /// Адрес Admin API.
        url: String,
        /// Имя входа.
        user: String,
        /// Пароль.
        password: String,
    },
    /// С устройства через OIDC.
    Oidc {
        /// Адрес Admin API.
        url: String,
    },
    /// Готовым токеном.
    Token {
        /// Адрес Admin API.
        url: String,
        /// Токен.
        token: String,
    },
}

/// Действие для запускающего кода.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// Войти.
    Login(LoginRequest),
    /// Запрос к Admin API; ответ — [`App::on_result`] с тем же номером.
    Call(u64, Request),
    /// Выйти из интерфейса.
    Quit,
}

/// Ответ Admin API на запрос.
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    /// Тело ответа (`null` — без тела).
    pub value: Value,
    /// Параметры запроса следующей страницы по ссылке `Link: rel="next"`.
    pub next: Option<Vec<(String, String)>>,
}

/// Важность сообщения строки состояния.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Норма ●.
    Ok,
    /// Внимание ▲.
    Warn,
    /// Ошибка ×.
    Err,
}

/// Активная панель.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    /// Дерево ресурсов.
    Tree,
    /// Список или карточка.
    Main,
}

/// Вошедший администратор.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// Имя входа.
    pub login: String,
    /// Организация администратора.
    pub home_org: String,
    /// Узел (адрес Admin API).
    pub server: String,
}

/// Список записей.
#[derive(Debug, Clone, PartialEq)]
pub struct ListView {
    /// Запрос первой страницы.
    pub req: Request,
    /// Записи загруженных страниц.
    pub rows: Vec<Value>,
    /// Выделенная запись.
    pub sel: usize,
    /// Первая видимая запись.
    pub top: usize,
    /// Параметры запроса следующей страницы.
    pub next: Option<Vec<(String, String)>>,
}

impl ListView {
    /// Выделенная запись.
    pub fn row(&self) -> Option<&Value> {
        self.rows.get(self.sel)
    }

    fn select(&mut self, sel: usize) {
        self.sel = sel.min(self.rows.len().saturating_sub(1));
        if self.sel < self.top {
            self.top = self.sel;
        } else if self.sel >= self.top + LIST_ROWS {
            self.top = self.sel + 1 - LIST_ROWS;
        }
    }
}

/// Карточка записи или результат действия.
#[derive(Debug, Clone, PartialEq)]
pub struct CardView {
    /// Запрос.
    pub req: Request,
    /// Заголовок.
    pub title: String,
    /// Тело ответа.
    pub value: Value,
    /// Первая видимая строка.
    pub top: usize,
    /// Список, из которого открыта карточка.
    pub back: Option<Box<ListView>>,
    /// Перечитать список при возврате.
    pub reload: bool,
}

/// Содержимое правой панели.
#[derive(Debug, Clone, PartialEq)]
pub enum View {
    /// Пояснение.
    Info {
        /// Заголовок.
        title: String,
        /// Строки.
        lines: Vec<String>,
    },
    /// Список.
    List(ListView),
    /// Карточка.
    Card(CardView),
}

/// Окно поверх правой панели.
#[derive(Debug, Clone, PartialEq)]
pub enum Overlay {
    /// Справка F1.
    Help,
    /// Меню F9: все операции Admin API по разделам схемы.
    Menu {
        /// Выделенная операция (индекс в каталоге).
        sel: usize,
    },
    /// Форма.
    Form(Box<Form>),
    /// Подтверждение удаления.
    Confirm(Request),
}

/// Состояние основного экрана.
#[derive(Debug, Clone, PartialEq)]
pub struct Main {
    /// Вошедший администратор.
    pub session: Session,
    /// Текущая организация.
    pub org: String,
    /// Другие организации установки.
    pub orgs: Vec<String>,
    /// Активная панель.
    pub focus: Panel,
    /// Выделенная строка дерева.
    pub tree: usize,
    /// Раздел правой панели.
    pub section: String,
    /// Правая панель.
    pub view: View,
    /// Окно поверх правой панели.
    pub overlay: Option<Overlay>,
}

/// Строка дерева.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeRow {
    /// Раздел организации.
    Section(usize),
    /// Другая организация.
    Org(usize),
    /// Конфигурация узла.
    Config,
}

impl Main {
    /// Строки дерева по порядку.
    pub fn tree_rows(&self) -> Vec<TreeRow> {
        (0..SECTIONS.len())
            .map(TreeRow::Section)
            .chain((0..self.orgs.len().min(ORG_ROWS)).map(TreeRow::Org))
            .chain([TreeRow::Config])
            .collect()
    }

    /// Подпись организации: своя — домен имени входа администратора, чужая — начало идентификатора.
    pub fn org_label(&self, org: &str) -> String {
        match self.session.login.split_once('@') {
            Some((_, domain)) if org == self.session.home_org => domain.to_owned(),
            _ => org.chars().take(8).collect(),
        }
    }

    fn ctx(&self) -> Context {
        Context::from([("org".to_owned(), self.org.clone())])
    }
}

/// Этап работы.
#[derive(Debug, Clone, PartialEq)]
pub enum Stage {
    /// Вход.
    Login(LoginForm),
    /// Основной экран.
    Main(Box<Main>),
}

#[derive(Debug, Clone, PartialEq)]
enum Pending {
    Orgs,
    Load {
        req: Request,
        form: Option<Box<Form>>,
        append: bool,
    },
    Submit {
        req: Request,
        form: Option<Box<Form>>,
    },
}

/// Запросы в пути и строка состояния.
#[derive(Debug, Clone, Default, PartialEq)]
struct Bus {
    pending: BTreeMap<u64, Pending>,
    seq: u64,
    status: Option<(Level, String)>,
}

impl Bus {
    fn call(&mut self, req: Request, p: Pending) -> Effect {
        self.seq += 1;
        self.pending.insert(self.seq, p);
        Effect::Call(self.seq, req)
    }

    fn load(&mut self, req: Request, form: Option<Box<Form>>, append: bool) -> Effect {
        self.call(req.clone(), Pending::Load { req, form, append })
    }

    fn submit(&mut self, req: Request, form: Option<Box<Form>>) -> Effect {
        self.call(req.clone(), Pending::Submit { req, form })
    }

    fn say(&mut self, level: Level, text: impl Into<String>) {
        self.status = Some((level, text.into()));
    }
}

/// Интерфейс целиком.
#[derive(Debug, Clone, PartialEq)]
pub struct App {
    /// Тема.
    pub theme: Theme,
    /// Этап.
    pub stage: Stage,
    bus: Bus,
}

impl App {
    /// Интерфейс на экране входа.
    pub fn new(theme: Theme, login: LoginForm) -> Self {
        Self {
            theme,
            stage: Stage::Login(login),
            bus: Bus::default(),
        }
    }

    /// Ждёт ли интерфейс ответа Admin API.
    pub fn busy(&self) -> bool {
        !self.bus.pending.is_empty()
    }

    /// Сообщение строки состояния.
    pub fn status(&self) -> Option<&(Level, String)> {
        self.bus.status.as_ref()
    }

    /// Клавиша.
    pub fn on_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if key.code == KeyCode::F(10)
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            return vec![Effect::Quit];
        }
        match &mut self.stage {
            Stage::Login(form) => form.key(key).into_iter().collect(),
            Stage::Main(m) => {
                self.bus.status = None;
                main_key(m, &mut self.bus, key)
            }
        }
    }

    /// Итог входа: администратор и узел, или ошибка.
    pub fn on_login(&mut self, result: Result<(WhoAmI, String), String>) -> Vec<Effect> {
        let Stage::Login(form) = &mut self.stage else {
            return Vec::new();
        };
        form.busy = false;
        form.device = None;
        let (who, server) = match result {
            Ok(r) => r,
            Err(e) => {
                form.error = Some(e);
                return Vec::new();
            }
        };
        let mut m = Box::new(Main {
            org: who.tenant.clone(),
            session: Session {
                login: who.login,
                home_org: who.tenant,
                server,
            },
            orgs: Vec::new(),
            focus: Panel::Main,
            tree: 0,
            section: String::new(),
            view: View::Info {
                title: String::new(),
                lines: Vec::new(),
            },
            overlay: None,
        });
        let mut out = Vec::new();
        if let Some(op) = catalog().get("list_orgs")
            && let Ok(req) = Request::new(op, &Context::new(), Vec::new())
        {
            out.push(self.bus.call(req, Pending::Orgs));
        }
        out.extend(open_tree(&mut m, &mut self.bus));
        self.stage = Stage::Main(m);
        out
    }

    /// Адрес и код входа с устройства.
    pub fn on_device(&mut self, text: String) {
        if let Stage::Login(form) = &mut self.stage {
            form.device = Some(text);
        }
    }

    /// Ответ Admin API на запрос `id`.
    pub fn on_result(
        &mut self,
        id: u64,
        result: Result<Page, tp_admin_client::Error>,
    ) -> Vec<Effect> {
        let Some(p) = self.bus.pending.remove(&id) else {
            return Vec::new();
        };
        let Stage::Main(m) = &mut self.stage else {
            return Vec::new();
        };
        let page = match result {
            Ok(page) => page,
            Err(e) => {
                if let (
                    Pending::Load {
                        form: Some(mut f), ..
                    }
                    | Pending::Submit {
                        form: Some(mut f), ..
                    },
                    tp_admin_client::Error::Api {
                        problem: Some(pr), ..
                    },
                ) = (p, &e)
                {
                    f.apply_problem(pr);
                    m.overlay = Some(Overlay::Form(f));
                }
                self.bus.say(Level::Err, e.to_string());
                return Vec::new();
            }
        };
        match p {
            Pending::Orgs => {
                let me = m.org.clone();
                m.orgs = page
                    .value
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|o| o.get("id").and_then(Value::as_str))
                    .filter(|id| *id != me)
                    .map(str::to_owned)
                    .collect();
                Vec::new()
            }
            Pending::Load { req, append, .. } => {
                loaded(m, req, page, append);
                Vec::new()
            }
            Pending::Submit { req, .. } => {
                self.bus
                    .say(Level::Ok, format!("готово: {}", req.op.title()));
                let back = take_list(&mut m.view);
                if !page.value.is_null() {
                    m.view = View::Card(CardView {
                        title: format!("Результат: {}", req.op.title()),
                        req,
                        value: page.value,
                        top: 0,
                        back,
                        reload: true,
                    });
                    return Vec::new();
                }
                match back {
                    Some(list) => {
                        let r = list.req.clone();
                        m.view = View::List(*list);
                        vec![self.bus.load(r, None, false)]
                    }
                    None => Vec::new(),
                }
            }
        }
    }

    /// Эквивалентная команда CLI для действия на экране.
    pub fn command_line(&self) -> String {
        match &self.stage {
            Stage::Login(f) => match f.method {
                Method::Password => format!(
                    "thepost admin login --user {}",
                    if f.user.trim().is_empty() {
                        "<имя_входа>"
                    } else {
                        f.user.trim()
                    }
                ),
                Method::Oidc => "thepost admin login --oidc".to_owned(),
                Method::Token => "THEPOST_ADMIN_TOKEN=… thepost admin whoami".to_owned(),
            },
            Stage::Main(m) => main_command(m),
        }
    }
}

fn main_command(m: &Main) -> String {
    let home = Some(m.session.home_org.as_str());
    let show = |req: &Request| cli::command(req, home);
    match &m.overlay {
        Some(Overlay::Form(f)) => return show(&f.preview()),
        Some(Overlay::Confirm(req)) => return show(req),
        Some(Overlay::Menu { sel }) => {
            return catalog()
                .ops
                .get(*sel)
                .map(|op| show(&Form::new(op, &m.ctx(), false, &Map::new()).preview()))
                .unwrap_or_default();
        }
        Some(Overlay::Help) | None => {}
    }
    if m.focus == Panel::Tree {
        return match m.tree_rows().get(m.tree) {
            Some(TreeRow::Section(i)) => SECTIONS[*i]
                .op
                .and_then(|id| catalog().get(id))
                .map(|op| show(&Form::new(op, &m.ctx(), false, &Map::new()).preview()))
                .unwrap_or_default(),
            Some(TreeRow::Config) => "thepost config check".to_owned(),
            _ => String::new(),
        };
    }
    match &m.view {
        View::List(l) => {
            let ops = engine::item_ops(catalog(), l.req.op);
            match (ops.view, ops.param, l.row()) {
                (Some(view), Some(param), Some(row)) => item_ctx(&l.req, param, row)
                    .and_then(|ctx| Request::new(view, &ctx, Vec::new()).ok())
                    .map_or_else(|| show(&l.req), |r| show(&r)),
                _ => show(&l.req),
            }
        }
        View::Card(c) => show(&c.req),
        View::Info { .. } => String::new(),
    }
}

/// Контекст запроса: параметры пути по именам.
fn req_ctx(req: &Request) -> Context {
    req.op
        .path_names()
        .into_iter()
        .map(str::to_owned)
        .zip(req.params.iter().cloned())
        .collect()
}

/// Контекст записи списка: параметры пути списка и ключ записи.
fn item_ctx(list: &Request, param: &str, row: &Value) -> Option<Context> {
    let mut ctx = req_ctx(list);
    ctx.insert(param.to_owned(), engine::row_key(row, param)?);
    Some(ctx)
}

fn take_list(view: &mut View) -> Option<Box<ListView>> {
    let v = std::mem::replace(
        view,
        View::Info {
            title: String::new(),
            lines: Vec::new(),
        },
    );
    match v {
        View::List(l) => Some(Box::new(l)),
        View::Card(c) => c.back,
        other => {
            *view = other;
            None
        }
    }
}

fn loaded(m: &mut Main, req: Request, page: Page, append: bool) {
    if !req.op.is_list() {
        let back = take_list(&mut m.view);
        m.view = View::Card(CardView {
            title: req.op.title(),
            req,
            value: page.value,
            top: 0,
            back,
            reload: false,
        });
        return;
    }
    let (rows, cursor) = engine::page(req.op, &page.value);
    let next = page.next.or_else(|| {
        cursor.map(|(k, v)| {
            let mut q: Vec<(String, String)> =
                req.query.iter().filter(|(n, _)| *n != k).cloned().collect();
            q.push((k, v));
            q
        })
    });
    if append && let View::List(l) = &mut m.view {
        l.rows.extend(rows);
        l.next = next;
        let sel = l.sel + 1;
        l.select(sel);
        return;
    }
    m.view = View::List(ListView {
        req,
        rows,
        sel: 0,
        top: 0,
        next,
    });
}

/// Открыть экран операции `op` в контексте `ctx`: список и карточка загружаются, если известны все параметры
/// пути и нет обязательных параметров запроса, иначе — форма; удаление — подтверждение.
fn open_op(
    m: &mut Main,
    bus: &mut Bus,
    op: &'static Operation,
    ctx: &Context,
    prefill: &Map<String, Value>,
) -> Vec<Effect> {
    let form = || Overlay::Form(Box::new(Form::new(op, ctx, false, prefill)));
    let ready = Request::new(op, ctx, Vec::new()).ok();
    match (engine::screen_kind(op), ready) {
        (ScreenKind::List | ScreenKind::Card, Some(req)) if !op.query().any(|f| f.required) => {
            return vec![bus.load(req, None, false)];
        }
        (ScreenKind::Confirm, Some(req)) if !op.query().any(|f| f.required) => {
            m.overlay = Some(Overlay::Confirm(req));
        }
        _ => m.overlay = Some(form()),
    }
    Vec::new()
}

fn open_tree(m: &mut Main, bus: &mut Bus) -> Vec<Effect> {
    let info = |title: &str, lines: &[&str]| View::Info {
        title: title.to_owned(),
        lines: lines.iter().map(|l| (*l).to_owned()).collect(),
    };
    match m.tree_rows().get(m.tree).copied() {
        Some(TreeRow::Section(i)) => {
            let s = SECTIONS[i];
            m.section = s.label.to_owned();
            m.focus = Panel::Main;
            match s.op.and_then(|id| catalog().get(id)) {
                Some(op) => {
                    let ctx = m.ctx();
                    open_op(m, bus, op, &ctx, &Map::new())
                }
                None => {
                    m.view = info(
                        s.label,
                        &[
                            "В Admin API пока нет операции этого раздела.",
                            "Раздел заработает, когда операция появится",
                            "в схеме Admin API.",
                        ],
                    );
                    Vec::new()
                }
            }
        }
        Some(TreeRow::Org(i)) => {
            let Some(next) = m.orgs.get(i).cloned() else {
                return Vec::new();
            };
            let prev = std::mem::replace(&mut m.org, next);
            m.orgs[i] = prev;
            m.tree = 0;
            open_tree(m, bus)
        }
        Some(TreeRow::Config) => {
            m.section = crate::tree::NODE_CONFIG.to_owned();
            m.view = info(
                crate::tree::NODE_CONFIG,
                &[
                    "Правка config.yaml — на самом узле, когда",
                    "интерфейс запущен там с правом на файл.",
                    "Проверка: thepost config check.",
                ],
            );
            Vec::new()
        }
        None => Vec::new(),
    }
}

fn main_key(m: &mut Main, bus: &mut Bus, key: KeyEvent) -> Vec<Effect> {
    if let Some(ov) = m.overlay.take() {
        return overlay_key(m, bus, ov, key);
    }
    match key.code {
        KeyCode::Tab | KeyCode::BackTab => {
            m.focus = match m.focus {
                Panel::Tree => Panel::Main,
                Panel::Main => Panel::Tree,
            };
            return Vec::new();
        }
        KeyCode::F(1) => {
            m.overlay = Some(Overlay::Help);
            return Vec::new();
        }
        KeyCode::F(9) => {
            m.overlay = Some(Overlay::Menu { sel: 0 });
            return Vec::new();
        }
        _ => {}
    }
    match m.focus {
        Panel::Tree => {
            let n = m.tree_rows().len();
            match key.code {
                KeyCode::Up => m.tree = m.tree.saturating_sub(1),
                KeyCode::Down => m.tree = (m.tree + 1).min(n - 1),
                KeyCode::Home => m.tree = 0,
                KeyCode::End => m.tree = n - 1,
                KeyCode::Enter | KeyCode::Right => return open_tree(m, bus),
                _ => {}
            }
            Vec::new()
        }
        Panel::Main => view_key(m, bus, key),
    }
}

fn view_key(m: &mut Main, bus: &mut Bus, key: KeyEvent) -> Vec<Effect> {
    let busy = !bus.pending.is_empty();
    match &mut m.view {
        View::List(l) => {
            let last = l.rows.len().saturating_sub(1);
            let at_end = l.sel >= last;
            match key.code {
                KeyCode::Up => l.select(l.sel.saturating_sub(1)),
                KeyCode::PageUp => l.select(l.sel.saturating_sub(LIST_ROWS)),
                KeyCode::Home => l.select(0),
                KeyCode::Down | KeyCode::PageDown | KeyCode::End if at_end => {
                    if let Some(next) = l.next.clone()
                        && !busy
                    {
                        let req = Request {
                            query: next,
                            ..l.req.clone()
                        };
                        bus.say(Level::Ok, "загрузка следующей страницы…");
                        return vec![bus.load(req, None, true)];
                    }
                }
                KeyCode::Down => l.select(l.sel + 1),
                KeyCode::PageDown => l.select(l.sel + LIST_ROWS),
                KeyCode::End => l.select(last),
                _ => return list_action(m, bus, key),
            }
            Vec::new()
        }
        View::Card(c) => {
            match key.code {
                KeyCode::Up => c.top = c.top.saturating_sub(1),
                KeyCode::Down => c.top += 1,
                KeyCode::PageUp => c.top = c.top.saturating_sub(BODY_ROWS),
                KeyCode::PageDown => c.top += BODY_ROWS,
                KeyCode::Home => c.top = 0,
                KeyCode::Esc | KeyCode::Backspace | KeyCode::Left => {
                    if let Some(back) = c.back.take() {
                        let reload = c.reload.then(|| back.req.clone());
                        m.view = View::List(*back);
                        if let Some(r) = reload {
                            return vec![bus.load(r, None, false)];
                        }
                    }
                }
                _ => return list_action(m, bus, key),
            }
            Vec::new()
        }
        View::Info { .. } => Vec::new(),
    }
}

/// Действия F2–F8 над списком или карточкой записи.
fn list_action(m: &mut Main, bus: &mut Bus, key: KeyEvent) -> Vec<Effect> {
    let c = catalog();
    let (list_req, row, item): (Request, Option<Value>, Option<Context>) = match &m.view {
        View::List(l) => {
            let ops = engine::item_ops(c, l.req.op);
            let item = ops
                .param
                .zip(l.row())
                .and_then(|(p, row)| item_ctx(&l.req, p, row));
            (l.req.clone(), l.row().cloned(), item)
        }
        View::Card(card) => match &card.back {
            Some(back) => (
                back.req.clone(),
                Some(card.value.clone()),
                Some(req_ctx(&card.req)),
            ),
            None => return Vec::new(),
        },
        View::Info { .. } => return Vec::new(),
    };
    let ops = engine::item_ops(c, list_req.op);
    let row_map = row
        .as_ref()
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let need = |bus: &mut Bus, what: &str| {
        bus.say(Level::Warn, format!("{what}: нет в разделе"));
        Vec::new()
    };
    match key.code {
        KeyCode::F(2) => {
            let mut prefill = Map::new();
            for (k, v) in req_ctx(&list_req) {
                prefill.insert(k, Value::String(v));
            }
            for (k, v) in &list_req.query {
                prefill.insert(k.clone(), Value::String(v.clone()));
            }
            let mut ctx = m.ctx();
            ctx.retain(|k, _| list_req.op.path_names().contains(&k.as_str()));
            m.overlay = Some(Overlay::Form(Box::new(Form::new(
                list_req.op,
                &ctx,
                false,
                &prefill,
            ))));
            Vec::new()
        }
        KeyCode::Enter | KeyCode::F(3) => match (ops.view, &item) {
            (Some(view), Some(ctx)) => open_op(m, bus, view, ctx, &Map::new()),
            (None, _) if matches!(m.view, View::List(_)) && row.is_some() => {
                let back = take_list(&mut m.view);
                m.view = View::Card(CardView {
                    title: list_req.op.title(),
                    req: list_req,
                    value: row.unwrap_or_default(),
                    top: 0,
                    back,
                    reload: false,
                });
                Vec::new()
            }
            _ => Vec::new(),
        },
        KeyCode::F(4) => match (ops.edit, &item) {
            (Some(op), Some(ctx)) => open_op(m, bus, op, ctx, &row_map),
            _ => need(bus, "правка"),
        },
        KeyCode::F(5) => {
            let block = ops.edit.filter(|op| {
                op.body.as_ref().is_some_and(|b| {
                    b.fields
                        .iter()
                        .any(|f| f.name == "enabled" && f.kind == Kind::Boolean)
                })
            });
            match (block, &item) {
                (Some(op), Some(ctx)) => {
                    let enabled = row_map
                        .get("enabled")
                        .and_then(Value::as_bool)
                        .unwrap_or(true);
                    let prefill = Map::from_iter([("enabled".to_owned(), Value::Bool(!enabled))]);
                    open_op(m, bus, op, ctx, &prefill)
                }
                _ => need(bus, "блокировка"),
            }
        }
        KeyCode::F(6) => match (ops.password, &item) {
            (Some(op), Some(ctx)) => open_op(m, bus, op, ctx, &Map::new()),
            _ => need(bus, "пароль"),
        },
        KeyCode::F(7) => match ops.create {
            Some(op) => {
                let ctx = req_ctx(&list_req);
                open_op(m, bus, op, &ctx, &Map::new())
            }
            None => need(bus, "создание"),
        },
        KeyCode::F(8) => match (ops.delete, &item) {
            (Some(op), Some(ctx)) => open_op(m, bus, op, ctx, &Map::new()),
            _ => need(bus, "удаление"),
        },
        _ => Vec::new(),
    }
}

fn overlay_key(m: &mut Main, bus: &mut Bus, ov: Overlay, key: KeyEvent) -> Vec<Effect> {
    match ov {
        Overlay::Help => Vec::new(),
        Overlay::Menu { sel } => {
            let n = catalog().ops.len();
            let sel = match key.code {
                KeyCode::Esc | KeyCode::F(9) => return Vec::new(),
                KeyCode::Up => sel.saturating_sub(1),
                KeyCode::Down => (sel + 1).min(n - 1),
                KeyCode::PageUp => sel.saturating_sub(BODY_ROWS),
                KeyCode::PageDown => (sel + BODY_ROWS).min(n - 1),
                KeyCode::Home => 0,
                KeyCode::End => n - 1,
                KeyCode::Enter => {
                    let op = &catalog().ops[sel];
                    m.section = op.title();
                    m.focus = Panel::Main;
                    let ctx = m.ctx();
                    return open_op(m, bus, op, &ctx, &Map::new());
                }
                _ => sel,
            };
            m.overlay = Some(Overlay::Menu { sel });
            Vec::new()
        }
        Overlay::Confirm(req) => match key.code {
            KeyCode::Enter | KeyCode::Char('y' | 'д') => {
                vec![bus.submit(req, None)]
            }
            KeyCode::Esc | KeyCode::Char('n' | 'н') => Vec::new(),
            _ => {
                m.overlay = Some(Overlay::Confirm(req));
                Vec::new()
            }
        },
        Overlay::Form(mut f) => {
            let n = f.fields.len();
            match key.code {
                KeyCode::Esc => return Vec::new(),
                KeyCode::Up | KeyCode::BackTab if n > 0 => f.focus = (f.focus + n - 1) % n,
                KeyCode::Down | KeyCode::Tab if n > 0 => f.focus = (f.focus + 1) % n,
                KeyCode::Left | KeyCode::Right if n > 0 => {
                    let ff = &mut f.fields[f.focus];
                    let mut choices: Vec<String> = ff.field.choices.clone();
                    if ff.field.kind == Kind::Boolean {
                        choices = vec!["да".to_owned(), "нет".to_owned()];
                    }
                    if !choices.is_empty() {
                        if !ff.field.required {
                            choices.insert(0, String::new());
                        }
                        let i = choices.iter().position(|c| *c == ff.value);
                        let k = choices.len();
                        let j = match (i, key.code) {
                            (None, _) => 0,
                            (Some(i), KeyCode::Left) => (i + k - 1) % k,
                            (Some(i), _) => (i + 1) % k,
                        };
                        ff.value = choices[j].clone();
                    }
                }
                KeyCode::Backspace if n > 0 => {
                    let at = f.focus;
                    f.fields[at].value.pop();
                }
                KeyCode::Char(ch) if n > 0 && !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    let at = f.focus;
                    f.fields[at].value.push(ch);
                }
                KeyCode::Enter => return submit(m, bus, f),
                _ => {}
            }
            m.overlay = Some(Overlay::Form(f));
            Vec::new()
        }
    }
}

fn submit(m: &mut Main, bus: &mut Bus, mut f: Box<Form>) -> Vec<Effect> {
    let Some(req) = f.request() else {
        m.overlay = Some(Overlay::Form(f));
        return Vec::new();
    };
    match engine::screen_kind(f.op) {
        ScreenKind::List | ScreenKind::Card => vec![bus.load(req, Some(f), false)],
        ScreenKind::Confirm => {
            m.overlay = Some(Overlay::Confirm(req));
            Vec::new()
        }
        ScreenKind::Form => vec![bus.submit(req, Some(f))],
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    pub(crate) fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    pub(crate) fn who() -> WhoAmI {
        WhoAmI {
            tenant: "00000000-0000-0000-0000-000000000001".into(),
            account: "a0".into(),
            login: "admin@it-task.ru".into(),
            roles: Vec::new(),
        }
    }

    /// Вошедший интерфейс с открытым списком учётных записей.
    pub(crate) fn with_accounts() -> App {
        let mut app = App::new(
            Theme { mono: false },
            LoginForm::new("https://node:9443".into(), false),
        );
        let fx = app.on_login(Ok((who(), "node:9443".into())));
        let ids: Vec<u64> = fx
            .iter()
            .filter_map(|e| match e {
                Effect::Call(id, _) => Some(*id),
                _ => None,
            })
            .collect();
        assert_eq!(ids.len(), 2, "{fx:?}");
        app.on_result(
            ids[0],
            Ok(Page {
                value: json!([{"id": "00000000-0000-0000-0000-000000000001", "default": true}]),
                next: None,
            }),
        );
        let rows: Vec<Value> = (0..20)
            .map(|i| {
                json!({"id": format!("acc-{i}"), "login": format!("user{i}@it-task.ru"),
                       "display_name": format!("Пользователь {i}"), "enabled": true})
            })
            .collect();
        app.on_result(
            ids[1],
            Ok(Page {
                value: Value::Array(rows),
                next: Some(vec![("after".into(), "user19@it-task.ru".into())]),
            }),
        );
        app
    }

    fn main(app: &App) -> &Main {
        match &app.stage {
            Stage::Main(m) => m,
            Stage::Login(_) => panic!("не вошли"),
        }
    }

    #[test]
    fn login_by_password() {
        let mut app = App::new(
            Theme { mono: true },
            LoginForm::new("https://node:9443".into(), true),
        );
        for c in "admin@it-task.ru".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(
            app.command_line(),
            "thepost admin login --user admin@it-task.ru"
        );
        app.on_key(key(KeyCode::Down));
        for c in "секрет".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        let fx = app.on_key(key(KeyCode::Enter));
        assert_eq!(
            fx,
            vec![Effect::Login(LoginRequest::Password {
                url: "https://node:9443".into(),
                user: "admin@it-task.ru".into(),
                password: "секрет".into(),
            })]
        );
        app.on_login(Err("401".into()));
        let Stage::Login(f) = &app.stage else {
            panic!("не экран входа");
        };
        assert_eq!(f.error.as_deref(), Some("401"));
        // Способ входа: ← → по полю способа.
        app.on_key(key(KeyCode::Up));
        app.on_key(key(KeyCode::Up));
        app.on_key(key(KeyCode::Right));
        assert_eq!(app.command_line(), "thepost admin login --oidc");
        assert_eq!(app.on_key(key(KeyCode::F(10))), vec![Effect::Quit]);
    }

    #[test]
    fn list_navigation_and_paging() {
        let mut app = with_accounts();
        assert_eq!(app.command_line(), "thepost admin account get acc-0");
        app.on_key(key(KeyCode::End));
        let fx = app.on_key(key(KeyCode::Down));
        let [Effect::Call(id, req)] = fx.as_slice() else {
            panic!("нет запроса следующей страницы: {fx:?}");
        };
        assert_eq!(
            req.query,
            [("after".to_owned(), "user19@it-task.ru".to_owned())]
        );
        app.on_result(
            *id,
            Ok(Page {
                value: json!([{"id": "acc-20", "login": "user20@it-task.ru"}]),
                next: None,
            }),
        );
        let View::List(l) = &main(&app).view else {
            panic!("не список");
        };
        assert_eq!((l.rows.len(), l.sel, l.next.is_none()), (21, 20, true));
        // Tab — дерево; строка CLI — команда раздела.
        app.on_key(key(KeyCode::Tab));
        assert_eq!(main(&app).focus, Panel::Tree);
        assert_eq!(app.command_line(), "thepost admin account list");
    }

    #[test]
    fn actions_on_record() {
        let mut app = with_accounts();
        // F5 — блокировка: форма PATCH с enabled = нет.
        app.on_key(key(KeyCode::F(5)));
        let Some(Overlay::Form(f)) = &main(&app).overlay else {
            panic!("нет формы");
        };
        assert_eq!(f.op.id, "patch_account");
        assert_eq!(f.value("enabled"), Some("нет"));
        assert_eq!(app.command_line(), "thepost admin account set acc-0");
        let fx = app.on_key(key(KeyCode::Enter));
        let [Effect::Call(id, req)] = fx.as_slice() else {
            panic!("нет запроса: {fx:?}");
        };
        assert_eq!(req.body, Some(json!({"enabled": false})));
        // Ответ с телом — карточка результата; Esc — назад к списку и перечитать его.
        let fx = app.on_result(
            *id,
            Ok(Page {
                value: json!({"id": "acc-0", "enabled": false}),
                next: None,
            }),
        );
        assert!(fx.is_empty());
        assert!(matches!(main(&app).view, View::Card(_)));
        let fx = app.on_key(key(KeyCode::Esc));
        assert!(matches!(fx.as_slice(), [Effect::Call(_, r)] if r.op.id == "list_accounts"));
        // F8 — у учётных записей удаления нет.
        app.on_key(key(KeyCode::F(8)));
        assert_eq!(app.status().map(|s| s.0), Some(Level::Warn));
        // F7 — форма создания; отказ — detail у поля.
        app.on_key(key(KeyCode::F(7)));
        // Поля тела по алфавиту: display_name, login.
        app.on_key(key(KeyCode::Down));
        for ch in "x".chars() {
            app.on_key(key(KeyCode::Char(ch)));
        }
        let fx = app.on_key(key(KeyCode::Enter));
        let [Effect::Call(id, _)] = fx.as_slice() else {
            panic!("нет запроса: {fx:?}");
        };
        app.on_result(
            *id,
            Err(tp_admin_client::Error::Api {
                status: 400,
                problem: Some(tp_admin::server::Problem {
                    status: 400,
                    title: "Bad Request".into(),
                    detail: Some("login: домен не принят организацией".into()),
                }),
                message: "POST: 400".into(),
            }),
        );
        let Some(Overlay::Form(f)) = &main(&app).overlay else {
            panic!("форма не вернулась");
        };
        assert_eq!(
            f.fields[f.focus].error.as_deref(),
            Some("login: домен не принят организацией")
        );
    }

    #[test]
    fn menu_lists_every_operation() {
        let mut app = with_accounts();
        app.on_key(key(KeyCode::F(9)));
        app.on_key(key(KeyCode::End));
        let Some(Overlay::Menu { sel }) = main(&app).overlay else {
            panic!("нет меню");
        };
        assert_eq!(sel, catalog().ops.len() - 1);
        app.on_key(key(KeyCode::Esc));
        assert_eq!(main(&app).overlay, None);
    }

    #[test]
    fn trash_asks_for_account() {
        let mut app = with_accounts();
        app.on_key(key(KeyCode::Tab));
        let trash = SECTIONS
            .iter()
            .position(|s| s.label == "Корзина")
            .expect("корзина");
        for _ in 0..trash {
            app.on_key(key(KeyCode::Down));
        }
        let fx = app.on_key(key(KeyCode::Enter));
        assert!(fx.is_empty());
        let Some(Overlay::Form(f)) = &main(&app).overlay else {
            panic!("нет формы");
        };
        assert_eq!(
            f.fields.first().map(|x| x.field.name.as_str()),
            Some("account")
        );
        assert_eq!(app.command_line(), "thepost admin trash list <account>");
    }
}
