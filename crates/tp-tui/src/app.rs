//! Состояние интерфейса и клавиши (TUI-01): вход, две панели (дерево ресурсов и список/карточка), F1–F10.
//! Сети здесь нет: действия возвращают [`Effect`], ответы Admin API приходят в [`App::on_result`] — поэтому экран
//! проверяется тестами без узла.

use std::collections::{BTreeMap, BTreeSet};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Map, Value};
use tp_admin::server::WhoAmI;

use crate::cli;
use crate::engine::{self, Context, Form, Request, ScreenKind};
use crate::layout::{self, Act, Layout, Rows};
use crate::node::{ConfigFile, Param, SaveError, Saved};
use crate::schema::{Kind, Operation, catalog};
use crate::theme::Theme;
use crate::tree::{ORG_ROWS, SECTIONS};

/// Строк списка в правой панели.
pub const LIST_ROWS: usize = 14;
/// Строк карточки, формы и меню в правой панели.
pub const BODY_ROWS: usize = 15;

/// Подписи F-клавиш экранов движка; раскладка раздела задаёт свои.
pub const KEYS: [&str; 10] = [
    "Помощь",
    "Фильтр",
    "Просм",
    "Правка",
    "",
    "",
    "Создать",
    "Удалить",
    "Меню",
    "Выход",
];

/// Подписи F-клавиш конфигурации узла.
pub const CONFIG_KEYS: [&str; 10] = [
    "Помощь",
    "Перечит",
    "",
    "Правка",
    "",
    "",
    "",
    "",
    "Меню",
    "Выход",
];

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
    /// Прочитать `config.yaml` узла; ответ — [`App::on_config`].
    ConfigLoad,
    /// Записать значение параметра конфигурации узла; ответ — [`App::on_config_saved`].
    ConfigSave {
        /// Путь параметра.
        path: Vec<String>,
        /// Значение; `None` — убрать (действует значение по умолчанию).
        value: Option<Value>,
        /// Текст файла на момент чтения.
        loaded: String,
    },
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
    /// Ручная раскладка раздела; нет — колонки движка.
    pub layout: Option<&'static Layout>,
    /// Запись, из которой взяты строки (участники группы, уровень политики); у страницы списка — `null`.
    pub record: Value,
    /// Экран, из которого открыт этот (Esc — назад).
    pub back: Option<Box<ListView>>,
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
    /// Конфигурация узла.
    Config(ConfigView),
}

/// Конфигурация узла: дерево параметров схемы со значениями файла.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigView {
    /// Прочитанный файл.
    pub file: ConfigFile,
    /// Раскрытые секции (ключи через точку).
    pub open: BTreeSet<String>,
    /// Выделенная строка.
    pub sel: usize,
    /// Первая видимая строка.
    pub top: usize,
}

impl ConfigView {
    /// Видимые строки дерева.
    pub fn rows(&self) -> Vec<&Param> {
        self.file.visible(&self.open)
    }

    /// Выделенный параметр.
    pub fn selected(&self) -> Option<&Param> {
        self.rows().get(self.sel).copied()
    }

    fn select(&mut self, sel: usize) {
        let n = self.rows().len();
        self.sel = sel.min(n.saturating_sub(1));
        if self.sel < self.top {
            self.top = self.sel;
        } else if self.sel >= self.top + LIST_ROWS {
            self.top = self.sel + 1 - LIST_ROWS;
        }
    }
}

/// Правка значения параметра конфигурации узла.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigEdit {
    /// Параметр.
    pub param: Param,
    /// Введённое значение; пустое — убрать (действует значение по умолчанию).
    pub value: String,
    /// Ошибка проверки у поля или отказ записи.
    pub error: Option<String>,
    /// Идёт проверка и запись.
    pub busy: bool,
}

impl View {
    /// Раскладка списка на экране или списка, из которого открыта карточка.
    pub fn layout(&self) -> Option<&'static Layout> {
        match self {
            Self::List(l) => l.layout,
            Self::Card(c) => c.back.as_ref().and_then(|b| b.layout),
            Self::Info { .. } | Self::Config(_) => None,
        }
    }
}

/// Что сделать с найденной по имени входа учётной записью.
#[derive(Debug, Clone, PartialEq)]
pub enum Then {
    /// Открыть операцию с параметром пути `account`.
    Open {
        /// Операция.
        op: &'static Operation,
        /// Раскладка.
        layout: Option<&'static Layout>,
        /// Контекст без `account`.
        ctx: Context,
        /// Вложить экран в текущий.
        nest: bool,
    },
    /// Добавить участником группы (`patch_group` с полным списком участников).
    AddMember {
        /// Запрос группы.
        group: Request,
        /// Участники до добавления.
        members: Vec<Value>,
    },
}

impl Then {
    /// Заголовок формы поиска.
    pub fn title(&self) -> String {
        match self {
            Self::Open { op, .. } => format!("{} — учётная запись", op.title()),
            Self::AddMember { .. } => "Новый участник группы".to_owned(),
        }
    }
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
    /// Учётная запись по имени входа (форма поиска `list_accounts`), затем действие.
    Lookup(Box<Form>, Then),
    /// Подтверждение удаления.
    Confirm(Request),
    /// Правка параметра конфигурации узла.
    ConfigEdit(ConfigEdit),
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
    /// Интерфейс запущен на узле с доступом к `config.yaml`.
    pub node: bool,
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

/// Как ответ списка ложится на экран.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Nav {
    /// Новый экран вместо текущего.
    Replace,
    /// Следующая страница текущего списка.
    Append,
    /// Новый экран поверх текущего (Esc — назад).
    Nest,
    /// Перечитать текущий список (раскладка, выделение и путь назад сохраняются).
    Reload,
}

#[derive(Debug, Clone, PartialEq)]
enum Pending {
    Orgs,
    Load {
        req: Request,
        form: Option<Box<Form>>,
        nav: Nav,
        layout: Option<&'static Layout>,
    },
    Submit {
        req: Request,
        form: Option<Box<Form>>,
    },
    Lookup {
        form: Box<Form>,
        then: Then,
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

    fn load(
        &mut self,
        req: Request,
        form: Option<Box<Form>>,
        nav: Nav,
        layout: Option<&'static Layout>,
    ) -> Effect {
        self.call(
            req.clone(),
            Pending::Load {
                req,
                form,
                nav,
                layout,
            },
        )
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
    /// Интерфейс запущен на узле с доступом к `config.yaml` (раздел «Конфигурация узла» правит файл).
    pub node: bool,
    bus: Bus,
}

impl App {
    /// Интерфейс на экране входа.
    pub fn new(theme: Theme, login: LoginForm) -> Self {
        Self {
            theme,
            stage: Stage::Login(login),
            node: false,
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

    /// Подписи F-клавиш: экраны движка — [`KEYS`]; раскладка раздела — свои; у строк записи (участники,
    /// политика) — только свои, Помощь, Меню и Выход.
    pub fn keys(&self) -> [&'static str; 10] {
        let Stage::Main(m) = &self.stage else {
            return KEYS;
        };
        if matches!(m.view, View::Config(_)) && m.overlay.is_none() {
            return CONFIG_KEYS;
        }
        let Some(l) = m.view.layout().filter(|_| m.overlay.is_none()) else {
            return KEYS;
        };
        let mut keys = KEYS;
        if l.rows != Rows::Page {
            for (i, k) in keys.iter_mut().enumerate() {
                if ![0, 8, 9].contains(&i) {
                    *k = "";
                }
            }
        }
        for k in l.keys {
            keys[usize::from(k.n - 1)] = k.label;
        }
        keys
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
            node: self.node,
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

    /// Прочитанный `config.yaml` узла (или ошибка чтения).
    pub fn on_config(&mut self, result: Result<ConfigFile, String>) {
        let Stage::Main(m) = &mut self.stage else {
            return;
        };
        match result {
            Ok(file) => {
                let (open, sel) = match &m.view {
                    View::Config(c) => (c.open.clone(), c.sel),
                    _ => (BTreeSet::new(), 0),
                };
                let mut v = ConfigView {
                    file,
                    open,
                    sel: 0,
                    top: 0,
                };
                v.select(sel);
                if let Some(reason) = v.file.access.reason() {
                    self.bus
                        .say(Level::Warn, format!("только чтение: {reason}"));
                }
                m.view = View::Config(v);
            }
            Err(e) => {
                m.view = View::Info {
                    title: crate::tree::NODE_CONFIG.to_owned(),
                    lines: vec![e.clone()],
                };
                self.bus.say(Level::Err, e);
            }
        }
    }

    /// Итог записи параметра конфигурации узла: ошибка — у поля правки; записано — файл перечитывается.
    pub fn on_config_saved(&mut self, result: Result<Saved, SaveError>) -> Vec<Effect> {
        let Stage::Main(m) = &mut self.stage else {
            return Vec::new();
        };
        match result {
            Ok(saved) => {
                m.overlay = None;
                self.bus.say(
                    Level::Ok,
                    format!(
                        "записано, копия — {}; узел перечитает файл без перезапуска",
                        saved.backup.display()
                    ),
                );
                vec![Effect::ConfigLoad]
            }
            Err(e) => {
                if let Some(Overlay::ConfigEdit(edit)) = &mut m.overlay {
                    edit.busy = false;
                    edit.error = Some(e.message.clone());
                }
                let what = if e.at_field {
                    "не записано: ошибка проверки"
                } else {
                    "не записано"
                };
                self.bus.say(Level::Err, format!("{what}: {}", e.message));
                Vec::new()
            }
        }
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
                if let tp_admin_client::Error::Api {
                    problem: Some(pr), ..
                } = &e
                {
                    match p {
                        Pending::Load {
                            form: Some(mut f), ..
                        }
                        | Pending::Submit {
                            form: Some(mut f), ..
                        } => {
                            f.apply_problem(pr);
                            m.overlay = Some(Overlay::Form(f));
                        }
                        Pending::Lookup { mut form, then } => {
                            form.apply_problem(pr);
                            m.overlay = Some(Overlay::Lookup(form, then));
                        }
                        _ => {}
                    }
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
            Pending::Load {
                req, nav, layout, ..
            } => {
                loaded(m, req, page, nav, layout);
                Vec::new()
            }
            Pending::Lookup { form, then } => found(m, &mut self.bus, form, then, &page.value),
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
                        vec![self.bus.load(r, None, Nav::Reload, None)]
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
        Some(Overlay::Form(f) | Overlay::Lookup(f, _)) => return show(&f.preview()),
        Some(Overlay::Confirm(req)) => return show(req),
        Some(Overlay::Menu { sel }) => {
            return catalog()
                .ops
                .get(*sel)
                .map(|op| show(&Form::new(op, &m.ctx(), false, &Map::new()).preview()))
                .unwrap_or_default();
        }
        Some(Overlay::ConfigEdit(_)) => return "thepost config check".to_owned(),
        Some(Overlay::Help) | None => {}
    }
    if m.focus == Panel::Tree {
        return match m.tree_rows().get(m.tree) {
            Some(TreeRow::Section(i)) => catalog()
                .get(SECTIONS[*i].op)
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
                (Some(view), Some(param), Some(row)) if l.record.is_null() => {
                    item_ctx(&l.req, param, row)
                        .and_then(|ctx| Request::new(view, &ctx, Vec::new()).ok())
                        .map_or_else(|| show(&l.req), |r| show(&r))
                }
                _ => show(&l.req),
            }
        }
        View::Card(c) => show(&c.req),
        View::Config(_) => "thepost config check".to_owned(),
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

fn loaded(m: &mut Main, req: Request, page: Page, nav: Nav, layout: Option<&'static Layout>) {
    let current = match (&m.view, nav) {
        (View::List(l), Nav::Reload | Nav::Append) if l.req.op.id == req.op.id => l.layout,
        _ => None,
    };
    let layout = layout.or(current).or_else(|| layout::of(&req.op.id));
    let rows_of_record = layout.is_some_and(|l| l.rows != Rows::Page);
    if !req.op.is_list() && !rows_of_record {
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
    let (rows, cursor) = match layout {
        Some(l) => l.rows(req.op, &page.value),
        None => engine::page(req.op, &page.value),
    };
    let next = page.next.or_else(|| {
        cursor.map(|(k, v)| {
            let mut q: Vec<(String, String)> =
                req.query.iter().filter(|(n, _)| *n != k).cloned().collect();
            q.push((k, v));
            q
        })
    });
    let record = if rows_of_record {
        page.value
    } else {
        Value::Null
    };
    if let View::List(l) = &mut m.view
        && l.req.op.id == req.op.id
    {
        match nav {
            Nav::Append => {
                l.rows.extend(rows);
                l.next = next;
                let sel = l.sel + 1;
                l.select(sel);
                return;
            }
            Nav::Reload => {
                let sel = if l.req == req { l.sel } else { 0 };
                l.req = req;
                l.rows = rows;
                l.next = next;
                l.record = record;
                l.top = 0;
                l.select(sel);
                return;
            }
            Nav::Replace | Nav::Nest => {}
        }
    }
    let back = match nav {
        Nav::Nest => take_list(&mut m.view),
        _ => None,
    };
    m.view = View::List(ListView {
        req,
        rows,
        sel: 0,
        top: 0,
        next,
        layout,
        record,
        back,
    });
}

/// Учётная запись по имени входа найдена (или нет): действие `then`.
fn found(
    m: &mut Main,
    bus: &mut Bus,
    mut form: Box<Form>,
    then: Then,
    rows: &Value,
) -> Vec<Effect> {
    let id = rows
        .as_array()
        .and_then(|r| r.first())
        .and_then(|r| r.get("id"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let Some(id) = id else {
        if let Some(f) = form.fields.first_mut() {
            f.error = Some("нет учётной записи с этим именем входа".to_owned());
        }
        m.overlay = Some(Overlay::Lookup(form, then));
        return Vec::new();
    };
    match then {
        Then::Open {
            op,
            layout,
            mut ctx,
            nest,
        } => {
            ctx.insert("account".to_owned(), id);
            let nav = if nest { Nav::Nest } else { Nav::Replace };
            open_op(m, bus, op, &ctx, &Map::new(), nav, layout)
        }
        Then::AddMember { group, mut members } => {
            members.push(serde_json::json!({ "account": id }));
            match patch_members(&group, members) {
                Some(req) => vec![bus.submit(req, None)],
                None => Vec::new(),
            }
        }
    }
}

/// Запрос `patch_group` с полным списком участников группы `group`.
fn patch_members(group: &Request, members: Vec<Value>) -> Option<Request> {
    let op = catalog().get("patch_group")?;
    let mut req = Request::new(op, &req_ctx(group), Vec::new()).ok()?;
    req.body = Some(serde_json::json!({ "members": members }));
    Some(req)
}

/// Форма поиска учётной записи по имени входа в организации контекста.
fn lookup(ctx: &Context, then: Then) -> Option<Overlay> {
    let op = catalog().get("list_accounts")?;
    let mut org = ctx.clone();
    org.retain(|k, _| k == "org");
    let mut form = Form::new(op, &org, true, &Map::new());
    form.retain(&["login"]);
    if let Some(f) = form.fields.first_mut() {
        f.error = None;
    }
    Some(Overlay::Lookup(Box::new(form), then))
}

/// Открыть экран операции `op` в контексте `ctx`: список и карточка загружаются, если известны все параметры
/// пути и нет обязательных параметров запроса, иначе — форма; удаление — подтверждение. Нет только учётной
/// записи — она ищется по имени входа.
fn open_op(
    m: &mut Main,
    bus: &mut Bus,
    op: &'static Operation,
    ctx: &Context,
    prefill: &Map<String, Value>,
    nav: Nav,
    layout: Option<&'static Layout>,
) -> Vec<Effect> {
    let missing: Vec<&str> = op
        .path_names()
        .into_iter()
        .filter(|n| !ctx.contains_key(*n))
        .collect();
    if missing == ["account"] {
        let then = Then::Open {
            op,
            layout,
            ctx: ctx.clone(),
            nest: nav == Nav::Nest,
        };
        m.overlay = lookup(ctx, then);
        return Vec::new();
    }
    let form = || Overlay::Form(Box::new(Form::new(op, ctx, false, prefill)));
    let ready = Request::new(op, ctx, Vec::new()).ok();
    match (engine::screen_kind(op), ready) {
        (ScreenKind::List | ScreenKind::Card, Some(req)) if !op.query().any(|f| f.required) => {
            return vec![bus.load(req, None, nav, layout)];
        }
        (ScreenKind::Confirm, Some(req)) if !op.query().any(|f| f.required) => {
            m.overlay = Some(Overlay::Confirm(req));
        }
        _ => m.overlay = Some(form()),
    }
    Vec::new()
}

fn open_tree(m: &mut Main, bus: &mut Bus) -> Vec<Effect> {
    match m.tree_rows().get(m.tree).copied() {
        Some(TreeRow::Section(i)) => {
            let s = SECTIONS[i];
            m.section = s.label.to_owned();
            m.focus = Panel::Main;
            match catalog().get(s.op) {
                Some(op) => {
                    let ctx = m.ctx();
                    open_op(m, bus, op, &ctx, &Map::new(), Nav::Replace, None)
                }
                None => Vec::new(),
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
        Some(TreeRow::Config) if m.node => {
            m.section = crate::tree::NODE_CONFIG.to_owned();
            m.focus = Panel::Main;
            if !matches!(m.view, View::Config(_)) {
                m.view = View::Info {
                    title: crate::tree::NODE_CONFIG.to_owned(),
                    lines: vec!["чтение config.yaml…".to_owned()],
                };
            }
            vec![Effect::ConfigLoad]
        }
        Some(TreeRow::Config) => {
            m.section = crate::tree::NODE_CONFIG.to_owned();
            m.view = View::Info {
                title: crate::tree::NODE_CONFIG.to_owned(),
                lines: [
                    "Правка config.yaml — на самом узле, когда",
                    "интерфейс запущен там с правом на файл.",
                    "Проверка: thepost config check.",
                ]
                .map(str::to_owned)
                .to_vec(),
            };
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
                        return vec![bus.load(req, None, Nav::Append, None)];
                    }
                }
                KeyCode::Down => l.select(l.sel + 1),
                KeyCode::PageDown => l.select(l.sel + LIST_ROWS),
                KeyCode::End => l.select(last),
                KeyCode::Esc | KeyCode::Backspace | KeyCode::Left => {
                    if let Some(back) = l.back.take() {
                        let r = back.req.clone();
                        m.view = View::List(*back);
                        return vec![bus.load(r, None, Nav::Reload, None)];
                    }
                }
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
                            return vec![bus.load(r, None, Nav::Reload, None)];
                        }
                    }
                }
                _ => return list_action(m, bus, key),
            }
            Vec::new()
        }
        View::Config(c) => config_key(c, bus, &mut m.overlay, key),
        View::Info { .. } => Vec::new(),
    }
}

/// Клавиши дерева конфигурации узла: ←→ и Enter — секции, Enter и F4 — правка параметра, F2 — перечитать.
fn config_key(
    c: &mut ConfigView,
    bus: &mut Bus,
    overlay: &mut Option<Overlay>,
    key: KeyEvent,
) -> Vec<Effect> {
    let n = c.rows().len();
    let sel = c.selected().cloned();
    match key.code {
        KeyCode::Up => c.select(c.sel.saturating_sub(1)),
        KeyCode::Down => c.select(c.sel + 1),
        KeyCode::PageUp => c.select(c.sel.saturating_sub(LIST_ROWS)),
        KeyCode::PageDown => c.select(c.sel + LIST_ROWS),
        KeyCode::Home => c.select(0),
        KeyCode::End => c.select(n.saturating_sub(1)),
        KeyCode::F(2) => return vec![Effect::ConfigLoad],
        KeyCode::Right | KeyCode::Enter if sel.as_ref().is_some_and(|p| p.section) => {
            let k = sel.map(|p| p.key()).unwrap_or_default();
            if key.code == KeyCode::Enter && c.open.contains(&k) {
                c.open.remove(&k);
            } else {
                c.open.insert(k);
            }
        }
        KeyCode::Left => {
            let Some(p) = sel else {
                return Vec::new();
            };
            if p.section && c.open.remove(&p.key()) {
                return Vec::new();
            }
            if p.path.len() > 1 {
                let parent = p.path[..p.path.len() - 1].join(".");
                if let Some(i) = c.rows().iter().position(|r| r.key() == parent) {
                    c.select(i);
                }
            }
        }
        KeyCode::Enter | KeyCode::F(4) => {
            let Some(p) = sel.filter(|p| !p.section) else {
                return Vec::new();
            };
            if let Some(reason) = c.file.access.reason() {
                bus.say(Level::Warn, format!("только чтение: {reason}"));
                return Vec::new();
            }
            let value = c
                .file
                .value(&p)
                .map(|v| engine::text(v, p.field.kind))
                .unwrap_or_default();
            *overlay = Some(Overlay::ConfigEdit(ConfigEdit {
                param: p,
                value,
                error: None,
                busy: false,
            }));
        }
        _ => {}
    }
    Vec::new()
}

/// Клавиши правки параметра конфигурации: Enter — проверить и записать (пусто — значение по умолчанию).
fn config_edit_key(m: &mut Main, mut e: ConfigEdit, key: KeyEvent) -> Vec<Effect> {
    if e.busy {
        m.overlay = Some(Overlay::ConfigEdit(e));
        return Vec::new();
    }
    let f = &e.param.field;
    match key.code {
        KeyCode::Esc => return Vec::new(),
        KeyCode::Enter => {
            let raw = e.value.trim();
            let value = if raw.is_empty() {
                Ok(None)
            } else {
                engine::parse(f, raw).map(Some)
            };
            let View::Config(c) = &m.view else {
                return Vec::new();
            };
            match value {
                Ok(value) => {
                    let save = Effect::ConfigSave {
                        path: e.param.path.clone(),
                        value,
                        loaded: c.file.text.clone(),
                    };
                    e.busy = true;
                    e.error = None;
                    m.overlay = Some(Overlay::ConfigEdit(e));
                    return vec![save];
                }
                Err(err) => e.error = Some(err),
            }
        }
        KeyCode::Left | KeyCode::Right => {
            let mut choices = f.choices.clone();
            if f.kind == Kind::Boolean {
                choices = vec!["да".to_owned(), "нет".to_owned()];
            }
            if !choices.is_empty() {
                choices.insert(0, String::new());
                let k = choices.len();
                let j = match (choices.iter().position(|c| *c == e.value), key.code) {
                    (None, _) => 0,
                    (Some(i), KeyCode::Left) => (i + k - 1) % k,
                    (Some(i), _) => (i + 1) % k,
                };
                e.value = choices[j].clone();
            }
        }
        KeyCode::Backspace => {
            e.value.pop();
        }
        KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => e.value.push(ch),
        _ => {}
    }
    m.overlay = Some(Overlay::ConfigEdit(e));
    Vec::new()
}

/// Список (или карточка записи из него), над которым выполняется действие.
struct Target {
    list: Request,
    row: Option<Value>,
    item: Option<Context>,
    record: Value,
}

/// Действия F2–F8 над списком или карточкой записи: сначала клавиши раскладки раздела, затем — движка.
fn list_action(m: &mut Main, bus: &mut Bus, key: KeyEvent) -> Vec<Effect> {
    let c = catalog();
    let t = match &m.view {
        View::List(l) => {
            let ops = engine::item_ops(c, l.req.op);
            let item = ops
                .param
                .zip(l.row())
                .and_then(|(p, row)| item_ctx(&l.req, p, row));
            Target {
                list: l.req.clone(),
                row: l.row().cloned(),
                item,
                record: l.record.clone(),
            }
        }
        View::Card(card) => match &card.back {
            Some(back) => Target {
                list: back.req.clone(),
                row: Some(card.value.clone()),
                item: Some(req_ctx(&card.req)),
                record: back.record.clone(),
            },
            None => return Vec::new(),
        },
        View::Info { .. } | View::Config(_) => return Vec::new(),
    };
    let n = match key.code {
        KeyCode::Enter => 3,
        KeyCode::F(n) => n,
        _ => return Vec::new(),
    };
    if let Some(l) = m.view.layout() {
        if let Some(act) = l.key(n) {
            return run(m, bus, act, &t);
        }
        if l.rows != Rows::Page {
            return Vec::new();
        }
    }
    let ops = engine::item_ops(c, t.list.op);
    let row_map = t
        .row
        .as_ref()
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let need = |bus: &mut Bus, what: &str| {
        bus.say(Level::Warn, format!("{what}: нет в разделе"));
        Vec::new()
    };
    match n {
        2 => {
            let mut prefill = Map::new();
            for (k, v) in req_ctx(&t.list) {
                prefill.insert(k, Value::String(v));
            }
            for (k, v) in &t.list.query {
                prefill.insert(k.clone(), Value::String(v.clone()));
            }
            let mut ctx = m.ctx();
            ctx.retain(|k, _| t.list.op.path_names().contains(&k.as_str()));
            m.overlay = Some(Overlay::Form(Box::new(Form::new(
                t.list.op, &ctx, false, &prefill,
            ))));
            Vec::new()
        }
        3 => match (ops.view, &t.item) {
            (Some(view), Some(ctx)) => open_op(m, bus, view, ctx, &Map::new(), Nav::Replace, None),
            (None, _) if matches!(m.view, View::List(_)) && t.row.is_some() => {
                let back = take_list(&mut m.view);
                m.view = View::Card(CardView {
                    title: t.list.op.title(),
                    req: t.list,
                    value: t.row.unwrap_or_default(),
                    top: 0,
                    back,
                    reload: false,
                });
                Vec::new()
            }
            _ => Vec::new(),
        },
        4 => match (ops.edit, &t.item) {
            (Some(op), Some(ctx)) => open_op(m, bus, op, ctx, &row_map, Nav::Replace, None),
            _ => need(bus, "правка"),
        },
        7 => match ops.create {
            Some(op) => {
                let ctx = req_ctx(&t.list);
                open_op(m, bus, op, &ctx, &Map::new(), Nav::Replace, None)
            }
            None => need(bus, "создание"),
        },
        8 => match (ops.delete, &t.item) {
            (Some(op), Some(ctx)) => open_op(m, bus, op, ctx, &Map::new(), Nav::Replace, None),
            _ => need(bus, "удаление"),
        },
        _ => Vec::new(),
    }
}

/// Действие раскладки раздела над целью `t`.
fn run(m: &mut Main, bus: &mut Bus, act: Act, t: &Target) -> Vec<Effect> {
    let c = catalog();
    let Some(op) = (match act {
        Act::Item { op, .. }
        | Act::Open { op, .. }
        | Act::Switch { op, .. }
        | Act::Account { op, .. }
        | Act::EditPolicy(op) => c.get(op),
        Act::AddMember | Act::RemoveMember => c.get("patch_group"),
    }) else {
        return Vec::new();
    };
    let no_row = |bus: &mut Bus| {
        bus.say(Level::Warn, "нет выделенной записи");
        Vec::new()
    };
    match act {
        Act::Item { prefill, .. } => match (&t.item, &t.row) {
            (Some(ctx), Some(row)) => open_op(m, bus, op, ctx, &prefill(row), Nav::Replace, None),
            _ => no_row(bus),
        },
        Act::Open { layout, .. } => match &t.item {
            Some(ctx) => open_op(m, bus, op, ctx, &Map::new(), Nav::Nest, Some(layout)),
            None => no_row(bus),
        },
        Act::Switch { layout, nest, .. } => {
            let mut ctx = req_ctx(&t.list);
            ctx.retain(|k, _| op.path_names().contains(&k.as_str()));
            let nav = if nest { Nav::Nest } else { Nav::Replace };
            open_op(m, bus, op, &ctx, &Map::new(), nav, Some(layout))
        }
        Act::Account { layout, .. } => {
            let mut ctx = req_ctx(&t.list);
            ctx.retain(|k, _| k == "org");
            open_op(m, bus, op, &ctx, &Map::new(), Nav::Nest, Some(layout))
        }
        Act::EditPolicy(_) => {
            let prefill = t
                .record
                .get("policy")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            open_op(m, bus, op, &req_ctx(&t.list), &prefill, Nav::Replace, None)
        }
        Act::AddMember => {
            let members = t
                .record
                .get("members")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let then = Then::AddMember {
                group: t.list.clone(),
                members,
            };
            m.overlay = lookup(&req_ctx(&t.list), then);
            Vec::new()
        }
        Act::RemoveMember => {
            let Some(id) = t
                .row
                .as_ref()
                .and_then(|r| r.get("id"))
                .and_then(Value::as_str)
            else {
                return no_row(bus);
            };
            m.overlay = patch_members(&t.list, layout::members_without(&t.record, id))
                .map(Overlay::Confirm);
            Vec::new()
        }
    }
}

/// Итог клавиши в форме.
enum FormKey {
    Stay,
    Cancel,
    Submit,
}

/// Правка полей формы: ↑↓ — поле, ←→ — выбор из вариантов, текст.
fn form_key(f: &mut Form, key: KeyEvent) -> FormKey {
    let n = f.fields.len();
    match key.code {
        KeyCode::Esc => return FormKey::Cancel,
        KeyCode::Enter => return FormKey::Submit,
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
        _ => {}
    }
    FormKey::Stay
}

fn overlay_key(m: &mut Main, bus: &mut Bus, ov: Overlay, key: KeyEvent) -> Vec<Effect> {
    match ov {
        Overlay::Help => Vec::new(),
        Overlay::ConfigEdit(e) => config_edit_key(m, e, key),
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
                    return open_op(m, bus, op, &ctx, &Map::new(), Nav::Replace, None);
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
        Overlay::Form(mut f) => match form_key(&mut f, key) {
            FormKey::Cancel => Vec::new(),
            FormKey::Submit => submit(m, bus, f),
            FormKey::Stay => {
                m.overlay = Some(Overlay::Form(f));
                Vec::new()
            }
        },
        Overlay::Lookup(mut f, then) => match form_key(&mut f, key) {
            FormKey::Cancel => Vec::new(),
            FormKey::Submit => match f.request() {
                Some(req) => vec![bus.call(req, Pending::Lookup { form: f, then })],
                None => {
                    m.overlay = Some(Overlay::Lookup(f, then));
                    Vec::new()
                }
            },
            FormKey::Stay => {
                m.overlay = Some(Overlay::Lookup(f, then));
                Vec::new()
            }
        },
    }
}

fn submit(m: &mut Main, bus: &mut Bus, mut f: Box<Form>) -> Vec<Effect> {
    let Some(req) = f.request() else {
        m.overlay = Some(Overlay::Form(f));
        return Vec::new();
    };
    match engine::screen_kind(f.op) {
        ScreenKind::List | ScreenKind::Card => {
            // Фильтр текущего списка: раскладка и путь назад сохраняются.
            let same = matches!(&m.view, View::List(l) if l.req.op.id == f.op.id);
            let nav = if same { Nav::Reload } else { Nav::Replace };
            vec![bus.load(req, Some(f), nav, None)]
        }
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
    use crate::theme::{Brand, Mode};
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
            Theme::new(Mode::Dark, Brand::default()),
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
            Theme::new(Mode::Mono, Brand::default()),
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
        // F8 — скрыть из книги: форма PATCH с hidden_from_gal = да.
        app.on_key(key(KeyCode::F(8)));
        let Some(Overlay::Form(f)) = &main(&app).overlay else {
            panic!("нет формы");
        };
        assert_eq!(f.value("hidden_from_gal"), Some("да"));
        app.on_key(key(KeyCode::Esc));
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
        // Учётная запись — по имени входа, а не по UUID.
        let Some(Overlay::Lookup(f, _)) = &main(&app).overlay else {
            panic!("нет формы поиска");
        };
        let names: Vec<&str> = f.fields.iter().map(|x| x.field.name.as_str()).collect();
        assert_eq!(names, ["login"]);
        for c in "nobody@it-task.ru".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(
            app.command_line(),
            "thepost admin account list --login nobody@it-task.ru"
        );
        let fx = app.on_key(key(KeyCode::Enter));
        let id = call(&fx, "list_accounts");
        // Не найдена — ошибка у поля, форма остаётся.
        app.on_result(id, ok(json!([])));
        let Some(Overlay::Lookup(f, _)) = &main(&app).overlay else {
            panic!("форма поиска не вернулась");
        };
        assert_eq!(
            f.fields[0].error.as_deref(),
            Some("нет учётной записи с этим именем входа")
        );
        let fx = app.on_key(key(KeyCode::Enter));
        let id = call(&fx, "list_accounts");
        let fx = app.on_result(
            id,
            ok(json!([{"id": "acc-7", "login": "nobody@it-task.ru"}])),
        );
        let [Effect::Call(id, req)] = fx.as_slice() else {
            panic!("нет запроса корзины: {fx:?}");
        };
        assert_eq!(req.op.id, "list_trash");
        assert_eq!(req.param("account"), Some("acc-7"));
        app.on_result(
            *id,
            ok(
                json!({"items": [{"message": "m1", "origin": "INBOX", "size": 2048,
                "deleted_at": "2026-11-14T09:00:00Z", "expires_at": "2026-12-14T09:00:00Z"}],
                "next_offset": null}),
            ),
        );
        let View::List(l) = &main(&app).view else {
            panic!("не список");
        };
        assert_eq!(l.layout, Some(&layout::TRASH));
        assert_eq!(app.keys()[4], "Восст");
        // F5 — восстановить письмо: POST без тела.
        app.on_key(key(KeyCode::F(5)));
        let Some(Overlay::Form(f)) = &main(&app).overlay else {
            panic!("нет формы восстановления");
        };
        assert_eq!(f.op.id, "restore");
        assert_eq!(app.command_line(), "thepost admin trash restore acc-7 m1");
    }

    pub(crate) fn ok(value: Value) -> Result<Page, tp_admin_client::Error> {
        Ok(Page { value, next: None })
    }

    /// Номер единственного запроса операции `op` среди действий.
    pub(crate) fn call(fx: &[Effect], op: &str) -> u64 {
        match fx {
            [Effect::Call(id, req)] if req.op.id == op => *id,
            other => panic!("нет запроса {op}: {other:?}"),
        }
    }

    /// Открыть раздел дерева по подписи.
    pub(crate) fn section(app: &mut App, label: &str) -> Vec<Effect> {
        if main(app).focus == Panel::Main {
            app.on_key(key(KeyCode::Tab));
        }
        app.on_key(key(KeyCode::Home));
        let at = SECTIONS
            .iter()
            .position(|s| s.label == label)
            .expect("раздел");
        for _ in 0..at {
            app.on_key(key(KeyCode::Down));
        }
        app.on_key(key(KeyCode::Enter))
    }

    /// Группы: Enter — участники; F7 — добавить по имени входа; F8 — убрать с подтверждением.
    pub(crate) fn with_members() -> App {
        let mut app = with_accounts();
        let id = call(&section(&mut app, "Группы"), "list_groups");
        app.on_result(
            id,
            ok(
                json!([{"id": "g1", "name": "Продажи", "address": "sales@it-task.ru",
                       "members": [{"account": "acc-1"}], "moderators": []}]),
            ),
        );
        assert_eq!(app.keys()[2], "Состав");
        let id = call(&app.on_key(key(KeyCode::Enter)), "get_group");
        app.on_result(
            id,
            ok(json!({"id": "g1", "name": "Продажи",
                      "members": [{"account": "acc-1"}, {"group": "g2"}],
                      "names": {"acc-1": "user1@it-task.ru", "g2": "Инженеры"}})),
        );
        app
    }

    #[test]
    fn group_members() {
        let mut app = with_members();
        let View::List(l) = &main(&app).view else {
            panic!("не список");
        };
        assert_eq!(l.layout, Some(&layout::MEMBERS));
        assert_eq!(l.rows.len(), 2);
        assert_eq!(l.rows[1]["name"], "Инженеры");
        assert_eq!(app.keys()[1], "");
        assert_eq!(app.keys()[6], "Добав");
        // F7 — новый участник по имени входа.
        app.on_key(key(KeyCode::F(7)));
        for c in "user5@it-task.ru".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        let id = call(&app.on_key(key(KeyCode::Enter)), "list_accounts");
        let fx = app.on_result(
            id,
            ok(json!([{"id": "acc-5", "login": "user5@it-task.ru"}])),
        );
        let [Effect::Call(id, req)] = fx.as_slice() else {
            panic!("нет правки группы: {fx:?}");
        };
        assert_eq!(req.op.id, "patch_group");
        assert_eq!(
            req.body,
            Some(json!({"members": [{"account": "acc-1"}, {"group": "g2"}, {"account": "acc-5"}]}))
        );
        app.on_result(*id, ok(json!({"id": "g1"})));
        // Esc из результата — участники перечитываются, путь назад к группам сохраняется.
        let fx = app.on_key(key(KeyCode::Esc));
        let id = call(&fx, "get_group");
        app.on_result(
            id,
            ok(json!({"id": "g1", "members": [{"group": "g2"}], "names": {"g2": "Инженеры"}})),
        );
        // F8 — убрать выделенного: подтверждение с остальными участниками.
        app.on_key(key(KeyCode::F(8)));
        let Some(Overlay::Confirm(req)) = &main(&app).overlay else {
            panic!("нет подтверждения");
        };
        assert_eq!(req.body, Some(json!({"members": []})));
        app.on_key(key(KeyCode::Esc));
        // Esc — назад к списку групп.
        let id = call(&app.on_key(key(KeyCode::Esc)), "list_groups");
        app.on_result(id, ok(json!([])));
        let View::List(l) = &main(&app).view else {
            panic!("не список");
        };
        assert_eq!(l.layout, Some(&layout::GROUPS));
    }

    #[test]
    fn policy_levels() {
        let mut app = with_accounts();
        let id = call(&section(&mut app, "Политики"), "get_org");
        app.on_result(
            id,
            ok(json!({"id": "o1", "default": true,
                      "policy": {"send_per_hour": 200},
                      "effective": {"send_per_hour": 200, "quota_bytes": 1_073_741_824u64}})),
        );
        let View::List(l) = &main(&app).view else {
            panic!("не список");
        };
        assert_eq!(l.layout, Some(&layout::POLICY_ORG));
        assert!(
            l.rows
                .iter()
                .any(|r| r["name"] == "send_per_hour" && r["policy"] == 200)
        );
        // F4 — правка уровня: PUT целиком, значения — заданные на уровне.
        app.on_key(key(KeyCode::F(4)));
        let Some(Overlay::Form(f)) = &main(&app).overlay else {
            panic!("нет формы");
        };
        assert_eq!(f.op.id, "put_org_policy");
        assert_eq!(f.value("send_per_hour"), Some("200"));
        app.on_key(key(KeyCode::Esc));
        // F5 — домены, Enter — политика домена, Esc — назад.
        let id = call(&app.on_key(key(KeyCode::F(5))), "list_domains");
        app.on_result(
            id,
            ok(json!({"domains": [{"id": "d1", "name": "it-task.ru"}]})),
        );
        let id = call(&app.on_key(key(KeyCode::Enter)), "get_domain_policy");
        app.on_result(id, ok(json!({"policy": {}, "effective": {}})));
        assert_eq!(main(&app).view.layout(), Some(&layout::POLICY_DOMAIN));
        app.on_key(key(KeyCode::F(4)));
        let Some(Overlay::Form(f)) = &main(&app).overlay else {
            panic!("нет формы");
        };
        assert_eq!(f.op.id, "put_domain_policy");
        app.on_key(key(KeyCode::Esc));
        let id = call(&app.on_key(key(KeyCode::Esc)), "list_domains");
        app.on_result(
            id,
            ok(json!({"domains": [{"id": "d1", "name": "it-task.ru"}]})),
        );
        let id = call(&app.on_key(key(KeyCode::Esc)), "get_org");
        app.on_result(id, ok(json!({"policy": {}, "effective": {}})));
        // F6 — ящик по имени входа.
        app.on_key(key(KeyCode::F(6)));
        for c in "user1@it-task.ru".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        let id = call(&app.on_key(key(KeyCode::Enter)), "list_accounts");
        let id = call(
            &app.on_result(id, ok(json!([{"id": "acc-1"}]))),
            "get_account",
        );
        app.on_result(
            id,
            ok(json!({"id": "acc-1", "policy": {"quota_bytes": 10}, "effective": {}})),
        );
        assert_eq!(main(&app).view.layout(), Some(&layout::POLICY_ACCOUNT));
        app.on_key(key(KeyCode::F(4)));
        let Some(Overlay::Form(f)) = &main(&app).overlay else {
            panic!("нет формы");
        };
        assert_eq!(
            (f.op.id.as_str(), f.value("quota_bytes")),
            ("put_account_policy", Some("10"))
        );
    }

    #[test]
    fn journals_switch_and_filter() {
        let mut app = with_accounts();
        let id = call(&section(&mut app, "Журналы"), "search_audit");
        app.on_result(
            id,
            ok(json!({"entries": [{"seq": 1, "at": "2026-11-14T09:00:00Z", "actor_login": "admin@it-task.ru",
                                   "method": "PATCH", "status": 200, "route": "/api/v1/orgs/{org}/accounts/{account}"}],
                      "next": 1})),
        );
        // Листание по курсору ответа.
        let fx = app.on_key(key(KeyCode::Down));
        let [Effect::Call(id, req)] = fx.as_slice() else {
            panic!("нет следующей страницы: {fx:?}");
        };
        assert_eq!(req.query, [("after".to_owned(), "1".to_owned())]);
        app.on_result(*id, ok(json!({"entries": [], "next": null})));
        // F2 — фильтр; раскладка остаётся.
        app.on_key(key(KeyCode::F(2)));
        let Some(Overlay::Form(f)) = &main(&app).overlay else {
            panic!("нет фильтра");
        };
        assert!(f.fields.iter().any(|x| x.field.name == "actor"));
        let fx = app.on_key(key(KeyCode::Enter));
        let id = call(&fx, "search_audit");
        app.on_result(id, ok(json!({"entries": [], "next": null})));
        assert_eq!(main(&app).view.layout(), Some(&layout::AUDIT));
        // F5 — журнал доставки и обратно.
        let id = call(&app.on_key(key(KeyCode::F(5))), "search_delivery");
        app.on_result(id, ok(json!({"events": [], "next": null})));
        assert_eq!(main(&app).view.layout(), Some(&layout::DELIVERY));
        assert_eq!(app.keys()[4], "Аудит");
        call(&app.on_key(key(KeyCode::F(5))), "search_audit");
    }

    #[test]
    fn moderation_actions() {
        let mut app = with_accounts();
        let id = call(&section(&mut app, "Модерация"), "list_held");
        app.on_result(
            id,
            ok(json!([{"id": "h1", "sender": "x@ext.test", "held_at": "2026-11-14T09:00:00Z", "size": 1000}])),
        );
        app.on_key(key(KeyCode::F(5)));
        let Some(Overlay::Form(f)) = &main(&app).overlay else {
            panic!("нет формы");
        };
        assert_eq!(f.op.id, "approve");
        let fx = app.on_key(key(KeyCode::Enter));
        let [Effect::Call(_, req)] = fx.as_slice() else {
            panic!("нет запроса: {fx:?}");
        };
        assert_eq!(
            (req.op.id.as_str(), req.param("held")),
            ("approve", Some("h1"))
        );
        app.on_key(key(KeyCode::F(8)));
        let Some(Overlay::Form(f)) = &main(&app).overlay else {
            panic!("нет формы");
        };
        assert_eq!(f.op.id, "reject");
    }

    #[test]
    fn app_passwords_from_account() {
        let mut app = with_accounts();
        let id = call(&app.on_key(key(KeyCode::F(6))), "list_app_passwords");
        app.on_result(
            id,
            ok(json!([{"id": "p1", "label": "Телефон", "created_at": "2026-11-01T10:00:00Z", "revoked_at": null}])),
        );
        assert_eq!(main(&app).view.layout(), Some(&layout::APP_PASSWORDS));
        app.on_key(key(KeyCode::F(8)));
        let Some(Overlay::Confirm(req)) = &main(&app).overlay else {
            panic!("нет подтверждения отзыва");
        };
        assert_eq!(
            (req.op.id.as_str(), req.param("id")),
            ("revoke", Some("p1"))
        );
        app.on_key(key(KeyCode::Esc));
        app.on_key(key(KeyCode::F(7)));
        let Some(Overlay::Form(f)) = &main(&app).overlay else {
            panic!("нет формы выпуска");
        };
        assert_eq!(f.op.id, "issue");
        app.on_key(key(KeyCode::Esc));
        let id = call(&app.on_key(key(KeyCode::Esc)), "list_accounts");
        app.on_result(id, ok(json!([])));
        assert_eq!(main(&app).view.layout(), Some(&layout::ACCOUNTS));
    }
}
