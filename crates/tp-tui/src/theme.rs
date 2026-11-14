//! Роли оформления экрана. Заглушка TUI-01: 16 цветов терминала и монохром (`NO_COLOR`); тему по токенам
//! `design/tokens.toml` (пять режимов, решение 119) делает TUI-06 — роли те же.

use ratatui::style::{Color, Modifier, Style};

/// Роль ячейки экрана (как в `design/tokens.toml`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Фон экрана.
    Screen,
    /// Фон панели.
    Panel,
    /// Текст.
    Text,
    /// Вторичный текст.
    Muted,
    /// Рамка неактивной панели.
    Frame,
    /// Рамка активной панели.
    FrameA,
    /// Заголовок активной панели.
    TitleA,
    /// Заголовки колонок.
    Head,
    /// Выделенная строка активной панели.
    Sel,
    /// Выделенная строка неактивной панели.
    SelI,
    /// Номер F-клавиши.
    FkNum,
    /// Подпись F-клавиши.
    Fk,
    /// Строка меню.
    Menu,
    /// Знак `$` строки команды.
    Prompt,
    /// Команда CLI.
    Cmd,
    /// Норма ●.
    Ok,
    /// Внимание ▲.
    Warn,
    /// Ошибка ×.
    Err,
}

/// Тема: монохром или 16 цветов.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    /// Без цвета (`NO_COLOR`): только атрибуты.
    pub mono: bool,
}

impl Theme {
    /// Режим по окружению: `NO_COLOR` задан и не пуст — монохром (<https://no-color.org>).
    pub fn from_env() -> Self {
        Self {
            mono: std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()),
        }
    }

    /// Стиль роли.
    pub fn style(self, role: Role) -> Style {
        let s = Style::default();
        if self.mono {
            return match role {
                Role::Sel | Role::Fk | Role::Menu => s.add_modifier(Modifier::REVERSED),
                Role::SelI => s.add_modifier(Modifier::UNDERLINED),
                Role::TitleA | Role::Head | Role::FkNum | Role::Prompt => {
                    s.add_modifier(Modifier::BOLD)
                }
                Role::Muted => s.add_modifier(Modifier::DIM),
                Role::Err | Role::Warn => s.add_modifier(Modifier::BOLD),
                _ => s,
            };
        }
        match role {
            Role::Screen | Role::Panel => s.bg(Color::Reset),
            Role::Text | Role::Cmd => s.fg(Color::Reset),
            Role::Muted | Role::Frame => s.fg(Color::DarkGray),
            Role::FrameA => s.fg(Color::Blue),
            Role::TitleA => s.fg(Color::LightMagenta).add_modifier(Modifier::BOLD),
            Role::Head => s.fg(Color::LightBlue).add_modifier(Modifier::BOLD),
            Role::Sel => s
                .bg(Color::Magenta)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
            Role::SelI => s.bg(Color::DarkGray).fg(Color::White),
            Role::FkNum => s.add_modifier(Modifier::BOLD),
            Role::Fk => s.bg(Color::Blue).fg(Color::White),
            Role::Menu => s.bg(Color::Blue).fg(Color::White),
            Role::Prompt => s.fg(Color::LightMagenta).add_modifier(Modifier::BOLD),
            Role::Ok => s.fg(Color::Green),
            Role::Warn => s.fg(Color::Yellow).add_modifier(Modifier::BOLD),
            Role::Err => s.fg(Color::LightRed).add_modifier(Modifier::BOLD),
        }
    }
}
