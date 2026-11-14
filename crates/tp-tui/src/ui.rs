//! Отрисовка экрана 80×24 по утверждённому макету (решение 119): строка 0 — меню; слева дерево ресурсов шириной
//! 30 колонок, справа список или карточка (активная панель — двойная рамка, неактивная — одинарная); строка 21 —
//! состояние, 22 — эквивалентная команда `thepost admin …`, 23 — F-клавиши.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::Style;
use ratatui::widgets::{Block, BorderType, Widget};
use serde_json::Value;

use crate::app::{
    App, BODY_ROWS, CardView, LIST_ROWS, Level, ListView, LoginField, LoginForm, Main, Method,
    Overlay, Panel, Stage, TreeRow, View,
};
use crate::engine::{self, Form};
use crate::schema::{Kind, catalog};
use crate::theme::{Role, Theme};
use crate::tree::{NODE_CONFIG, SECTIONS};

/// Ширина экрана, на которую рассчитана раскладка.
pub const WIDTH: u16 = 80;
/// Высота экрана.
pub const HEIGHT: u16 = 24;
/// Ширина левой панели.
pub const LEFT: u16 = 30;

const ROW_STATUS: u16 = 21;
const ROW_CLI: u16 = 22;
const ROW_KEYS: u16 = 23;
const ROW_SEP: u16 = 17;

/// Подписи F-клавиш.
pub const KEYS: [&str; 10] = [
    "Помощь",
    "Фильтр",
    "Просм",
    "Правка",
    "Блок",
    "Пароль",
    "Создать",
    "Удалить",
    "Меню",
    "Выход",
];

/// Отрисовать интерфейс в кадр.
pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    render(app, area, frame.buffer_mut());
}

/// Отрисовать интерфейс в буфер.
pub fn render(app: &App, area: Rect, buf: &mut Buffer) {
    let t = app.theme;
    buf.set_style(area, t.style(Role::Screen));
    if area.width < WIDTH || area.height < HEIGHT {
        put(
            buf,
            0,
            0,
            "Нужен терминал не меньше 80×24",
            t.style(Role::Warn),
            area.width,
        );
        return;
    }
    let w = area.width;
    let mut p = Painter { buf, t, w };
    p.menu();
    let mut p = match &app.stage {
        Stage::Login(form) => p.login(form),
        Stage::Main(m) => p.main(m),
    };
    p.status(app);
    p.cli(&app.command_line());
    p.keys();
}

struct Painter<'a> {
    buf: &'a mut Buffer,
    t: Theme,
    w: u16,
}

impl Painter<'_> {
    fn s(&self, r: Role) -> Style {
        self.t.style(r)
    }

    fn put(&mut self, x: u16, y: u16, text: &str, r: Role) {
        let max = self.w.saturating_sub(x);
        let st = self.s(r);
        put(self.buf, x, y, text, st, max);
    }

    fn put_n(&mut self, x: u16, y: u16, text: &str, r: Role, width: u16) {
        let st = self.s(r);
        put(self.buf, x, y, &fit(text, width as usize), st, width);
    }

    fn fill(&mut self, x0: u16, x1: u16, y: u16, r: Role) {
        let st = self.s(r);
        self.buf
            .set_style(Rect::new(x0, y, x1.saturating_sub(x0) + 1, 1), st);
    }

    fn menu(&mut self) {
        let w = self.w;
        self.fill(0, w - 1, 0, Role::Menu);
        self.put(
            1,
            0,
            "Установка  Организация  Вид  Сервис  Справка",
            Role::Menu,
        );
        let brand = format!("IT TASK · The Post {}", env!("CARGO_PKG_VERSION"));
        let x = w.saturating_sub(brand.chars().count() as u16 + 1);
        self.put(x, 0, &brand, Role::Menu);
    }

    fn panel(&mut self, area: Rect, active: bool, title: &str) {
        let block = Block::bordered()
            .border_type(if active {
                BorderType::Double
            } else {
                BorderType::Plain
            })
            .border_style(self.s(if active { Role::FrameA } else { Role::Frame }))
            .title(format!(" {title} "))
            .title_alignment(Alignment::Center)
            .title_style(self.s(if active { Role::TitleA } else { Role::Text }));
        let inner = Rect::new(area.x + 1, area.y + 1, area.width - 2, area.height - 2);
        self.buf.set_style(inner, self.s(Role::Panel));
        block.render(area, self.buf);
        // Разделитель над строками подробностей.
        let (l, r, h) = if active {
            ("╟", "╢", "─")
        } else {
            ("├", "┤", "─")
        };
        let role = if active { Role::FrameA } else { Role::Frame };
        self.put(area.x, ROW_SEP, l, role);
        for x in area.x + 1..area.x + area.width - 1 {
            self.put(x, ROW_SEP, h, Role::Frame);
        }
        self.put(area.x + area.width - 1, ROW_SEP, r, role);
    }

    fn login(mut self, f: &LoginForm) -> Self {
        let w = self.w;
        self.buf
            .set_style(Rect::new(0, 1, w, ROW_STATUS - 1), self.s(Role::Panel));
        let (bw, bh) = (58u16, 12u16);
        let x0 = (w - bw) / 2;
        let y0 = 5;
        let area = Rect::new(x0, y0, bw, bh);
        Block::bordered()
            .border_type(BorderType::Double)
            .border_style(self.s(Role::FrameA))
            .title(" Вход в Admin API ")
            .title_alignment(Alignment::Center)
            .title_style(self.s(Role::TitleA))
            .render(area, self.buf);
        let (lx, vx, vw) = (x0 + 2, x0 + 20, bw - 23);
        let mut y = y0 + 2;
        for field in f.fields() {
            let (label, value) = match field {
                LoginField::Url => ("Адрес Admin API", f.url.clone()),
                LoginField::Method => ("Способ входа", format!("‹ {} ›", f.method.label())),
                LoginField::User => ("Имя входа", f.user.clone()),
                LoginField::Secret => (
                    if f.method == Method::Token {
                        "Токен"
                    } else {
                        "Пароль"
                    },
                    "•".repeat(f.secret.chars().count()),
                ),
            };
            self.put(lx, y, label, Role::Text);
            let role = if *field == f.focus {
                Role::Sel
            } else {
                Role::Text
            };
            self.fill(vx, vx + vw - 1, y, role);
            self.put_n(vx, y, &value, role, vw);
            y += 1;
        }
        y += 1;
        let lines: Vec<(Role, String)> = if let Some(e) = &f.error {
            wrap(e, (bw - 4) as usize)
                .into_iter()
                .map(|l| (Role::Err, l))
                .collect()
        } else if let Some(d) = &f.device {
            d.lines()
                .flat_map(|l| wrap(l, (bw - 4) as usize))
                .map(|l| (Role::Warn, l))
                .collect()
        } else if f.busy {
            vec![(Role::Muted, "вход…".to_owned())]
        } else {
            vec![(
                Role::Muted,
                "Enter — войти · ↑↓ — поле · ←→ — способ".to_owned(),
            )]
        };
        for (role, l) in lines.into_iter().take((y0 + bh - 1 - y) as usize) {
            self.put_n(lx, y, &l, role, bw - 4);
            y += 1;
        }
        self
    }

    fn main(mut self, m: &Main) -> Self {
        let w = self.w;
        let left = Rect::new(0, 1, LEFT, ROW_STATUS - 1);
        let right = Rect::new(LEFT, 1, w - LEFT, ROW_STATUS - 1);
        let tree_active = m.focus == Panel::Tree && m.overlay.is_none();
        self.panel(left, tree_active, "Ресурсы");
        let title = format!("{} › {}", m.org_label(&m.org), m.section);
        let title: String = title.chars().take((w - LEFT - 4) as usize).collect();
        self.panel(right, !tree_active, &title);
        self.tree(m, tree_active);
        let body = Body {
            x0: LEFT + 2,
            x1: w - 3,
            active: !tree_active,
        };
        match &m.overlay {
            Some(Overlay::Form(f)) => self.form(&body, f),
            Some(Overlay::Confirm(req)) => self.confirm(&body, req),
            Some(Overlay::Menu { sel }) => self.op_menu(&body, *sel),
            Some(Overlay::Help) => self.help(&body, m),
            None => match &m.view {
                View::List(l) => self.list(&body, l),
                View::Card(c) => self.card(&body, c),
                View::Info { title, lines } => {
                    self.put(body.x0, 4, title, Role::Text);
                    for (i, l) in lines.iter().enumerate() {
                        self.put_n(body.x0, 6 + i as u16, l, Role::Muted, body.width());
                    }
                }
            },
        }
        self
    }

    fn tree(&mut self, m: &Main, active: bool) {
        self.put_n(
            1,
            2,
            &format!("▾ {}", m.org_label(&m.org)),
            Role::Text,
            LEFT - 2,
        );
        let rows = m.tree_rows();
        for (i, row) in rows.iter().enumerate() {
            let y = match row {
                TreeRow::Section(s) => {
                    let y = 3 + *s as u16;
                    let branch = if *s + 1 == SECTIONS.len() {
                        "└"
                    } else {
                        "├"
                    };
                    self.put(3, y, branch, Role::Frame);
                    self.put_n(5, y, SECTIONS[*s].label, Role::Text, LEFT - 6);
                    y
                }
                TreeRow::Org(o) => {
                    let y = 3 + SECTIONS.len() as u16 + *o as u16;
                    let label = m.orgs.get(*o).map(|id| m.org_label(id)).unwrap_or_default();
                    self.put_n(1, y, &format!("▸ {label}"), Role::Text, LEFT - 2);
                    y
                }
                TreeRow::Config => {
                    self.put_n(1, 18, &format!("◆ {NODE_CONFIG}"), Role::Text, LEFT - 2);
                    self.put_n(3, 19, "config.yaml · на узле", Role::Muted, LEFT - 4);
                    18
                }
            };
            if i == m.tree {
                self.fill(1, LEFT - 2, y, if active { Role::Sel } else { Role::SelI });
            }
        }
    }

    fn list(&mut self, b: &Body, l: &ListView) {
        let cols = engine::list_columns(l.req.op);
        let visible = &l.rows[l.top.min(l.rows.len())..(l.top + LIST_ROWS).min(l.rows.len())];
        let total = b.width() as usize;
        let mut widths = Vec::new();
        let mut used = 0;
        for c in &cols {
            let want = visible
                .iter()
                .map(|r| engine::cell(r.get(&c.name)).chars().count())
                .chain([c.name.chars().count()])
                .max()
                .unwrap_or(4)
                .clamp(3, 28);
            if used + want > total {
                if total > used + 4 {
                    widths.push(total - used);
                }
                break;
            }
            widths.push(want);
            used += want + 1;
        }
        let mut x = b.x0;
        for (c, wdt) in cols.iter().zip(&widths) {
            self.put_n(x, 2, &c.name, Role::Head, *wdt as u16);
            x += *wdt as u16 + 1;
        }
        if l.rows.is_empty() {
            self.put(b.x0, 4, "Записей нет.", Role::Muted);
        }
        for (i, row) in visible.iter().enumerate() {
            let y = 3 + i as u16;
            let sel = l.top + i == l.sel;
            let role = match (sel, b.active) {
                (true, true) => Role::Sel,
                (true, false) => Role::SelI,
                _ => Role::Text,
            };
            if sel {
                self.fill(b.x0 - 1, b.x1 + 1, y, role);
            }
            let mut x = b.x0;
            for (c, wdt) in cols.iter().zip(&widths) {
                self.put_n(x, y, &engine::cell(row.get(&c.name)), role, *wdt as u16);
                x += *wdt as u16 + 1;
            }
        }
        if l.rows.len() > LIST_ROWS {
            let track = LIST_ROWS as u16;
            let len = l.rows.len().max(1);
            let thumb = ((LIST_ROWS * LIST_ROWS) / len).max(1) as u16;
            let start = ((l.top * LIST_ROWS) / len) as u16;
            let role = if b.active { Role::FrameA } else { Role::Frame };
            for i in 0..track {
                let ch = if (start..start + thumb).contains(&i) {
                    "▓"
                } else {
                    "░"
                };
                self.put(b.x1 + 2, 3 + i, ch, role);
            }
        }
        if let Some(row) = l.row() {
            let first = cols
                .first()
                .map(|c| engine::cell(row.get(&c.name)))
                .unwrap_or_default();
            let rest: Vec<String> = cols
                .iter()
                .skip(1)
                .take(2)
                .map(|c| engine::cell(row.get(&c.name)))
                .collect();
            self.put_n(b.x0, 18, &first, Role::Text, b.width());
            let x = b.x0 + first.chars().count() as u16 + 1;
            if x < b.x1 {
                self.put_n(
                    x,
                    18,
                    &format!("· {}", rest.join(" · ")),
                    Role::Muted,
                    b.x1 - x + 1,
                );
            }
        }
        let mut hint = String::from("Enter — открыть · F7 — создать");
        if l.next.is_some() {
            hint.push_str(" · ↓ — ещё");
        }
        self.put_n(b.x0, 19, &hint, Role::Muted, b.width());
    }

    fn card(&mut self, b: &Body, c: &CardView) {
        let lines = card_lines(&c.value);
        let top = c.top.min(lines.len().saturating_sub(1));
        let keyw = lines
            .iter()
            .map(|(k, _)| k.chars().count())
            .max()
            .unwrap_or(0)
            .min(18) as u16;
        for (i, (k, v)) in lines.iter().skip(top).take(BODY_ROWS).enumerate() {
            let y = 2 + i as u16;
            self.put_n(b.x0, y, k, Role::Head, keyw);
            let x = b.x0 + keyw + 1;
            self.put_n(x, y, v, Role::Text, b.x1 - x + 1);
        }
        self.put_n(b.x0, 18, &c.title, Role::Text, b.width());
        let hint = if c.back.is_some() {
            "Esc — назад · F4 — правка · F8 — удалить"
        } else {
            "↑↓ — прокрутка"
        };
        self.put_n(b.x0, 19, hint, Role::Muted, b.width());
    }

    fn form(&mut self, b: &Body, f: &Form) {
        self.put_n(b.x0, 2, &f.op.title(), Role::TitleA, b.width());
        self.put_n(
            b.x0,
            3,
            &format!("{} {}", f.op.method, f.op.path),
            Role::Muted,
            b.width(),
        );
        let lw: u16 = 18;
        let vx = b.x0 + lw + 1;
        let vw = b.x1 - vx + 1;
        // Строки: поле и под ним ошибка.
        let mut lines: Vec<(usize, bool)> = Vec::new();
        for (i, ff) in f.fields.iter().enumerate() {
            lines.push((i, false));
            if ff.error.is_some() {
                lines.push((i, true));
            }
        }
        let rows = 12usize;
        let at = lines.iter().position(|(i, _)| *i == f.focus).unwrap_or(0);
        let top = at.saturating_sub(rows - 2);
        if f.fields.is_empty() {
            self.put(b.x0, 5, "Полей нет: Enter — выполнить.", Role::Muted);
        }
        for (n, (i, err)) in lines.iter().skip(top).take(rows).enumerate() {
            let y = 5 + n as u16;
            let ff = &f.fields[*i];
            if *err {
                let e = ff.error.as_deref().unwrap_or_default();
                self.put_n(b.x0 + 1, y, &format!("× {e}"), Role::Err, b.width() - 1);
                continue;
            }
            let mark = if ff.field.required { "*" } else { "" };
            self.put_n(b.x0, y, &format!("{}{mark}", ff.field.name), Role::Text, lw);
            let focus = *i == f.focus;
            let role = if focus { Role::Sel } else { Role::Panel };
            self.fill(vx, b.x1, y, role);
            let secret = ff.field.name.contains("password");
            let shown = if secret {
                "•".repeat(ff.value.chars().count())
            } else {
                ff.value.clone()
            };
            // Длинное значение — видно конец (место ввода).
            let n = shown.chars().count();
            let shown: String = if n > vw as usize {
                shown.chars().skip(n + 1 - vw as usize).collect()
            } else {
                shown
            };
            self.put_n(
                vx,
                y,
                &shown,
                if focus { Role::Sel } else { Role::Text },
                vw,
            );
        }
        let focused = f.fields.get(f.focus);
        match (&f.error, focused) {
            (Some(e), _) => self.put_n(b.x0, 18, &format!("× {e}"), Role::Err, b.width()),
            (None, Some(ff)) => {
                let mut d = ff.field.description.clone();
                if !ff.field.choices.is_empty() {
                    d = format!("{} · {}", ff.field.choices.join("|"), d);
                } else if ff.field.kind == Kind::List {
                    d = format!("через запятую · {d}");
                } else if ff.field.kind == Kind::Json {
                    d = format!("JSON · {d}");
                }
                self.put_n(b.x0, 18, &d, Role::Muted, b.width());
            }
            (None, None) => {}
        }
        let choice = focused
            .is_some_and(|ff| !ff.field.choices.is_empty() || ff.field.kind == Kind::Boolean);
        let hint = if choice {
            "Enter — отправить · Esc — отмена · ←→ — выбор"
        } else {
            "Enter — отправить · Esc — отмена · ↑↓ — поле"
        };
        self.put_n(b.x0, 19, hint, Role::Muted, b.width());
    }

    fn confirm(&mut self, b: &Body, req: &engine::Request) {
        self.put(b.x0, 4, "▲ Удалить?", Role::Warn);
        self.put_n(b.x0, 6, &req.op.title(), Role::Text, b.width());
        self.put_n(
            b.x0,
            7,
            &format!("{} {}", req.op.method, req.op.path),
            Role::Muted,
            b.width(),
        );
        for (i, (name, v)) in req.op.path_names().iter().zip(&req.params).enumerate() {
            self.put_n(
                b.x0,
                9 + i as u16,
                &format!("{name}: {v}"),
                Role::Text,
                b.width(),
            );
        }
        self.put_n(b.x0, 18, "Действие необратимо.", Role::Warn, b.width());
        self.put_n(
            b.x0,
            19,
            "Enter — удалить · Esc — отмена",
            Role::Muted,
            b.width(),
        );
    }

    fn op_menu(&mut self, b: &Body, sel: usize) {
        let ops = &catalog().ops;
        let top = sel.saturating_sub(BODY_ROWS - 1);
        for (i, op) in ops.iter().enumerate().skip(top).take(BODY_ROWS) {
            let y = 2 + (i - top) as u16;
            let role = if i == sel { Role::Sel } else { Role::Text };
            if i == sel {
                self.fill(b.x0 - 1, b.x1 + 1, y, role);
            }
            self.put_n(
                b.x0,
                y,
                &op.tag,
                if i == sel { role } else { Role::Muted },
                12,
            );
            self.put_n(b.x0 + 13, y, &op.method, role, 6);
            self.put_n(b.x0 + 20, y, &op.title(), role, b.x1 - b.x0 - 19);
        }
        if let Some(op) = ops.get(sel) {
            self.put_n(b.x0, 18, &op.path, Role::Muted, b.width());
        }
        self.put_n(
            b.x0,
            19,
            "Enter — открыть · Esc — закрыть",
            Role::Muted,
            b.width(),
        );
    }

    fn help(&mut self, b: &Body, m: &Main) {
        let lines = [
            "Tab — другая панель · ↑↓ PgUp PgDn — строки",
            "Enter, F3 — открыть запись · Esc — назад",
            "F2 — фильтр (параметры запроса)",
            "F4 — правка · F5 — блокировка",
            "F6 — пароль приложения · F7 — создать",
            "F8 — удалить (с подтверждением)",
            "F9 — все операции Admin API · F10 — выход",
            "",
            "Внизу — та же операция командой CLI.",
        ];
        self.put(b.x0, 2, "Справка", Role::TitleA);
        for (i, l) in lines.iter().enumerate() {
            self.put_n(b.x0, 4 + i as u16, l, Role::Text, b.width());
        }
        let op = match &m.view {
            View::List(l) => Some(l.req.op),
            View::Card(c) => Some(c.req.op),
            View::Info { .. } => None,
        };
        if let Some(op) = op {
            for (i, l) in wrap(&op.summary.replace('\n', " "), b.width() as usize)
                .iter()
                .take(3)
                .enumerate()
            {
                self.put_n(b.x0, 14 + i as u16, l, Role::Muted, b.width());
            }
        }
        self.put_n(b.x0, 19, "Любая клавиша — закрыть", Role::Muted, b.width());
    }

    fn status(&mut self, app: &App) {
        let (level, text) = match app.status() {
            Some((l, t)) => (*l, t.clone()),
            None => (Level::Ok, idle_status(app)),
        };
        let (sign, role) = match level {
            Level::Ok => ("●", Role::Ok),
            Level::Warn => ("▲", Role::Warn),
            Level::Err => ("×", Role::Err),
        };
        self.put(1, ROW_STATUS, sign, role);
        let text = text.lines().next().unwrap_or_default().to_owned();
        let tr = if level == Level::Ok {
            Role::Muted
        } else {
            role
        };
        let w = self.w;
        self.put_n(3, ROW_STATUS, &text, tr, w - 4);
    }

    fn cli(&mut self, cmd: &str) {
        let w = self.w;
        self.put(1, ROW_CLI, "$", Role::Prompt);
        let n = cmd.chars().count() as u16;
        let label = "эквивалент CLI";
        if n + 4 < w - label.chars().count() as u16 - 2 {
            self.put(3, ROW_CLI, cmd, Role::Cmd);
            self.put(w - 14, ROW_CLI, label, Role::Muted);
        } else {
            self.put_n(3, ROW_CLI, cmd, Role::Cmd, w - 4);
        }
    }

    fn keys(&mut self) {
        for (i, label) in KEYS.iter().enumerate() {
            let x = i as u16 * 8;
            let num = (i + 1).to_string();
            self.put(x, ROW_KEYS, &num, Role::FkNum);
            let lw = 8 - num.len() as u16;
            self.put_n(x + num.len() as u16, ROW_KEYS, label, Role::Fk, lw);
        }
    }
}

/// Строка состояния без сообщения.
fn idle_status(app: &App) -> String {
    match &app.stage {
        Stage::Login(f) if f.busy => "вход…".to_owned(),
        Stage::Login(_) => "вход в Admin API".to_owned(),
        Stage::Main(m) => {
            let mut s = format!("{} → {}", m.session.login, m.session.server);
            if let View::List(l) = &m.view {
                s.push_str(&format!(" · записей: {}", l.rows.len()));
                if l.next.is_some() {
                    s.push_str(" · есть ещё");
                }
            }
            if app.busy() {
                s.push_str(" · загрузка…");
            }
            s
        }
    }
}

struct Body {
    x0: u16,
    x1: u16,
    active: bool,
}

impl Body {
    fn width(&self) -> u16 {
        self.x1 - self.x0 + 1
    }
}

/// Строки карточки: поле и значение; вложенные объекты — JSON одной строкой.
fn card_lines(v: &Value) -> Vec<(String, String)> {
    match v {
        Value::Object(map) => map
            .iter()
            .map(|(k, x)| (k.clone(), engine::cell(Some(x))))
            .collect(),
        other => vec![(String::new(), engine::cell(Some(other)))],
    }
}

/// Строка ровно `w` символов: длиннее — обрезана с «…», короче — дополнена пробелами.
fn fit(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n > w {
        let mut out: String = s.chars().take(w.saturating_sub(1)).collect();
        if w > 0 {
            out.push('…');
        }
        out
    } else {
        format!("{s}{}", " ".repeat(w - n))
    }
}

/// Разбить текст на строки не длиннее `w` по словам.
fn wrap(s: &str, w: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut line = String::new();
    for word in s.split_whitespace() {
        let need = line.chars().count() + word.chars().count() + usize::from(!line.is_empty());
        if need > w && !line.is_empty() {
            out.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        out.push(line);
    }
    out
}

fn put(buf: &mut Buffer, x: u16, y: u16, text: &str, style: Style, max: u16) {
    if y < buf.area.height && x < buf.area.width {
        buf.set_stringn(x, y, text, max as usize, style);
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;

    use super::*;
    use crate::app::LoginForm;
    use crate::app::tests::{key, with_accounts};
    use crossterm::event::KeyCode;

    fn screen(app: &App) -> (Vec<String>, Buffer) {
        let mut term = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).expect("терминал");
        term.draw(|f| draw(f, app)).expect("отрисовка");
        let buf = term.backend().buffer().clone();
        let rows = (0..HEIGHT)
            .map(|y| {
                (0..WIDTH)
                    .map(|x| buf[(x, y)].symbol().to_owned())
                    .collect()
            })
            .collect();
        (rows, buf)
    }

    /// 80×24: меню, обе панели (активная — двойная рамка), подсказка клавиш, строка команды CLI.
    #[test]
    fn main_screen_layout() {
        let mut app = with_accounts();
        let (rows, _) = screen(&app);
        assert!(
            rows[0].starts_with(" Установка  Организация"),
            "{}",
            rows[0]
        );
        assert!(
            rows[0]
                .trim_end()
                .ends_with(&format!("The Post {}", env!("CARGO_PKG_VERSION")))
        );
        // Активна правая панель: у неё двойная рамка, у дерева — одинарная.
        assert!(rows[1].starts_with('┌'), "{}", rows[1]);
        assert_eq!(rows[1].chars().nth(30), Some('╔'));
        assert_eq!(rows[20].chars().nth(79), Some('╝'));
        assert!(rows[1].contains(" Ресурсы "));
        assert!(
            rows[1].contains(" it-task.ru › Учётные записи "),
            "{}",
            rows[1]
        );
        assert!(rows[2].contains("▾ it-task.ru"));
        assert!(rows[3].contains("├ Учётные записи"));
        assert!(rows[13].contains("└ Очередь"));
        assert!(rows[18].contains("◆ Конфигурация узла"));
        assert!(rows[2].contains("login"), "{}", rows[2]);
        assert!(rows[3].contains("user0@it-task.ru"), "{}", rows[3]);
        assert_eq!(rows[17].chars().nth(30), Some('╟'));
        assert!(
            rows[21].contains("● admin@it-task.ru → node:9443 · записей: 20 · есть ещё"),
            "{}",
            rows[21]
        );
        assert!(
            rows[22].starts_with(" $ thepost admin account get acc-0"),
            "{}",
            rows[22]
        );
        assert!(rows[22].ends_with("эквивалент CLI"), "{}", rows[22]);
        assert_eq!(
            rows[23],
            "1Помощь 2Фильтр 3Просм  4Правка 5Блок   6Пароль 7Создать8Удалить9Меню   10Выход "
        );
        // Tab — активно дерево.
        app.on_key(key(KeyCode::Tab));
        let (rows, _) = screen(&app);
        assert!(rows[1].starts_with('╔'), "{}", rows[1]);
        assert_eq!(rows[1].chars().nth(30), Some('┌'));
        assert!(
            rows[22].starts_with(" $ thepost admin account list"),
            "{}",
            rows[22]
        );
    }

    #[test]
    fn login_screen() {
        let mut app = App::new(
            Theme { mono: false },
            LoginForm::new("https://node:9443".into(), false),
        );
        for c in "admin".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Char('x')));
        let (rows, _) = screen(&app);
        let all = rows.join("\n");
        assert!(all.contains(" Вход в Admin API "), "{all}");
        assert!(all.contains("https://node:9443"));
        assert!(all.contains("‹ пароль ›"));
        assert!(all.contains("Пароль            •"), "{all}");
        assert!(!all.contains("admin x"));
        assert!(
            rows[22].starts_with(" $ thepost admin login --user admin"),
            "{}",
            rows[22]
        );
        assert!(rows[23].starts_with("1Помощь"));
    }

    /// Монохром (`NO_COLOR`): ни одного цвета, выделение — инверсией.
    #[test]
    fn monochrome() {
        let mut app = with_accounts();
        app.theme = Theme { mono: true };
        let (_, buf) = screen(&app);
        for cell in buf.content() {
            assert_eq!((cell.fg, cell.bg), (Color::Reset, Color::Reset), "{cell:?}");
        }
    }

    /// Форма с неверным значением: ошибка под полем.
    #[test]
    fn form_error_at_field() {
        let mut app = with_accounts();
        app.on_key(key(KeyCode::F(2)));
        for _ in 0..3 {
            app.on_key(key(KeyCode::Down));
        }
        for c in "много".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Enter));
        let (rows, _) = screen(&app);
        let all = rows.join("\n");
        assert!(all.contains("limit"), "{all}");
        assert!(all.contains("× нужно целое число"), "{all}");
        assert!(
            rows[22].contains("account list --limit много"),
            "{}",
            rows[22]
        );
    }

    #[test]
    fn too_small() {
        let app = with_accounts();
        let mut term = Terminal::new(TestBackend::new(60, 20)).expect("терминал");
        term.draw(|f| draw(f, &app)).expect("отрисовка");
        let buf = term.backend().buffer();
        let row: String = (0..30).map(|x| buf[(x, 0)].symbol().to_owned()).collect();
        assert!(row.starts_with("Нужен терминал не меньше 80×24"));
    }
}
