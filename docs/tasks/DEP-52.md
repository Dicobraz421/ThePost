# DEP-52: стенд: доверенные подсети вместо выключенного ограничения частоты

lane: deploy
deps: [PLT-32]
class: A
files: [deploy/stand/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [none]
manual: yes

## Сделать
- После беты с PLT-32 (#601, #608): `deploy/stand/thresholds.yml` — `ratelimit.trusted: [подсеть генератора]` вместо
  `burst: 0` для всех (решение 109); временное решение удалить.

## Приёмка
- Прогон Д.6 на стенде: генератор не упирается в ограничение, перебор паролей с доверенного адреса блокируется.
