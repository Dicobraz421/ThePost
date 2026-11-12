//! Входы ночного фаззинга, на которых разбор падал (FIX-06, FIX-07): файлы `crash-*` в корпусах целей
//! `fuzz/corpus/<цель>` (в git, начальный корпус фаззинга) — проверки [`crate::fuzz`] на них без паники.

/// FIX-06, issue #475: цель `meta`.
#[test]
fn meta_crash_inputs() {
    crate::fuzz::meta(include_bytes!(
        "../fuzz/corpus/meta/crash-39bf64e2208e84fcb2f2c3fc8d00d3b4e3eec17a"
    ));
    crate::fuzz::meta(include_bytes!(
        "../fuzz/corpus/meta/crash-442ac6fb4faa2aac36eeb5db2892881ec03c5ed8"
    ));
}

