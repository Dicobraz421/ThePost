//! Консистентное хэширование учётных записей по узлам: rendezvous (HRW), SRS 4.6.
//!
//! Узел получает для ключа вес `weight(узел, ключ)`; владелец ключа — узел с наибольшим весом.
//! При удалении узла владельца меняют только его ключи, при добавлении — только ключи, которые
//! забирает новый узел. Функция веса зафиксирована тестом: все узлы кластера считают одинаково.

use tp_contracts::AccountId;

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Вес узла для ключа: FNV-1a по `узел ‖ 0xff ‖ ключ` с финализатором splitmix64.
///
/// Байт `0xff` не встречается в UTF-8, поэтому пары (`"ab"`, `"c"`) и (`"a"`, `"bc"`) различаются.
pub fn weight(node: &str, key: &[u8]) -> u64 {
    let mut h = FNV_OFFSET;
    for &b in node.as_bytes().iter().chain(&[0xff]).chain(key) {
        h ^= u64::from(b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    // FNV плохо перемешивает старшие биты на коротких входах — добиваем финализатором.
    h ^= h >> 30;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^ (h >> 31)
}

/// Состав кластера для распределения ключей (учётных записей) по узлам.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rendezvous {
    /// Имена узлов, отсортированы, без повторов.
    nodes: Vec<String>,
}

impl Rendezvous {
    /// Состав из имён узлов; порядок и повторы не важны.
    pub fn new<I, S>(nodes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut nodes: Vec<String> = nodes.into_iter().map(Into::into).collect();
        nodes.sort();
        nodes.dedup();
        Self { nodes }
    }

    /// Узлы состава по возрастанию имени.
    pub fn nodes(&self) -> &[String] {
        &self.nodes
    }

    /// Добавляет узел; `false`, если он уже есть.
    pub fn insert(&mut self, node: impl Into<String>) -> bool {
        let node = node.into();
        match self.nodes.binary_search(&node) {
            Ok(_) => false,
            Err(at) => {
                self.nodes.insert(at, node);
                true
            }
        }
    }

    /// Убирает узел; `false`, если его не было.
    pub fn remove(&mut self, node: &str) -> bool {
        match self.nodes.binary_search_by(|n| n.as_str().cmp(node)) {
            Ok(at) => {
                self.nodes.remove(at);
                true
            }
            Err(_) => false,
        }
    }

    /// Узел-владелец учётной записи; `None` при пустом составе.
    pub fn owner(&self, account: AccountId) -> Option<&str> {
        self.owner_of(account.as_uuid().as_bytes())
    }

    /// Узел-владелец произвольного ключа; `None` при пустом составе.
    pub fn owner_of(&self, key: &[u8]) -> Option<&str> {
        self.ranked(key).next().map(|(_, node)| node)
    }

    /// До `n` узлов по убыванию веса: владелец, затем резервные (при отказе владельца ключ
    /// переходит к следующему в этом порядке).
    pub fn owners(&self, account: AccountId, n: usize) -> Vec<&str> {
        self.ranked(account.as_uuid().as_bytes())
            .take(n)
            .map(|(_, node)| node)
            .collect()
    }

    /// Узлы по убыванию веса; при равном весе раньше идёт меньшее имя.
    fn ranked(&self, key: &[u8]) -> impl Iterator<Item = (u64, &str)> {
        let mut ranked: Vec<(u64, &str)> = self
            .nodes
            .iter()
            .map(|n| (weight(n, key), n.as_str()))
            .collect();
        ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
        ranked.into_iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accounts(n: usize) -> Vec<AccountId> {
        (0..n).map(|_| AccountId::new()).collect()
    }

    #[test]
    fn weight_is_pinned() {
        // Значения зафиксированы: смена функции веса перетасует ящики по узлам всего кластера.
        assert_eq!(weight("node-a", b"key"), 0xd7d1_8edc_f8a1_594e);
        assert_eq!(weight("", b""), 0x4f2d_b50f_1240_40cd);
        assert_ne!(weight("ab", b"c"), weight("a", b"bc"));
    }

    #[test]
    fn owner_is_stable_and_order_independent() {
        let a = Rendezvous::new(["n1", "n2", "n3", "n2"]);
        let b = Rendezvous::new(["n3", "n1", "n2"]);
        assert_eq!(a, b);
        assert_eq!(a.nodes().len(), 3);
        for acc in accounts(200) {
            assert_eq!(a.owner(acc), b.owner(acc));
            assert_eq!(a.owners(acc, 3)[0], a.owner(acc).expect("owner"));
        }
        assert_eq!(Rendezvous::default().owner(AccountId::new()), None);
    }

    #[test]
    fn removing_node_moves_only_its_keys() {
        let mut ring = Rendezvous::new(["n1", "n2", "n3", "n4", "n5"]);
        let keys = accounts(5000);
        let before: Vec<String> = keys
            .iter()
            .map(|&k| ring.owner(k).expect("owner").to_owned())
            .collect();
        assert!(ring.remove("n3"));
        assert!(!ring.remove("n3"));
        let mut moved = 0;
        for (k, old) in keys.iter().zip(&before) {
            let new = ring.owner(*k).expect("owner");
            if old == "n3" {
                assert_ne!(new, "n3");
                moved += 1;
            } else {
                assert_eq!(new, old, "ключ чужого узла переехал");
            }
        }
        assert!(moved > 0);
    }

    #[test]
    fn adding_node_moves_keys_only_to_it() {
        let mut ring = Rendezvous::new(["n1", "n2", "n3", "n4"]);
        let keys = accounts(5000);
        let before: Vec<String> = keys
            .iter()
            .map(|&k| ring.owner(k).expect("owner").to_owned())
            .collect();
        assert!(ring.insert("n5"));
        assert!(!ring.insert("n5"));
        let mut moved = 0;
        for (k, old) in keys.iter().zip(&before) {
            let new = ring.owner(*k).expect("owner");
            if new != old {
                assert_eq!(new, "n5");
                moved += 1;
            }
        }
        // Новый узел забирает около 1/5 ключей.
        assert!((700..1300).contains(&moved), "moved = {moved}");
    }

    #[test]
    fn distribution_is_even() {
        let ring = Rendezvous::new(["n1", "n2", "n3", "n4", "n5"]);
        let mut counts = std::collections::HashMap::new();
        for k in accounts(10_000) {
            *counts
                .entry(ring.owner(k).expect("owner"))
                .or_insert(0usize) += 1;
        }
        for (node, c) in counts {
            assert!((1600..2400).contains(&c), "{node}: {c}");
        }
    }

    #[test]
    fn backup_takes_over_on_owner_failure() {
        let mut ring = Rendezvous::new(["n1", "n2", "n3"]);
        for acc in accounts(500) {
            let ranked: Vec<String> = ring.owners(acc, 2).into_iter().map(str::to_owned).collect();
            assert_eq!(ranked.len(), 2);
            ring.remove(&ranked[0]);
            assert_eq!(ring.owner(acc), Some(ranked[1].as_str()));
            ring.insert(ranked[0].clone());
        }
    }
}
