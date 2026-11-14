//! Отрисовка экрана 80×24 по утверждённому макету (решение 119): строка 0 — меню; слева дерево ресурсов шириной
//! 30 колонок, справа список или карточка (активная панель — двойная рамка, неактивная — одинарная); строка 21 —
//! состояние, 22 — эквивалентная команда `thepost admin …`, 23 — F-клавиши.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, BorderType, Widget};
use serde_json::Value;

use crate::app::{
    App, BODY_ROWS, CardView, ConfigEdit, ConfigView, LIST_ROWS, Level, ListView, LoginField,
    LoginForm, Main, Method, Overlay, Panel, Stage, TreeRow, View,
};
use crate::engine::{self, Form};
use crate::layout::{self, Layout};
use crate::node::Access;
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

/// Отрисовать интерфейс в кадр.
pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    render(app, area, frame.buffer_mut());
}

/// Отрисовать интерфейс в буфер.
pub fn render(app: &App, area: Rect, buf: &mut Buffer) {
    let t = &app.theme;
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
    p.keys(&app.keys());
}

struct Painter<'a> {
    buf: &'a mut Buffer,
    t: &'a Theme,
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

    /// Строка меню: градиент бренда, пункты и название бренда с версией (жирным) справа.
    fn menu(&mut self) {
        let w = self.w;
        for x in 0..w {
            self.buf.set_style(Rect::new(x, 0, 1, 1), self.t.menu(x, w));
        }
        let plain = Style::default();
        put(
            self.buf,
            1,
            0,
            "Установка  Организация  Вид  Сервис  Справка",
            plain,
            w - 1,
        );
        let brand = format!(
            "{} · The Post {}",
            self.t.brand().name,
            env!("CARGO_PKG_VERSION")
        );
        let x = w.saturating_sub(brand.chars().count() as u16 + 1);
        put(
            self.buf,
            x,
            0,
            &brand,
            plain.add_modifier(Modifier::BOLD),
            w - x,
        );
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

    /// Знак и название бренда над формой входа; без графики — только название.
    fn brand(&mut self) {
        let t = self.t;
        let name = &t.brand().name;
        let nw = name.chars().count() as u16;
        match t.mark() {
            Some(mark) => {
                let s = mark.size();
                let x = self.w.saturating_sub(s.width + 2 + nw) / 2;
                t.render_mark(mark, Rect::new(x, 1, s.width, s.height), self.buf);
                self.put(x + s.width + 2, 1 + s.height / 2, name, Role::Name);
            }
            None => self.put(self.w.saturating_sub(nw) / 2, 3, name, Role::Name),
        }
    }

    fn login(mut self, f: &LoginForm) -> Self {
        let w = self.w;
        self.buf
            .set_style(Rect::new(0, 1, w, ROW_STATUS - 1), self.s(Role::Panel));
        self.brand();
        let (bw, bh) = (58u16, 12u16);
        let x0 = (w - bw) / 2;
        let y0 = 6;
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
            Some(Overlay::Form(f)) => self.form(&body, f, &f.op.title()),
            Some(Overlay::Lookup(f, then)) => self.form(&body, f, &then.title()),
            Some(Overlay::Confirm(req)) => self.confirm(&body, req),
            Some(Overlay::Menu { sel }) => self.op_menu(&body, *sel),
            Some(Overlay::Help) => self.help(&body, m),
            Some(Overlay::ConfigEdit(e)) => self.config_edit(&body, m, e),
            None => match &m.view {
                View::List(l) => self.list(&body, l),
                View::Card(c) => self.card(&body, c),
                View::Config(c) => self.config(&body, c),
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
                    let (label, ro) = match &m.view {
                        View::Config(c) => match &c.file.access {
                            Access::Write => ("config.yaml · правка".to_owned(), false),
                            Access::Managed(tool) => {
                                (format!("config.yaml · {}", capitalized(tool)), true)
                            }
                            Access::ReadOnly(_) => ("config.yaml · нет права".to_owned(), true),
                        },
                        _ => ("config.yaml · на узле".to_owned(), false),
                    };
                    self.put_n(3, 19, &label, Role::Muted, LEFT - 8);
                    if ro {
                        self.put(LEFT - 5, 19, "R/O", Role::Warn);
                    }
                    18
                }
            };
            if i == m.tree {
                self.fill(1, LEFT - 2, y, if active { Role::Sel } else { Role::SelI });
            }
        }
    }

    fn list(&mut self, b: &Body, l: &ListView) {
        if let Some(layout) = l.layout {
            return self.layout_list(b, l, layout);
        }
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
        self.scrollbar(b, l.rows.len(), l.top);
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

    /// Конфигурация узла: дерево параметров схемы, значение или значение по умолчанию, описание и доступ.
    fn config(&mut self, b: &Body, c: &ConfigView) {
        let (nw, vx) = (22u16, b.x0 + 23);
        let vw = b.x1 - vx + 1;
        self.put_n(b.x0, 2, "Параметр", Role::Head, nw);
        self.put_n(vx, 2, "Значение", Role::Head, vw);
        let rows = c.rows();
        let end = (c.top + LIST_ROWS).min(rows.len());
        for (i, p) in rows[c.top.min(end)..end].iter().enumerate() {
            let y = 3 + i as u16;
            let sel = c.top + i == c.sel;
            let line = match (sel, b.active) {
                (true, true) => Some(Role::Sel),
                (true, false) => Some(Role::SelI),
                _ => None,
            };
            if let Some(r) = line {
                self.fill(b.x0 - 1, b.x1 + 1, y, r);
            }
            let indent = "  ".repeat(p.path.len() - 1);
            let name = if p.section {
                let mark = if c.open.contains(&p.key()) {
                    "▾"
                } else {
                    "▸"
                };
                format!("{indent}{mark} {}", p.name())
            } else {
                format!("{indent}  {}", p.name())
            };
            self.put_n(b.x0, y, &name, line.unwrap_or(Role::Text), nw);
            let (value, role) = match (p.section, c.file.value(p)) {
                (true, _) => (String::new(), Role::Muted),
                (false, Some(v)) => (engine::cell(Some(v)), Role::Text),
                (false, None) => match &p.default {
                    Some(d) => (format!("{} (умолч.)", engine::cell(Some(d))), Role::Muted),
                    None => ("—".to_owned(), Role::Muted),
                },
            };
            self.put_n(vx, y, &value, line.unwrap_or(role), vw);
        }
        self.scrollbar(b, rows.len(), c.top);
        if let Some(p) = c.selected() {
            let d = if p.field.description.is_empty() {
                p.key()
            } else {
                p.field.description.clone()
            };
            self.put_n(b.x0, 18, &d, Role::Muted, b.width());
        }
        match c.file.access.reason() {
            Some(reason) => self.put_n(
                b.x0,
                19,
                &format!("▲ только чтение: {reason}"),
                Role::Warn,
                b.width(),
            ),
            None => self.put_n(
                b.x0,
                19,
                "Enter — правка · ←→ — секции · F2 — перечитать",
                Role::Muted,
                b.width(),
            ),
        }
    }

    /// Правка параметра конфигурации узла.
    fn config_edit(&mut self, b: &Body, m: &Main, e: &ConfigEdit) {
        let p = &e.param;
        self.put_n(b.x0, 2, &p.key(), Role::TitleA, b.width());
        let file = match &m.view {
            View::Config(c) => c.file.path.display().to_string(),
            _ => String::new(),
        };
        self.put_n(b.x0, 3, &file, Role::Muted, b.width());
        for (i, l) in wrap(&p.field.description, b.width() as usize)
            .iter()
            .take(3)
            .enumerate()
        {
            self.put_n(b.x0, 5 + i as u16, l, Role::Text, b.width());
        }
        let lw: u16 = 18;
        let vx = b.x0 + lw + 1;
        let vw = b.x1 - vx + 1;
        self.put_n(b.x0, 9, "Значение", Role::Text, lw);
        self.fill(vx, b.x1, 9, Role::Sel);
        let n = e.value.chars().count();
        let shown: String = if n > vw as usize {
            e.value.chars().skip(n + 1 - vw as usize).collect()
        } else {
            e.value.clone()
        };
        self.put_n(vx, 9, &shown, Role::Sel, vw);
        let default = p
            .default
            .as_ref()
            .map_or_else(|| "—".to_owned(), |d| engine::cell(Some(d)));
        self.put_n(b.x0, 10, "По умолчанию", Role::Muted, lw);
        self.put_n(vx, 10, &default, Role::Muted, vw);
        let mut kind = match p.field.kind {
            Kind::Integer => "целое".to_owned(),
            Kind::Number => "число".to_owned(),
            Kind::Boolean => "да или нет · ←→".to_owned(),
            Kind::List => "через запятую".to_owned(),
            Kind::Json => "JSON".to_owned(),
            Kind::Text => "текст".to_owned(),
        };
        if !p.field.choices.is_empty() {
            kind = format!("{} · ←→", p.field.choices.join("|"));
        }
        self.put_n(b.x0, 11, "Вид", Role::Muted, lw);
        self.put_n(vx, 11, &kind, Role::Muted, vw);
        if let Some(err) = &e.error {
            for (i, l) in wrap(&format!("× {err}"), b.width() as usize)
                .iter()
                .take(3)
                .enumerate()
            {
                self.put_n(b.x0, 13 + i as u16, l, Role::Err, b.width());
            }
        }
        let (hint, role) = if e.busy {
            ("проверка: thepost config check…", Role::Muted)
        } else {
            ("Enter — проверить и записать (копия .bak)", Role::Muted)
        };
        self.put_n(b.x0, 18, hint, role, b.width());
        self.put_n(
            b.x0,
            19,
            "пусто — значение по умолчанию · Esc — отмена",
            Role::Muted,
            b.width(),
        );
    }

    /// Полоса прокрутки списка длиннее экрана.
    fn scrollbar(&mut self, b: &Body, len: usize, top: usize) {
        if len <= LIST_ROWS {
            return;
        }
        let track = LIST_ROWS as u16;
        let thumb = ((LIST_ROWS * LIST_ROWS) / len).max(1) as u16;
        let start = ((top * LIST_ROWS) / len) as u16;
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

    /// Список в ручной раскладке раздела: колонки с подписями и знаками, подробности выделенной записи.
    fn layout_list(&mut self, b: &Body, l: &ListView, layout: &Layout) {
        let mut x = b.x0;
        for c in layout.columns {
            self.put_n(x, 2, c.head, Role::Head, c.width);
            x += c.width + 1;
        }
        if l.rows.is_empty() {
            self.put(b.x0, 4, "Записей нет.", Role::Muted);
        }
        let end = (l.top + LIST_ROWS).min(l.rows.len());
        for (i, row) in l.rows[l.top.min(end)..end].iter().enumerate() {
            let y = 3 + i as u16;
            let sel = l.top + i == l.sel;
            let line = match (sel, b.active) {
                (true, true) => Some(Role::Sel),
                (true, false) => Some(Role::SelI),
                _ => None,
            };
            if let Some(r) = line {
                self.fill(b.x0 - 1, b.x1 + 1, y, r);
            }
            let mut x = b.x0;
            for c in layout.columns {
                let (text, role) = layout::cell(c, row);
                self.put_n(x, y, &text, line.unwrap_or(role), c.width);
                x += c.width + 1;
            }
        }
        self.scrollbar(b, l.rows.len(), l.top);
        let Some(row) = l.row() else {
            return;
        };
        for (y, parts) in [18, 19].into_iter().zip((layout.detail)(row)) {
            let mut x = b.x0;
            for (text, role) in parts {
                if x > b.x1 {
                    break;
                }
                self.put_n(
                    x,
                    y,
                    &text,
                    role,
                    (b.x1 - x + 1).min(text.chars().count() as u16),
                );
                x += text.chars().count() as u16;
            }
        }
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

    fn form(&mut self, b: &Body, f: &Form, title: &str) {
        self.put_n(b.x0, 2, title, Role::TitleA, b.width());
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
            View::Info { .. } | View::Config(_) => None,
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

    fn keys(&mut self, labels: &[&str; 10]) {
        for (i, label) in labels.iter().enumerate() {
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

/// Слово с прописной буквы (`ansible` → `Ansible`).
fn capitalized(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
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
    use crate::app::tests::{call, key, ok, section, with_accounts, with_members};
    use crate::theme::{Brand, Mode};
    use crossterm::event::KeyCode;
    use serde_json::json;

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
        assert!(rows[2].contains("Адрес"), "{}", rows[2]);
        assert!(rows[3].contains("●"), "{}", rows[3]);
        assert!(
            rows[18].contains("user0@it-task.ru · Пользователь 0"),
            "{}",
            rows[18]
        );
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
            "1Помощь 2Фильтр 3Просм  4Правка 5Блок   6Пароли 7Создать8Скрыть 9Меню   10Выход "
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
            Theme::new(Mode::Dark, Brand::default()),
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

    const MODES: [Mode; 5] = [Mode::Dark, Mode::Light, Mode::C256, Mode::C16, Mode::Mono];

    /// 80×24 в каждом из пяти режимов: раскладка та же, знаки состояний на месте, цвета — только своего
    /// режима (в монохроме — ни одного); строка меню — градиент в truecolor и 256, сплошная в 16 цветах,
    /// инверсия в монохроме.
    #[test]
    fn five_modes() {
        for mode in MODES {
            let mut app = with_accounts();
            app.theme = Theme::new(mode, Brand::default());
            let (rows, buf) = screen(&app);
            assert!(rows[0].contains("IT TASK · The Post"), "{mode:?}");
            assert_eq!(rows[1].chars().nth(30), Some('╔'), "{mode:?}");
            assert!(rows[3].contains('●'), "{mode:?}: {}", rows[3]);
            assert!(rows[21].starts_with(" ●"), "{mode:?}: {}", rows[21]);
            let (left, right) = (buf[(0, 0)].bg, buf[(79, 0)].bg);
            let sel = buf[(40, 3)].style();
            match mode {
                Mode::Dark | Mode::Light => {
                    assert_eq!(left, Color::Rgb(0xE4, 0x10, 0x7F));
                    assert_eq!(right, Color::Rgb(0x17, 0x7C, 0xAA));
                    assert_eq!(sel.bg, Some(Color::Rgb(0xE3, 0x10, 0x7F)));
                }
                Mode::C256 => {
                    assert_eq!(left, Color::Indexed(162));
                    assert!(matches!(right, Color::Indexed(_)) && right != left);
                }
                Mode::C16 => assert_eq!((left, right), (Color::Blue, Color::Blue)),
                Mode::Mono => {
                    assert!(buf[(0, 0)].modifier.contains(Modifier::REVERSED));
                    assert!(sel.add_modifier.contains(Modifier::REVERSED));
                }
            }
            for cell in buf.content() {
                let ok = |c: Color| match mode {
                    Mode::Dark | Mode::Light => matches!(c, Color::Rgb(..) | Color::Reset),
                    Mode::C256 => matches!(c, Color::Indexed(16..) | Color::Reset),
                    Mode::C16 => !matches!(c, Color::Rgb(..) | Color::Indexed(_)),
                    Mode::Mono => c == Color::Reset,
                };
                assert!(ok(cell.fg) && ok(cell.bg), "{mode:?}: {cell:?}");
            }
        }
    }

    /// Экран входа: знак бренда полублоками и название; в 16 цветах и монохроме — только название IT TASK,
    /// жирное, белое или чёрное по фону режима.
    #[test]
    fn login_brand() {
        let picker = ratatui_image::picker::Picker::halfblocks();
        for mode in MODES {
            let theme = Theme::new(mode, Brand::default()).with_mark(&picker);
            let app = App::new(theme, LoginForm::new("https://node:9443".into(), false));
            let (rows, buf) = screen(&app);
            let top = rows[1..5].join(
                "
",
            );
            assert!(
                rows.join(
                    "
"
                )
                .contains(" Вход в Admin API ")
            );
            let blocks = top.contains('▀') || top.contains('▄');
            assert_eq!(
                blocks,
                mode.graphics(),
                "{mode:?}
{top}"
            );
            let y = (1..5)
                .find(|&y| rows[y].contains("IT TASK"))
                .unwrap_or_else(|| {
                    panic!(
                        "{mode:?}: нет названия
{top}"
                    )
                });
            let x = rows[y].chars().position(|c| c == 'I').unwrap_or_default();
            let cell = &buf[(x as u16, y as u16)];
            assert!(cell.modifier.contains(Modifier::BOLD), "{mode:?}");
            let want = match mode {
                Mode::Dark => Color::Rgb(0xFF, 0xFF, 0xFF),
                Mode::Light => Color::Rgb(0x16, 0x16, 0x16),
                Mode::C256 => Color::Indexed(231),
                Mode::C16 => Color::White,
                Mode::Mono => Color::Reset,
            };
            assert_eq!(cell.fg, want, "{mode:?}");
        }
    }

    /// Бренд заказчика (решение 122): его название в меню и на входе, градиент от его акцента.
    #[test]
    fn customer_brand() {
        let brand = Brand {
            name: "ACME".into(),
            mark: None,
            primary: crate::theme::Rgb(0x00, 0x33, 0x99),
            accent: crate::theme::Rgb(0xCC, 0x00, 0x00),
        };
        let app = App::new(
            Theme::new(Mode::Dark, brand),
            LoginForm::new("https://node:9443".into(), false),
        );
        let (rows, buf) = screen(&app);
        assert!(rows[0].contains("ACME · The Post"), "{}", rows[0]);
        assert!(rows[3].contains("ACME"), "{}", rows[3]);
        assert_eq!(buf[(0, 0)].bg, Color::Rgb(0xCC, 0x00, 0x00));
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

    /// Ручные раскладки разделов: подписи колонок, знаки состояний, подробности и F-клавиши раздела.
    #[test]
    fn section_layouts() {
        let app = with_members();
        let (rows, _) = screen(&app);
        assert!(rows[2].contains("Вид  Участник"), "{}", rows[2]);
        assert!(rows[3].contains("уч.  user1@it-task.ru"), "{}", rows[3]);
        assert!(rows[4].contains("гр.  Инженеры"), "{}", rows[4]);
        assert!(
            rows[18].contains("user1@it-task.ru · acc-1"),
            "{}",
            rows[18]
        );
        assert!(rows[23].contains("7Добав"), "{}", rows[23]);
        assert!(!rows[23].contains("Фильтр"), "{}", rows[23]);

        let mut app = with_accounts();
        let id = call(&section(&mut app, "Политики"), "get_org");
        app.on_result(
            id,
            ok(json!({"policy": {"send_per_hour": 200},
                      "effective": {"send_per_hour": 200, "quota_bytes": 5_368_709_120u64}})),
        );
        app.on_key(key(KeyCode::Tab));
        let (rows, _) = screen(&app);
        let all = rows.join(
            "
",
        );
        assert!(
            rows[2].contains("Параметр           Задано        Действует"),
            "{}",
            rows[2]
        );
        assert!(
            all.contains("quota_bytes        —             5 ГБ"),
            "{all}"
        );
        assert!(
            all.contains("send_per_hour      200           200"),
            "{all}"
        );
        assert!(
            rows[23].contains("5Домены") && rows[23].contains("6Ящик"),
            "{}",
            rows[23]
        );

        let id = call(&section(&mut app, "Журналы"), "search_audit");
        app.on_result(
            id,
            ok(json!({"entries": [{"seq": 1, "at": "2026-11-14T09:30:00Z", "actor_login": "admin@it-task.ru",
                "method": "POST", "status": 409, "path": "/api/v1/orgs/o1/accounts", "roles": ["organization_admin"]}],
                "next": null})),
        );
        let (rows, _) = screen(&app);
        assert!(
            rows[2].contains("Время            Администратор    Метод  Код"),
            "{}",
            rows[2]
        );
        assert!(
            rows[3].contains("2026-11-14 09:30 admin@it-task.ru POST   × 409"),
            "{}",
            rows[3]
        );
        assert!(
            rows[18].contains("POST /api/v1/orgs/o1/accounts"),
            "{}",
            rows[18]
        );
        assert!(rows[23].contains("5Достав"), "{}", rows[23]);

        // Заблокированная учётная запись с почти полной квотой: знак ×, строка квоты.
        let mut app = with_accounts();
        let id = call(&section(&mut app, "Учётные записи"), "list_accounts");
        app.on_result(
            id,
            ok(json!([{"id": "a1", "login": "petrov@it-task.ru", "display_name": "Петров Илья", "enabled": false,
                "hidden_from_gal": true, "quota": {"used_bytes": 4_831_838_208u64, "limit_bytes": 5_368_709_120u64, "messages": 9}}])),
        );
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Tab));
        let (rows, _) = screen(&app);
        assert!(
            rows[3].contains("petrov@it-task.ru     Петров Илья     5 ГБ   ×"),
            "{}",
            rows[3]
        );
        assert!(rows[18].contains("Петров Илья · скрыта из"), "{}", rows[18]);
        assert!(
            rows[19].contains("× заблокирована · 4,5 ГБ из 5 ГБ █"),
            "{}",
            rows[19]
        );
        assert!(rows[19].contains("90 %"), "{}", rows[19]);
    }

    /// Конфигурация узла на узле: дерево параметров, правка с проверкой `config check` до записи; ошибка — у поля.
    #[test]
    fn node_config_edit() {
        use crate::app::{Effect, Overlay, Stage};
        use crate::node::tests::Dir;
        use crate::node::{load, save};

        let dir = Dir::new();
        let node = dir.node("{\"domain\": \"it-task.ru\"}\n");
        let mut app = App::new(
            Theme::new(Mode::Dark, Brand::default()),
            LoginForm::new("https://node:9443".into(), false),
        );
        app.node = true;
        app.on_login(Ok((crate::app::tests::who(), "node:9443".into())));
        app.on_key(key(KeyCode::Tab));
        app.on_key(key(KeyCode::End));
        assert_eq!(app.on_key(key(KeyCode::Enter)), vec![Effect::ConfigLoad]);
        app.on_config(load(&node));
        assert_eq!(app.keys()[1], "Перечит");
        // ↓↓ — секция smtp, → — раскрыть, ↓↓↓ — max_size (greeting, limits, max_size).
        for k in [
            KeyCode::Down,
            KeyCode::Down,
            KeyCode::Right,
            KeyCode::Down,
            KeyCode::Down,
            KeyCode::Down,
        ] {
            app.on_key(key(k));
        }
        let (rows, _) = screen(&app);
        let all = rows.join("\n");
        assert!(
            rows[2].contains("Параметр               Значение"),
            "{}",
            rows[2]
        );
        let row = |name: &str| {
            rows.iter()
                .find(|r| r.contains(name))
                .cloned()
                .unwrap_or_default()
        };
        assert!(
            row(" domain ").contains("   domain               it-task.ru"),
            "{all}"
        );
        assert!(all.contains("▾ smtp"), "{all}");
        assert!(
            row("max_size").contains("     max_size           10 (умолч.)"),
            "{all}"
        );
        assert!(
            rows[18].contains("Предельный размер письма."),
            "{}",
            rows[18]
        );
        assert!(rows[19].contains("config.yaml · правка"), "{}", rows[19]);
        assert!(rows[22].contains("$ thepost config check"), "{}", rows[22]);

        app.on_key(key(KeyCode::Enter));
        for c in "500".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        let fx = app.on_key(key(KeyCode::Enter));
        let [
            Effect::ConfigSave {
                path,
                value,
                loaded,
            },
        ] = fx.as_slice()
        else {
            panic!("нет записи: {fx:?}");
        };
        // Неверное значение: config check отказывает, ошибка — у поля, файл не тронут.
        let r = save(&node, loaded, path, value.clone());
        assert!(app.on_config_saved(r).is_empty());
        let (rows, _) = screen(&app);
        let all = rows.join("\n");
        assert!(all.contains("× smtp.max_size: invalid value: 500"), "{all}");
        assert!(
            rows[21].contains("× не записано: ошибка проверки"),
            "{}",
            rows[21]
        );
        let Stage::Main(m) = &app.stage else {
            panic!("не вошли");
        };
        assert!(matches!(&m.overlay, Some(Overlay::ConfigEdit(e)) if !e.busy));
        // Верное значение: записано с копией, файл перечитывается.
        for _ in 0..3 {
            app.on_key(key(KeyCode::Backspace));
        }
        for c in "50".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        let fx = app.on_key(key(KeyCode::Enter));
        let [
            Effect::ConfigSave {
                path,
                value,
                loaded,
            },
        ] = fx.as_slice()
        else {
            panic!("нет записи: {fx:?}");
        };
        let r = save(&node, loaded, path, value.clone());
        assert_eq!(app.on_config_saved(r), vec![Effect::ConfigLoad]);
        assert!(dir.0.join("config.yaml.bak").exists());
        app.on_config(load(&node));
        let (rows, _) = screen(&app);
        let all = rows.join("\n");
        let max = rows
            .iter()
            .find(|r| r.contains("max_size"))
            .cloned()
            .unwrap_or_default();
        assert!(max.contains("     max_size           50 "), "{all}");
        assert!(rows[21].contains("● записано, копия"), "{}", rows[21]);
    }

    /// Файл с меткой DEP-55 — только чтение с пояснением, где править.
    #[test]
    fn node_config_managed() {
        use crate::node::load;
        use crate::node::tests::Dir;

        let dir = Dir::new();
        let node = dir.node("# thepost: managed-by=ansible\n{\"domain\": \"it-task.ru\"}\n");
        let mut app = App::new(
            Theme::new(Mode::Dark, Brand::default()),
            LoginForm::new("https://node:9443".into(), false),
        );
        app.node = true;
        app.on_login(Ok((crate::app::tests::who(), "node:9443".into())));
        app.on_key(key(KeyCode::Tab));
        app.on_key(key(KeyCode::End));
        app.on_key(key(KeyCode::Enter));
        app.on_config(load(&node));
        app.on_key(key(KeyCode::Enter));
        let (rows, _) = screen(&app);
        assert!(
            rows[19].contains("config.yaml · Ansible R/O"),
            "{}",
            rows[19]
        );
        assert!(
            rows[19].contains("▲ только чтение: файл управляется Ansible"),
            "{}",
            rows[19]
        );
        assert!(rows[21].contains("▲ только чтение"), "{}", rows[21]);
        assert!(!rows.join("\n").contains("По умолчанию"));
    }

    /// Без доступа к файлу узла — пояснение, где править.
    #[test]
    fn node_config_elsewhere() {
        let mut app = with_accounts();
        app.on_key(key(KeyCode::Tab));
        app.on_key(key(KeyCode::End));
        assert!(app.on_key(key(KeyCode::Enter)).is_empty());
        let (rows, _) = screen(&app);
        assert!(
            rows.join("\n")
                .contains("Правка config.yaml — на самом узле")
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
