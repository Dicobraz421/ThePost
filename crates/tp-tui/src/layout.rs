//! Ручные раскладки главных разделов (TUI-03, решение 118) поверх движка: колонки с подписями и знаками
//! состояний, строки подробностей выделенной записи и действия раздела по F-клавишам. Запросы — те же операции
//! схемы Admin API, что у движка; раздел без раскладки — экран движка.

use std::fmt;

use serde_json::{Map, Value};

use crate::engine;
use crate::schema::catalog;
use crate::theme::Role;

/// Откуда берутся строки раскладки.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rows {
    /// Записи страницы списка (как у движка): листание, фильтр F2, действия движка.
    Page,
    /// Участники группы из записи группы (`members`, имена — `names`).
    Members,
    /// Параметры политики уровня: поля тела операции `put` — заданное (`policy`) и действующее (`effective`).
    Policy(&'static str),
}

/// Вид значения ячейки.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fmt {
    /// Текст поля.
    Text,
    /// Момент RFC 3339 — `ГГГГ-ММ-ДД ЧЧ:ММ`.
    Time,
    /// Байты — в КБ, МБ, ГБ.
    Bytes,
    /// Число элементов списка.
    Count,
    /// Знак состояния учётной записи: × заблокирована, ▲ квота занята на 90 %, ● норма.
    Account,
    /// Знак пароля приложения: × отозван, ● действует.
    Revoked,
    /// Знак группы: ▲ модерируемая, ● обычная.
    Group,
    /// Код ответа: × 4xx/5xx, ● прочие.
    Status,
    /// Решение журнала доставки: ● принято, ▲ отложено или задержано, × отклонено.
    Decision,
    /// Вид участника группы: уч. или гр.
    MemberKind,
    /// Квота учётной записи (`quota.limit_bytes`).
    Quota,
}

/// Колонка раскладки.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Col {
    /// Заголовок.
    pub head: &'static str,
    /// Поле записи.
    pub field: &'static str,
    /// Ширина.
    pub width: u16,
    /// Вид значения.
    pub fmt: Fmt,
}

const fn col(head: &'static str, field: &'static str, width: u16, fmt: Fmt) -> Col {
    Col {
        head,
        field,
        width,
        fmt,
    }
}

/// Действие раздела.
#[derive(Debug, Clone, Copy)]
pub enum Act {
    /// Операция `op` над выделенной записью; значения формы — `prefill` от записи (переключатель).
    Item {
        /// `operationId`.
        op: &'static str,
        /// Начальные значения формы.
        prefill: fn(&Value) -> Map<String, Value>,
    },
    /// Список или запись `op` в контексте выделенной записи; Esc — назад.
    Open {
        /// `operationId`.
        op: &'static str,
        /// Раскладка.
        layout: &'static Layout,
    },
    /// Операция `op` в контексте раздела (без записи); `nest` — Esc возвращает назад.
    Switch {
        /// `operationId`.
        op: &'static str,
        /// Раскладка.
        layout: &'static Layout,
        /// Вложить в текущий экран.
        nest: bool,
    },
    /// Учётная запись по имени входа, затем `op` с ней; Esc — назад.
    Account {
        /// `operationId`.
        op: &'static str,
        /// Раскладка.
        layout: &'static Layout,
    },
    /// Правка уровня политики операцией `op`; значения — заданные на уровне (`policy`).
    EditPolicy(&'static str),
    /// Добавить участника группы по имени входа.
    AddMember,
    /// Убрать выделенного участника группы.
    RemoveMember,
}

/// F-клавиша раздела.
#[derive(Debug, Clone, Copy)]
pub struct Key {
    /// Номер (1–10); 3 — и Enter.
    pub n: u8,
    /// Подпись (до 7 знаков).
    pub label: &'static str,
    /// Действие.
    pub act: Act,
}

/// Строка подробностей: части текста с ролями.
pub type Line = Vec<(String, Role)>;

/// Ручная раскладка раздела.
pub struct Layout {
    /// Имя (для отладки и тестов).
    pub name: &'static str,
    /// Откуда строки.
    pub rows: Rows,
    /// Колонки.
    pub columns: &'static [Col],
    /// Строки подробностей выделенной записи (строки 18 и 19 экрана).
    pub detail: fn(&Value) -> [Line; 2],
    /// F-клавиши раздела.
    pub keys: &'static [Key],
}

impl Layout {
    /// Действие по F-клавише `n`.
    pub fn key(&self, n: u8) -> Option<Act> {
        self.keys.iter().find(|k| k.n == n).map(|k| k.act)
    }

    /// Подпись F-клавиши `n`.
    pub fn label(&self, n: u8) -> Option<&'static str> {
        self.keys.iter().find(|k| k.n == n).map(|k| k.label)
    }

    /// Строки экрана из ответа операции `op`.
    pub fn rows(
        &self,
        op: &crate::schema::Operation,
        value: &Value,
    ) -> (Vec<Value>, Option<(String, String)>) {
        match self.rows {
            Rows::Page => engine::page(op, value),
            Rows::Members => (members(value), None),
            Rows::Policy(put) => (policy_rows(put, value), None),
        }
    }
}

impl PartialEq for Layout {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}

impl fmt::Debug for Layout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Layout({})", self.name)
    }
}

/// Раскладка операции по умолчанию (раздел дерева, меню F9).
pub fn of(op: &str) -> Option<&'static Layout> {
    Some(match op {
        "list_accounts" => &ACCOUNTS,
        "list_app_passwords" => &APP_PASSWORDS,
        "list_groups" => &GROUPS,
        "get_group" => &MEMBERS,
        "get_org" => &POLICY_ORG,
        "get_domain_policy" => &POLICY_DOMAIN,
        "search_audit" => &AUDIT,
        "search_delivery" => &DELIVERY,
        "list_held" => &MODERATION,
        "list_trash" => &TRASH,
        "list_hidden" => &TRASH_HIDDEN,
        _ => return None,
    })
}

fn none(_: &Value) -> Map<String, Value> {
    Map::new()
}

fn toggle(row: &Value, field: &str, default: bool) -> Map<String, Value> {
    let now = row.get(field).and_then(Value::as_bool).unwrap_or(default);
    Map::from_iter([(field.to_owned(), Value::Bool(!now))])
}

fn toggle_enabled(row: &Value) -> Map<String, Value> {
    toggle(row, "enabled", true)
}

fn toggle_hidden(row: &Value) -> Map<String, Value> {
    toggle(row, "hidden_from_gal", false)
}

/// Учётные записи: блокировка, скрытие из книги, пароли приложений, псевдонимы — в правке.
pub static ACCOUNTS: Layout = Layout {
    name: "accounts",
    rows: Rows::Page,
    columns: &[
        col("Адрес", "login", 21, Fmt::Text),
        col("Имя", "display_name", 15, Fmt::Text),
        col("Квота", "quota", 6, Fmt::Quota),
        col("С", "enabled", 1, Fmt::Account),
    ],
    detail: account_detail,
    keys: &[
        Key {
            n: 5,
            label: "Блок",
            act: Act::Item {
                op: "patch_account",
                prefill: toggle_enabled,
            },
        },
        Key {
            n: 6,
            label: "Пароли",
            act: Act::Open {
                op: "list_app_passwords",
                layout: &APP_PASSWORDS,
            },
        },
        Key {
            n: 8,
            label: "Скрыть",
            act: Act::Item {
                op: "patch_account",
                prefill: toggle_hidden,
            },
        },
    ],
};

/// Пароли приложений учётной записи: выпуск F7 (значение — в карточке результата), отзыв F8.
pub static APP_PASSWORDS: Layout = Layout {
    name: "app_passwords",
    rows: Rows::Page,
    columns: &[
        col("С", "revoked_at", 1, Fmt::Revoked),
        col("Подпись", "label", 22, Fmt::Text),
        col("Выпущен", "created_at", 16, Fmt::Time),
    ],
    detail: app_password_detail,
    keys: &[Key {
        n: 8,
        label: "Отозв",
        act: Act::Item {
            op: "revoke",
            prefill: none,
        },
    }],
};

/// Группы: Enter — участники.
pub static GROUPS: Layout = Layout {
    name: "groups",
    rows: Rows::Page,
    columns: &[
        col("Имя", "name", 19, Fmt::Text),
        col("Адрес", "address", 19, Fmt::Text),
        col("Уч.", "members", 3, Fmt::Count),
        col("С", "moderators", 1, Fmt::Group),
    ],
    detail: group_detail,
    keys: &[Key {
        n: 3,
        label: "Состав",
        act: Act::Open {
            op: "get_group",
            layout: &MEMBERS,
        },
    }],
};

/// Участники группы: F7 — добавить по имени входа, F8 — убрать.
pub static MEMBERS: Layout = Layout {
    name: "members",
    rows: Rows::Members,
    columns: &[
        col("Вид", "kind", 4, Fmt::MemberKind),
        col("Участник", "name", 41, Fmt::Text),
    ],
    detail: member_detail,
    keys: &[
        Key {
            n: 7,
            label: "Добав",
            act: Act::AddMember,
        },
        Key {
            n: 8,
            label: "Убрать",
            act: Act::RemoveMember,
        },
    ],
};

const POLICY_COLUMNS: &[Col] = &[
    col("Параметр", "name", 18, Fmt::Text),
    col("Задано", "policy", 13, Fmt::Text),
    col("Действует", "effective", 13, Fmt::Text),
];

/// Политика организации: F5 — домены, F6 — ящик.
pub static POLICY_ORG: Layout = Layout {
    name: "policy_org",
    rows: Rows::Policy("put_org_policy"),
    columns: POLICY_COLUMNS,
    detail: policy_detail,
    keys: &[
        Key {
            n: 4,
            label: "Правка",
            act: Act::EditPolicy("put_org_policy"),
        },
        Key {
            n: 5,
            label: "Домены",
            act: Act::Switch {
                op: "list_domains",
                layout: &POLICY_DOMAINS,
                nest: true,
            },
        },
        Key {
            n: 6,
            label: "Ящик",
            act: Act::Account {
                op: "get_account",
                layout: &POLICY_ACCOUNT,
            },
        },
    ],
};

/// Домены организации для выбора уровня политики.
pub static POLICY_DOMAINS: Layout = Layout {
    name: "policy_domains",
    rows: Rows::Page,
    columns: &[
        col("Домен", "name", 30, Fmt::Text),
        col("Вид", "kind", 15, Fmt::Text),
    ],
    detail: domain_detail,
    keys: &[Key {
        n: 3,
        label: "Полит",
        act: Act::Open {
            op: "get_domain_policy",
            layout: &POLICY_DOMAIN,
        },
    }],
};

/// Политика домена.
pub static POLICY_DOMAIN: Layout = Layout {
    name: "policy_domain",
    rows: Rows::Policy("put_domain_policy"),
    columns: POLICY_COLUMNS,
    detail: policy_detail,
    keys: &[Key {
        n: 4,
        label: "Правка",
        act: Act::EditPolicy("put_domain_policy"),
    }],
};

/// Политика ящика.
pub static POLICY_ACCOUNT: Layout = Layout {
    name: "policy_account",
    rows: Rows::Policy("put_account_policy"),
    columns: POLICY_COLUMNS,
    detail: policy_detail,
    keys: &[Key {
        n: 4,
        label: "Правка",
        act: Act::EditPolicy("put_account_policy"),
    }],
};

/// Журнал аудита: фильтр F2, листание, F5 — журнал доставки.
pub static AUDIT: Layout = Layout {
    name: "audit",
    rows: Rows::Page,
    columns: &[
        col("Время", "at", 16, Fmt::Time),
        col("Администратор", "actor_login", 16, Fmt::Text),
        col("Метод", "method", 6, Fmt::Text),
        col("Код", "status", 5, Fmt::Status),
    ],
    detail: audit_detail,
    keys: &[Key {
        n: 5,
        label: "Достав",
        act: Act::Switch {
            op: "search_delivery",
            layout: &DELIVERY,
            nest: false,
        },
    }],
};

/// Журнал доставки: фильтр F2, листание, F5 — журнал аудита.
pub static DELIVERY: Layout = Layout {
    name: "delivery",
    rows: Rows::Page,
    columns: &[
        col("Время", "at", 16, Fmt::Time),
        col("Событие", "kind", 7, Fmt::Text),
        col("Решение", "decision", 10, Fmt::Decision),
        col("Отправитель", "sender", 10, Fmt::Text),
    ],
    detail: delivery_detail,
    keys: &[Key {
        n: 5,
        label: "Аудит",
        act: Act::Switch {
            op: "search_audit",
            layout: &AUDIT,
            nest: false,
        },
    }],
};

/// Очередь модерации: F5 — одобрить, F8 — отклонить.
pub static MODERATION: Layout = Layout {
    name: "moderation",
    rows: Rows::Page,
    columns: &[
        col("Поставлено", "held_at", 16, Fmt::Time),
        col("Отправитель", "sender", 20, Fmt::Text),
        col("Размер", "size", 8, Fmt::Bytes),
    ],
    detail: held_detail,
    keys: &[
        Key {
            n: 5,
            label: "Одобр",
            act: Act::Item {
                op: "approve",
                prefill: none,
            },
        },
        Key {
            n: 8,
            label: "Откл",
            act: Act::Item {
                op: "reject",
                prefill: none,
            },
        },
    ],
};

/// Корзина ящика: F5 — восстановить, F6 — скрытые письма.
pub static TRASH: Layout = Layout {
    name: "trash",
    rows: Rows::Page,
    columns: &[
        col("Удалено", "deleted_at", 16, Fmt::Time),
        col("Папка", "origin", 18, Fmt::Text),
        col("Размер", "size", 8, Fmt::Bytes),
    ],
    detail: trash_detail,
    keys: &[
        Key {
            n: 5,
            label: "Восст",
            act: Act::Item {
                op: "restore",
                prefill: none,
            },
        },
        Key {
            n: 6,
            label: "Скрытые",
            act: Act::Switch {
                op: "list_hidden",
                layout: &TRASH_HIDDEN,
                nest: true,
            },
        },
    ],
};

/// Скрытые письма корзины: F5 — восстановить.
pub static TRASH_HIDDEN: Layout = Layout {
    name: "trash_hidden",
    rows: Rows::Page,
    columns: TRASH.columns,
    detail: trash_detail,
    keys: &[Key {
        n: 5,
        label: "Восст",
        act: Act::Item {
            op: "restore_hidden",
            prefill: none,
        },
    }],
};

/// Текст и роль ячейки колонки `c` записи `row`.
pub fn cell(c: &Col, row: &Value) -> (String, Role) {
    let v = row.get(c.field);
    let text = || (engine::cell(v), Role::Text);
    match c.fmt {
        Fmt::Text => text(),
        Fmt::Time => (time(v), Role::Text),
        Fmt::Bytes => (
            v.and_then(Value::as_u64)
                .map_or_else(|| "—".to_owned(), bytes),
            Role::Muted,
        ),
        Fmt::Count => (
            v.and_then(Value::as_array)
                .map_or_else(|| "—".to_owned(), |a| a.len().to_string()),
            Role::Muted,
        ),
        Fmt::Quota => (
            row.pointer("/quota/limit_bytes")
                .and_then(Value::as_u64)
                .map_or_else(|| "—".to_owned(), bytes),
            Role::Muted,
        ),
        Fmt::Account => {
            if row.get("enabled").and_then(Value::as_bool) == Some(false) {
                ("×".to_owned(), Role::Err)
            } else if quota_share(row).is_some_and(|s| s >= 0.9) {
                ("▲".to_owned(), Role::Warn)
            } else {
                ("●".to_owned(), Role::Ok)
            }
        }
        Fmt::Revoked => match v {
            Some(Value::Null) | None => ("●".to_owned(), Role::Ok),
            Some(_) => ("×".to_owned(), Role::Err),
        },
        Fmt::Group => match v.and_then(Value::as_array) {
            Some(m) if !m.is_empty() => ("▲".to_owned(), Role::Warn),
            _ => ("●".to_owned(), Role::Ok),
        },
        Fmt::Status => match v.and_then(Value::as_u64) {
            Some(s) if s >= 400 => (format!("× {s}"), Role::Err),
            Some(s) => (format!("● {s}"), Role::Ok),
            None => text(),
        },
        Fmt::Decision => {
            let d = engine::cell(v);
            match d.as_str() {
                "accept" => (format!("● {d}"), Role::Ok),
                "reject" | "discard" => (format!("× {d}"), Role::Err),
                _ => (format!("▲ {d}"), Role::Warn),
            }
        }
        Fmt::MemberKind => match v.and_then(Value::as_str) {
            Some("group") => ("гр.".to_owned(), Role::Muted),
            _ => ("уч.".to_owned(), Role::Muted),
        },
    }
}

/// Момент RFC 3339 одной строкой до минут.
pub fn time(v: Option<&Value>) -> String {
    match v.and_then(Value::as_str) {
        Some(s) if s.len() >= 16 => s[..16].replace('T', " "),
        _ => engine::cell(v),
    }
}

/// Размер в байтах: Б, КБ, МБ, ГБ (десятичная запятая).
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["Б", "КБ", "МБ", "ГБ"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 || v >= 10.0 || v.fract() < 0.05 {
        format!("{:.0} {}", v, UNITS[u])
    } else {
        format!("{:.1} {}", v, UNITS[u]).replace('.', ",")
    }
}

fn quota_share(row: &Value) -> Option<f64> {
    let used = row.pointer("/quota/used_bytes")?.as_u64()?;
    let limit = row.pointer("/quota/limit_bytes")?.as_u64()?;
    (limit > 0).then(|| used as f64 / limit as f64)
}

fn s(text: impl Into<String>, role: Role) -> (String, Role) {
    (text.into(), role)
}

fn field(row: &Value, name: &str) -> String {
    engine::cell(row.get(name))
}

fn account_detail(row: &Value) -> [Line; 2] {
    let mut first = vec![s(field(row, "login"), Role::Text)];
    let name = field(row, "display_name");
    if name != "—" {
        first.push(s(format!(" · {name}"), Role::Muted));
    }
    if row.get("hidden_from_gal").and_then(Value::as_bool) == Some(true) {
        first.push(s(" · скрыта из книги", Role::Warn));
    }
    let mut second = Vec::new();
    let blocked = row.get("enabled").and_then(Value::as_bool) == Some(false);
    // Строка помещается в 46 знаков: у заблокированной — короче полоса квоты.
    let (prefix, bar) = if blocked {
        second.push(s("× заблокирована · ", Role::Err));
        ("", 8)
    } else {
        ("Занято ", 16)
    };
    let used = row.pointer("/quota/used_bytes").and_then(Value::as_u64);
    match (used, quota_share(row)) {
        (Some(u), Some(share)) => {
            let limit = row
                .pointer("/quota/limit_bytes")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            second.push(s(
                format!("{prefix}{} из {} ", bytes(u), bytes(limit)),
                Role::Muted,
            ));
            let filled = ((share.min(1.0) * f64::from(bar)).round()) as usize;
            let role = if share >= 0.9 {
                Role::Warn
            } else {
                Role::FrameA
            };
            second.push(s("█".repeat(filled), role));
            second.push(s("░".repeat(bar as usize - filled), Role::Frame));
            second.push(s(format!(" {:.0} %", share * 100.0), Role::Text));
        }
        (Some(u), None) => second.push(s(format!("Занято {} · без квоты", bytes(u)), Role::Muted)),
        _ => {}
    }
    let aliases = field(row, "aliases");
    if second.is_empty() && aliases != "—" && !aliases.is_empty() {
        second.push(s(format!("псевдонимы: {aliases}"), Role::Muted));
    }
    [first, second]
}

fn app_password_detail(row: &Value) -> [Line; 2] {
    let state = match row.get("revoked_at") {
        Some(Value::Null) | None => s("● действует", Role::Ok),
        Some(v) => s(format!("× отозван {}", time(Some(v))), Role::Err),
    };
    [
        vec![s(field(row, "label"), Role::Text)],
        vec![state, s(" · F7 — выпустить новый", Role::Muted)],
    ]
}

fn group_detail(row: &Value) -> [Line; 2] {
    let mut first = vec![s(field(row, "name"), Role::Text)];
    let address = field(row, "address");
    if address != "—" {
        first.push(s(format!(" · {address}"), Role::Muted));
    }
    if row.get("external_id").is_some_and(|v| !v.is_null()) {
        first.push(s(" · из каталога", Role::Muted));
    }
    let count = |n: &str| row.get(n).and_then(Value::as_array).map_or(0, Vec::len);
    let mut second = vec![s(
        format!(
            "участников: {} · модераторов: {}",
            count("members"),
            count("moderators")
        ),
        Role::Muted,
    )];
    if row.get("hidden_from_gal").and_then(Value::as_bool) == Some(true) {
        second.push(s(" · скрыта из книги", Role::Warn));
    }
    [first, second]
}

fn member_detail(row: &Value) -> [Line; 2] {
    [
        vec![
            s(field(row, "name"), Role::Text),
            s(format!(" · {}", field(row, "id")), Role::Muted),
        ],
        vec![s("F7 — добавить по имени входа · F8 — убрать", Role::Muted)],
    ]
}

fn policy_detail(row: &Value) -> [Line; 2] {
    let set = row.get("policy").is_some_and(|v| !v.is_null());
    [
        vec![s(field(row, "description"), Role::Muted)],
        vec![if set {
            s("задано на этом уровне · F4 — правка", Role::Text)
        } else {
            s(
                "не задано: действует вышестоящее · F4 — правка",
                Role::Muted,
            )
        }],
    ]
}

fn domain_detail(row: &Value) -> [Line; 2] {
    [
        vec![s(field(row, "name"), Role::Text)],
        vec![s("Enter — политика домена · Esc — назад", Role::Muted)],
    ]
}

fn audit_detail(row: &Value) -> [Line; 2] {
    let route = row
        .get("path")
        .filter(|v| !v.is_null())
        .or_else(|| row.get("route"));
    [
        vec![s(
            format!("{} {}", field(row, "method"), engine::cell(route)),
            Role::Text,
        )],
        vec![s(format!("роли: {}", field(row, "roles")), Role::Muted)],
    ]
}

fn delivery_detail(row: &Value) -> [Line; 2] {
    let mut second = vec![s(field(row, "message_id"), Role::Muted)];
    let reason = field(row, "reason");
    if reason != "—" {
        second.push(s(format!(" · {reason}"), Role::Muted));
    }
    [
        vec![s(
            format!("{} → {}", field(row, "sender"), field(row, "recipients")),
            Role::Text,
        )],
        second,
    ]
}

fn held_detail(row: &Value) -> [Line; 2] {
    let subject = match row.get("subject") {
        Some(Value::String(t)) => s(t.clone(), Role::Text),
        _ => s(
            "тема — фильтр F2: include = subject (в журнал аудита)",
            Role::Muted,
        ),
    };
    let source = match row.get("rule").and_then(Value::as_str) {
        Some(rule) => format!("правило потока «{rule}»"),
        None => format!("группа {}", field(row, "group")),
    };
    [
        vec![subject],
        vec![s(
            format!("{source} · кому: {}", field(row, "recipients")),
            Role::Muted,
        )],
    ]
}

fn trash_detail(row: &Value) -> [Line; 2] {
    let first = match (row.get("subject"), row.get("from")) {
        (Some(Value::String(t)), from) => vec![
            s(t.clone(), Role::Text),
            s(format!(" · {}", engine::cell(from)), Role::Muted),
        ],
        _ => vec![s(
            format!(
                "письмо {} · тема — F2: include = subject",
                field(row, "message")
            ),
            Role::Muted,
        )],
    };
    [
        first,
        vec![s(
            format!(
                "удаляется окончательно не раньше {}",
                time(row.get("expires_at"))
            ),
            Role::Muted,
        )],
    ]
}

/// Строки участников группы: вид, идентификатор, имя (из `names`, нет — идентификатор).
fn members(group: &Value) -> Vec<Value> {
    let names = group.get("names");
    group
        .get("members")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let (kind, id) = m.as_object()?.iter().next()?;
            let id = id.as_str()?;
            let name = names
                .and_then(|n| n.get(id))
                .and_then(Value::as_str)
                .unwrap_or(id);
            Some(serde_json::json!({"kind": kind, "id": id, "name": name}))
        })
        .collect()
}

/// Участники группы без участника `id` — для `patch_group`.
pub fn members_without(group: &Value, id: &str) -> Vec<Value> {
    group
        .get("members")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|m| {
            m.as_object()
                .and_then(|o| o.values().next())
                .and_then(Value::as_str)
                != Some(id)
        })
        .cloned()
        .collect()
}

/// Строки политики уровня: параметры тела операции `put` со значениями — заданным и действующим.
fn policy_rows(put: &str, record: &Value) -> Vec<Value> {
    let Some(body) = catalog().get(put).and_then(|o| o.body.as_ref()) else {
        return Vec::new();
    };
    body.fields
        .iter()
        .map(|f| {
            let at = |level: &str| {
                let v = record.pointer(&format!("/{level}/{}", f.name)).cloned();
                match v {
                    Some(Value::Number(n)) if f.name.ends_with("_bytes") => n
                        .as_u64()
                        .map_or(Value::Number(n), |b| Value::String(bytes(b))),
                    other => other.unwrap_or(Value::Null),
                }
            };
            serde_json::json!({
                "name": f.name,
                "policy": at("policy"),
                "effective": at("effective"),
                "description": f.description,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Операции и поля раскладок есть в схеме Admin API; ширина колонок помещается в правую панель.
    #[test]
    fn layouts_match_spec() {
        let all: [&Layout; 13] = [
            &ACCOUNTS,
            &APP_PASSWORDS,
            &GROUPS,
            &MEMBERS,
            &POLICY_ORG,
            &POLICY_DOMAINS,
            &POLICY_DOMAIN,
            &POLICY_ACCOUNT,
            &AUDIT,
            &DELIVERY,
            &MODERATION,
            &TRASH,
            &TRASH_HIDDEN,
        ];
        let c = catalog();
        for l in all {
            let width: u16 = l.columns.iter().map(|c| c.width + 1).sum::<u16>() - 1;
            assert!(width <= 46, "{}: ширина {width}", l.name);
            if let Rows::Policy(put) = l.rows {
                assert!(c.get(put).is_some(), "{}: нет {put}", l.name);
            }
            for k in l.keys {
                assert!(k.label.chars().count() <= 7, "{}: {}", l.name, k.label);
                let op = match k.act {
                    Act::Item { op, .. }
                    | Act::Open { op, .. }
                    | Act::Switch { op, .. }
                    | Act::Account { op, .. }
                    | Act::EditPolicy(op) => op,
                    Act::AddMember | Act::RemoveMember => "patch_group",
                };
                assert!(c.get(op).is_some(), "{}: нет операции {op}", l.name);
            }
        }
        for (op, l) in [
            ("list_accounts", &ACCOUNTS),
            ("list_groups", &GROUPS),
            ("search_audit", &AUDIT),
            ("search_delivery", &DELIVERY),
            ("list_held", &MODERATION),
            ("list_trash", &TRASH),
            ("list_app_passwords", &APP_PASSWORDS),
        ] {
            let Some(crate::schema::Response::List { columns, .. }) =
                c.get(op).map(|o| &o.response)
            else {
                panic!("{op}: не список");
            };
            let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
            for col in l.columns {
                assert!(names.contains(&col.field), "{op}: нет поля {}", col.field);
            }
        }
    }

    #[test]
    fn cells_and_signs() {
        let row = json!({"login": "a@b", "enabled": false,
                         "quota": {"used_bytes": 95, "limit_bytes": 100, "messages": 1}});
        let state = ACCOUNTS.columns[3];
        assert_eq!(cell(&state, &row), ("×".to_owned(), Role::Err));
        let full = json!({"enabled": true, "quota": {"used_bytes": 95, "limit_bytes": 100}});
        assert_eq!(cell(&state, &full).0, "▲");
        assert_eq!(
            cell(
                &ACCOUNTS.columns[2],
                &json!({"quota": {"limit_bytes": 5_368_709_120u64}})
            )
            .0,
            "5 ГБ"
        );
        assert_eq!(bytes(1_610_612_736), "1,5 ГБ");
        assert_eq!(bytes(512), "512 Б");
        assert_eq!(
            time(Some(&json!("2026-11-14T09:30:15Z"))),
            "2026-11-14 09:30"
        );
        assert_eq!(cell(&AUDIT.columns[3], &json!({"status": 409})).0, "× 409");
        assert_eq!(
            cell(&DELIVERY.columns[2], &json!({"decision": "defer"})).0,
            "▲ defer"
        );
    }

    #[test]
    fn members_and_policy_rows() {
        let g = json!({"members": [{"account": "a1"}, {"group": "g2"}], "names": {"a1": "ivanova@it-task.ru"}});
        let rows = members(&g);
        assert_eq!(rows[0]["name"], "ivanova@it-task.ru");
        assert_eq!(rows[1]["kind"], "group");
        assert_eq!(rows[1]["name"], "g2");
        assert_eq!(members_without(&g, "a1"), vec![json!({"group": "g2"})]);
        let org = json!({"policy": {"quota_bytes": 1_073_741_824u64}, "effective": {"quota_bytes": 1_073_741_824u64, "send_per_hour": 100}});
        let rows = policy_rows("put_org_policy", &org);
        let quota = rows
            .iter()
            .find(|r| r["name"] == "quota_bytes")
            .expect("quota_bytes");
        assert_eq!(quota["policy"], "1 ГБ");
        let send = rows
            .iter()
            .find(|r| r["name"] == "send_per_hour")
            .expect("send_per_hour");
        assert_eq!(
            (send["policy"].clone(), send["effective"].clone()),
            (Value::Null, json!(100))
        );
    }
}
