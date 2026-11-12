# META-76: убрать старые PgMeta::policy/set_policy и реэкспорт PolicyTarget

lane: meta
deps: [META-70]
class: A
files: [crates/tp-meta/src/*, crates/thepost/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

## Сделать
- После META-70 (#537) остались старые `PgMeta::policy`/`set_policy` рядом с `PolicyStore` и реэкспорт
  `tp_meta::retention::PolicyTarget` — правило 12.11: старый путь рядом с новым не оставлять. Перевести всех вызывающих
  (узел, `retention`) на `PolicyStore` и типы контракта, старое удалить. Две полосы (meta + место вызова в thepost) —
  если без этого не собирается, вести веткой `x/…` как класс B.

## Приёмка
- `grep -rn "fn set_policy\|retention::PolicyTarget" crates/` — пусто; тесты политик на PostgreSQL проходят.
