# DEP-46: эталон: только сокеты systemd, без CAP_NET_BIND_SERVICE

lane: deploy
deps: []
class: A
files: [deploy/*, docs/manual-tests/DEP-08.md]
read: [docs/decisions/2026-11-11-stage2-followups.md, docs/tasks/SYS-10.md]
checks: [none]
manual: no

Аудит 12.11 «старый путь рядом с новым не оставлять» (решение 93), п. 10; решение 102.

## Сделать
- Роль `thepost`: сокеты systemd всегда (переключатель `thepost_socket_activation` удалить), drop-in
  `bind-capability.conf` и ветку CAP удалить; `deploy/README.md`; `docs/manual-tests/DEP-08.md` без `TODO(PLT-16)`.

## Приёмка
- Стенд: узел слушает 25/143/443 без capabilities, `doctor` — `privileges.capabilities` OK.
