# DEP-08: минимальные привилегии в эталонной поставке

lane: deploy
deps: [DEP-02, DEP-07, META-23, PLT-16]
class: A
files: [deploy/*]
read: [docs/decisions/2026-11-01-borrowings-security.md, docs/spec/S2.md#2.7, docs/spec/S6.md#6.8]
checks: [nextest]
manual: docs/manual-tests/DEP-08.md

## Сделать
- Unit systemd: активация сокетов для 25, 465, 587, 993, полный hardening (NoNewPrivileges, ProtectSystem=strict, SystemCallFilter, пустой CapabilityBoundingSet и др.).
- Helm: runAsNonRoot, readOnlyRootFilesystem, без capabilities, seccomp.
- Роли PostgreSQL (владелец схемы, приложение, только чтение) в compose и Ansible; Admin API — на внутреннем адресе.

## Приёмка
- `systemd-analyze security thepost.service` — оценка не хуже 2.0; doctor без предупреждений о привилегиях.
