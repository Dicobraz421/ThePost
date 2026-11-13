# DEP-55: метка «управляется Ansible/Helm» в config.yaml

lane: deploy
deps: []
class: A
files: [deploy/*]
read: [docs/decisions/2026-11-11-stage2-followups.md#118]
checks: [lint]
manual: no

## Сделать
- Шаблоны Ansible и Helm пишут в начало `config.yaml` машинно-читаемую метку (например
  `# thepost: managed-by=ansible`), по которой текстовый интерфейс открывает файл только на чтение (TUI-04).

## Приёмка
- Сгенерированный `config.yaml` в обоих путях содержит метку; `thepost config check` её не трогает.
