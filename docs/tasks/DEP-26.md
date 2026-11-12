# DEP-26: юнит systemd — растущая пауза перезапуска

lane: deploy
deps: [APP-31]
class: A
files: [deploy/systemd/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

Хвост волны app-30, PR #436.

Требует подтверждения владельца: растущая пауза перезапуска 10 с → 5 мин без предела попыток (рекомендация агента), а не постоянные 30 с? (PR #436, решение 2)

## Сделать
- `deploy/systemd/thepost.service` (вторая половина APP-31, решение 58): `[Service]` — `RestartSec=10s`,
  `RestartSteps=6`, `RestartMaxDelaySec=5min` (пауза растёт с 10 с до 5 мин; нужен systemd 254+, на старых —
  постоянные 10 с); `[Unit]` — `StartLimitIntervalSec=0` (без предела числа попыток). Сейчас перезапуск — каждые 5 с.
- В комментарии юнита: после длинной серии сбоев узел вернётся не позже чем через 5 минут после устранения причины,
  ускорить — `systemctl restart thepost`.

## Приёмка
- `systemd-analyze verify deploy/systemd/thepost.service` без ошибок; на стенде процесс, падающий при старте,
  перезапускается с растущей паузой.
