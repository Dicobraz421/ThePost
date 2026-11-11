# DEP-23: резервная копия OpenBao — снимок Raft

lane: deploy
deps: [DEP-15]
class: A
files: [deploy/ansible/roles/openbao/*, deploy/ansible/roles/pgbackrest/*, deploy/ansible/restore.yml, deploy/ansible/inventory.example.yml, docs/manual-tests/DEP-23.md]
read: [docs/decisions/2026-11-11-stage2-followups.md, docs/manual-tests/DEP-15.md]
checks: [none]
manual: yes

## Сделать
- Снимок `bao operator raft snapshot save` по systemd timer рядом с копией СУБД, в то же хранилище копий; токен
  с политикой только на снимок — из OpenBao, не в инвентаре (решение 39).
- `restore.yml`: восстановление снимка на новой ВМ (`snapshot restore -force`) до восстановления СУБД; распечатывание —
  ключами, которые администратор хранит вне сервера (инструкция в `docs/INSTALL.md` — отдельным PR полосы docs).

## Приёмка
- Новая ВМ: снимок + копия СУБД → узел стартует, DKIM подписывает тем же ключом — `docs/manual-tests/DEP-23.md`.
