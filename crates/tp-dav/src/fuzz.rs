//! Проверки разбора DAV на произвольных данных (DAV-02; SRS 6.8): цель cargo-fuzz `crates/tp-dav/fuzz` и тесты
//! крейта на примерах. Функция паникует, только если нарушен инвариант разбора.
//!
//! - [`xml`] — тело запроса: дерево не глубже и не больше лимитов, запросы `PROPFIND`, `PROPPATCH` и
//!   `sync-collection` из него разбираются без паники; имена свойств `prop` — без повторов.

use std::collections::BTreeSet;

use crate::props::{Want, keys, updates};
use crate::sync::SyncRequest;
use crate::xml::{self, XmlLimits};

/// Лимиты разбора в цели (меньше конфигурации по умолчанию: важна граница, не значение).
const LIMITS: XmlLimits = XmlLimits {
    max_depth: 16,
    max_elements: 2_000,
};

/// Тело запроса DAV.
pub fn xml(data: &[u8]) {
    let Ok(root) = xml::parse(data, LIMITS) else {
        return;
    };
    assert!(root.depth() <= LIMITS.max_depth, "глубина больше лимита");
    assert!(
        root.count() <= LIMITS.max_elements,
        "элементов больше лимита"
    );
    if let Some(Want::Props(names)) = Want::parse(Some(&root)) {
        let uniq: BTreeSet<_> = names.iter().collect();
        assert_eq!(uniq.len(), names.len(), "повтор свойства");
    }
    if let Some(ops) = updates(&root) {
        assert!(ops.len() < root.count(), "операций больше элементов");
    }
    if let Ok(req) = SyncRequest::parse(&root) {
        assert!(req.limit.is_none_or(|l| l >= 1), "limit 0");
        assert!(req.props.len() < root.count());
    }
    let _ = keys(&root);
}

#[cfg(test)]
mod tests {
    #[test]
    fn samples() {
        let samples: [&[u8]; 6] = [
            br#"<d:propfind xmlns:d="DAV:"><d:prop><d:getetag/><d:getetag/></d:prop></d:propfind>"#,
            br#"<d:propertyupdate xmlns:d="DAV:"><d:set><d:prop><d:displayname>x</d:displayname></d:prop></d:set><d:remove><d:prop><d:x/></d:prop></d:remove></d:propertyupdate>"#,
            br#"<d:sync-collection xmlns:d="DAV:"><d:sync-token/><d:limit><d:nresults>0</d:nresults></d:limit></d:sync-collection>"#,
            b"<a><b>",
            b"<!DOCTYPE a><a/>",
            &[0xff, 0x3c, 0x61],
        ];
        for s in samples {
            super::xml(s);
        }
        let deep = format!("{}{}", "<a>".repeat(40), "</a>".repeat(40));
        super::xml(deep.as_bytes());
    }
}
