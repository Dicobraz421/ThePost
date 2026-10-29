# DEP-02: systemd и пример конфигурации SMB

lane: deploy
deps: []
class: A
files: [deploy/systemd/*, deploy/config/*]
read: [docs/spec/S1.md#1.2, docs/spec/S5.md#5.5]
checks: [none]
manual: no

## Сделать
- Юнит systemd (hardening: ProtectSystem, NoNewPrivileges, CapabilityBoundingSet для портов <1024), config.smb.yaml, config.enterprise.yaml.

## Приёмка
- systemd-analyze verify проходит.
