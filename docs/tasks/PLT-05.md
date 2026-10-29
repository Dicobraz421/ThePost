# PLT-05: Рантайм ролей

lane: platform
deps: [PLT-02, PLT-04]
class: A
files: [crates/tp-platform/src/roles.rs]
read: [docs/spec/S2.md#2.1, docs/CONTRACTS.md#Roles]
checks: [nextest]
manual: no

## Сделать
- Запуск набора RoleService по конфигурации; SIGTERM → drain: снять ready, ждать до drain_deadline, затем отмена.

## Приёмка
- Тест: роль завершается в пределах дедлайна; зависшая роль отменяется.
