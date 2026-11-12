# DEP-32: «малый Enterprise» на 5 ВМ и отказ одной ВМ

lane: deploy
deps: [DEP-10, DEP-31]
class: A
files: [deploy/ansible/*, docs/manual-tests/DEP-10.md]
read: [docs/manual-tests/DEP-10.md]
checks: [none]
manual: yes

Хвост волны deploy-07 (DEP-10), PR #398: отказ ВМ на стенде не выполнялся (нужно 5 ВМ).

## Сделать
- Полный прогон раздела «Ansible „Малый Enterprise“: стенд (5 ВМ)» `docs/manual-tests/DEP-10.md`: отказ узла mailbox,
  ВМ данных (primary СУБД, лидер OpenBao, SeaweedFS) — почта принимается и читается, переключение СУБД ≤ 30 с.

## Приёмка
- Итоги в `docs/manual-tests/DEP-10.md` без «не прошёл».
