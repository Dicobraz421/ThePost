# DEP-14: стенд SMB — стресс, ×3, отказы и длительный прогон — ручная проверка

Стенд, учётные записи и наполнение — [DEP-04](DEP-04.md), [DEP-12](DEP-12.md); замеры и отчёт — [DEP-13](DEP-13.md).
Все сценарии — `stand.yml --tags run -e stand_scenario=<имя>`, отчёт — `criteria.md` прогона.

## Сценарии
| Сценарий | Нагрузка | Критерий в `criteria.md` |
|---|---|---|
| `stress` | ступени `stand_stress_factors` (×1, ×1,5, ×2, ×3, …) по `stand_stress_step` с (600) до первого нарушения Д.7 (`criteria.py --check` после каждой ступени) | раздел «Стресс: ступени»: p95 зондов, CPU узла и процесса, RSS, память, диск, соединения СУБД; точка насыщения и узкое место |
| `x3` | ×3, 30 мин | «Поведение под нагрузкой ×3»: рестартов `thepost` 0 (`NRestarts`, `MainPID`), отказы SMTP — только 4xx, JMAP 5xx — 0, потерянных писем 0 |
| `faults` | ×1, 30 мин; `stand.yml` на 1/3 — `systemctl kill --signal=SIGKILL thepost`, на 2/3 — `systemctl restart postgresql` (события — `events.txt`) | «Отказ узла приложения»: перерыв входа IMAP и сессии JMAP после каждого отказа ≤ 30 с, потерянных писем 0; задержки строк 1–5 — без 60 с после отказа |
| `long` | ×1, `stand_duration` (72 ч); окна `stand_long_window` с (1 800) | «Длительный тест 72 ч»: рост RSS (медиана окна) ≤ 10 %, деградация p95 (smtp, jmap_query, select) ≤ 20 %; таблица окон |

Подключённые клиенты (imaptest, push) масштабируются вместе с множителем. В `thresholds.yml` выключен
`ratelimit` узла (адрес, подсеть, учётная запись): вся нагрузка идёт с одного адреса генератора, и лимит по
умолчанию (60 соединений SMTP в минуту, блокировка 15 мин) отсекал её `421 4.7.0 Too many connections from your
address`; строка Д.5 «соединений с IP: 5» на стенде нагрузки не проверяется (решение владельца).

## Шаги (уменьшенный стенд)
| # | Шаг | Ожидание |
|---|---|---|
| 1 | `… --tags run -e stand_scenario=stress -e stand_users=50 -e stand_stress_step=60 -e stand_idle_connections=0` | ступени до первого нарушения или до последней; в `criteria.md` — таблица ступеней, точка насыщения, узкое место |
| 2 | `… -e stand_scenario=x3 -e stand_users=50 -e stand_duration=180 -e stand_idle_connections=0` | строка ×3 — «прошёл» или «не прошёл» с причиной; `nrestarts` в `smb-samples.csv` не растёт |
| 3 | `… -e stand_scenario=faults -e stand_users=50 -e stand_duration=240 -e stand_idle_connections=0` | в `events.txt` — `kill-node` и `restart-postgresql`; в `criteria.md` — перерыв по каждому отказу и сверка писем |
| 4 | `… -e stand_scenario=long -e stand_users=50 -e stand_duration=360 -e stand_long_window=60 -e stand_idle_connections=0` | таблица окон (6 × 60 с), строка «Длительный тест» с ростом RSS и деградацией p95 (примечание «короче 72 ч») |

## Итог уменьшенного прогона (2026-11-11)
Стенд — как в [DEP-12](DEP-12.md), 50 пользователей, без замера простаивающих соединений. Все четыре сценария
отработали одной командой и дали `criteria.md`; `stand.yml` завершился `failed` на `Check criteria D.7` (есть
«не прошёл»), отчёты скачаны.

| Шаг | Факт |
|---|---|
| 1 `stress` (ступень 60 с) | ×1 — уже нарушение: JMAP query p95 29 с, SELECT 10 тыс. p95 24 с, поиск p95 44 с, 56 из 56 писем нагрузки — `451`/`454`; ступень длилась ~5 мин (imaptest и клиенты SMTP ждут зависшие команды до `stand_duration + 600` с). Отчёт: «Точка насыщения: нет — нарушение уже на первой ступени; узкое место: пул СУБД 3 из 3» (CPU узла 19 %, процесса 4 %, память хоста 88 %, диск 69 %) |
| 2 `x3` (180 с) | `nrestarts` 0, один PID — узел не падал; отказы SMTP 197 из 197 (`451` 120, `454` 73, `conn` 4); потерь нет (принято 1, доставлено 5 — хвост прошлых прогонов); строка ×3 — «не прошёл» (отказы `conn` и 5xx JMAP) |
| 3 `faults` (240 с) | `events.txt`: `kill-node` на +85 с, `restart-postgresql` на +169 с; после `SIGKILL` systemd перезапускал узел 9 раз — каждый запуск падал `thepost: unavailable` (вход AppRole в OpenBao не успевал: OpenBao `failed to persist lease entry: context canceled` — диск хоста перегружен); вход IMAP / сессия JMAP не вернулись до конца замеров: «kill-node: > 289 с, restart-postgresql: > 205 с» — «не прошёл»; потерь нет (принятых писем за прогон — 0) |
| 4 `long` (360 с, окна 60 с) | нагрузка шла 668 с (зависшие команды клиентов); 11 окон, таблица окон в отчёте; RSS по окнам 939 MB (рост 0 %), деградация p95: jmap_query −17 %, select +353 % — «не прошёл», примечание «окон 11 × 60 с, всего 0,18 ч (короче 72 ч Д.6)»; SMTP нагрузки — 1 принято из 336 |

Причина отказов на этом стенде — узел ждёт соединение пула СУБД (`database.max_connections: 3`, порог Д.5):
в журнале `acquired connection … exceeded slow threshold`, `запись BLOB не удалась … unavailable`,
`запись узла в членстве не продлена`, IMAP `[UNAVAILABLE] Authentication temporarily unavailable`.

## Что модель не покрывает (на сегодня)
- Отказы Д.6 «узел SeaweedFS» и «разрыв ядро — S3»: в профиле SMB хранилище — локальный диск, S3 нет.
- Узкое место «пул СУБД»: метрики занятости пула у узла нет — оценка по `pg_stat_activity` (соединения `thepost*`
  против `database.max_connections`).
- `node_exporter` не ставится: CPU, память и диск — из `/proc` узла (`smb-sampler.py`).
