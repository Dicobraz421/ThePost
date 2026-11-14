//! Дерево ресурсов левой панели: разделы организации (по утверждённому макету) и операции Admin API, которыми они
//! открываются. Прочие операции — в меню F9 (строится по схеме).

/// Раздел организации.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Section {
    /// Подпись.
    pub label: &'static str,
    /// `operationId` операции, открывающей раздел; `None` — в Admin API операции пока нет.
    pub op: Option<&'static str>,
}

/// Разделы организации по порядку.
pub const SECTIONS: [Section; 11] = [
    Section {
        label: "Учётные записи",
        op: Some("list_accounts"),
    },
    Section {
        label: "Домены",
        op: Some("list_domains"),
    },
    Section {
        label: "Группы",
        op: Some("list_groups"),
    },
    Section {
        label: "Контакты",
        op: Some("list_contacts"),
    },
    Section {
        label: "Оргструктура",
        op: Some("list_units"),
    },
    Section {
        label: "Ресурсы",
        op: Some("list_resources"),
    },
    Section {
        label: "Политики",
        op: Some("get_org"),
    },
    Section {
        label: "Журналы",
        op: Some("search_audit"),
    },
    Section {
        label: "Модерация",
        op: Some("list_held"),
    },
    Section {
        label: "Корзина",
        op: Some("list_trash"),
    },
    Section {
        label: "Очередь",
        op: None,
    },
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
            if let Some(id) = s.op {
                assert!(
                    catalog().get(id).is_some(),
                    "{}: нет операции {id}",
                    s.label
                );
            }
        }
    }
}
