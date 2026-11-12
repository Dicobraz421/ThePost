# DEP-20: эталон Ansible на РЕД ОС 8 и семействе RHEL (AlmaLinux 9 и 10)

lane: deploy
deps: [DEP-15, DEP-17]
class: A
files: [deploy/ansible/*, docs/PREREQUISITES.md, docs/INSTALL.md]
read: [docs/decisions/2026-11-11-stage2-followups.md, docs/PREREQUISITES.md]
checks: [none]
manual: yes

## Сделать
- Роли работают на РЕД ОС 8 и AlmaLinux 9 и 10 — совместимость с RHEL 9 и 10 (семейство RHEL: dnf, SELinux, firewalld; решение 52): репозитории и имена пакетов, PostgreSQL,
  OpenBao и Rspamd (пакеты дистрибутива или официальные), политика SELinux для службы и портов, firewalld (решение 28).
- Совместимость выпуска: glibc дистрибутива ≥ требуемой бинарным файлом.
- Строки в `docs/PREREQUISITES.md` и `docs/INSTALL.md`.

## Приёмка
- `smb.yml` на чистых РЕД ОС 8, AlmaLinux 9 и 10 — `failed=0`, `thepost doctor` без `FAIL`, письмо между двумя пользователями —
  `docs/manual-tests/DEP-20.md`.
