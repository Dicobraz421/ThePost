//! Дерево ресурсов левой панели: разделы организации (по утверждённому макету) и операции Admin API, которыми они
//! открываются. Прочие операции — в меню F9 (строится по схеме).

/// Раздел организации.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Section {
    /// Подпись.
    pub label: &'static str,
    /// `operationId` операции, открывающей раздел.
    pub op: &'static str,
}

const fn section(label: &'static str, op: &'static str) -> Section {
    Section { label, op }
}

/// Разделы организации по порядку.
pub const SECTIONS: [Section; 11] = [
    section("Учётные записи", "list_accounts"),
    section("Домены", "list_domains"),
    section("Группы", "list_groups"),
    section("Контакты", "list_contacts"),
    section("Оргструктура", "list_units"),
    section("Ресурсы", "list_resources"),
    section("Политики", "get_org"),
    section("Журналы", "search_audit"),
    section("Модерация", "list_held"),
    section("Корзина", "list_trash"),
    section("Очередь", "list_queue"),
];

/// Подпись раздела конфигурации узла.
pub const NODE_CONFIG: &str = "Конфигурация узла";

/// Сколько строк дерева отведено другим организациям (строки между разделами и разделителем).
pub const ORG_ROWS: usize = 3;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::catalog;

    #[test]
    fn sections_open_existing_operations() {
        for s in SECTIONS {
            assert!(
                catalog().get(s.op).is_some(),
                "{}: нет операции {}",
                s.label,
                s.op
            );
        }
    }
}
