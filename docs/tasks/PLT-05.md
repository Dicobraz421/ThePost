# PLT-05: Рантайм ролей

lane: platform
deps: [PLT-02, PLT-04]
class: A
files: [crates/tp-platform/src/roles.rs]
read: [docs/spec/S2.md#2.1, docs/spec/S2.md#2.7, docs/CONTRACTS.md#Roles]
checks: [nextest]
manual: no

## Сделать
- Запуск набора RoleService по конфигурации; ленивая инициализация — невключённая роль не создаёт пулов, кэшей, потоков, портов;
- лимиты кэшей и пулов по ролям от ресурсов узла (SYS-02); SIGTERM → drain: снять ready, ждать до drain_deadline, затем отмена.

## Приёмка
- Тест: роль завершается в пределах дедлайна; зависшая роль отменяется; метрика RSS процесса экспортируется (цель ≤ 64 MB на роль без нагрузки — SRS 2.7).
