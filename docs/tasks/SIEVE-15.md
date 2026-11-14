# SIEVE-15: расширение `vnd.thepost.risk` — `signal`, `environment "vnd.thepost.*"`, `extlists`, `:regex`

lane: sieve
deps: [CTR-95, DEPS-19, SIEVE-14]
class: A
files: [crates/tp-sieve/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md#126, docs/research/2026-11-14-anomaly-ti.md, docs/tasks/CTR-95.md, docs/tasks/SIEVE-14.md]
checks: [nextest]
manual: no

## Сделать
- Расширение `vnd.thepost.risk` (вендорское, RFC 5228 §6): команда `signal "<id>" [:weight N]` — признак в итог
  правил (`signals`, CTR-95). `id` — признак каталога (без `:weight` берётся его действующий вес) или `org.<имя>`
  (без `:weight` — вес 1). `N` — целое `0 ≤ N ≤ sieve.risk.max_weight` (по умолчанию 10, конфигурация); отрицательное,
  больше предела или повтор одного `id` с разными весами — ошибка проверки со строкой и столбцом. Вычесть балл,
  снять признак или изменить порог команда не может.
- Только профиль «админский» (скрипт организации, этап before, SIEVE-14): `require "vnd.thepost.risk"` в скрипте
  пользователя — ошибка проверки.
- `environment` (RFC 5183) с пунктами ядра (`domain`, `host`, `phase` — по RFC) и нашими `vnd.thepost.thread`
  (`trusted`|`untrusted`|`none`), `vnd.thepost.reply` (`yes`|`no`), `vnd.thepost.risk.score` (балл до Sieve),
  `vnd.thepost.risk.signals` (сработавшие `id` через запятую) — из контекста (CTR-95), не из заголовков.
- `extlists` (RFC 6134): `:list` у `header`, `address`, `envelope`, тест `valid_ext_list`; имя списка — URI
  `tag:` по CTR-95 на список TI или адресную книгу; поиск — через интерфейс, который передаёт узел (APP-79), в
  тестах — память. Это же снимает ограничение `:organizers` у `processcalendar` (SIEVE-14).
- Тип сравнения `:regex` (draft-ietf-sieve-regex) — только в профиле «админский»: `regex::RegexBuilder` с
  `size_limit` и `dfa_size_limit` из конфигурации (`sieve.regex`), компиляция при проверке скрипта; превышение —
  ошибка проверки, а не выполнения.
- `EXTENSIONS`: `vnd.thepost.risk`, `environment`, `extlists`, `regex` — объявляются только для админского профиля
  (ManageSieve и JMAP Sieve пользователя их не показывают).

## Как делают другие
- Sieve не считает: `spamtest`/`virustest` (RFC 5235) только читают готовый балл; `environment` (RFC 5183) и
  `extlists` (RFC 6134) — стандартные способы отдать факты и списки
  (https://www.rfc-editor.org/rfc/rfc5183, https://www.rfc-editor.org/rfc/rfc6134).
- Stalwart — `vnd.stalwart.expressions` (`eval`, `let`) только в доверенном интерпретаторе
  (https://stalw.art/docs/sieve/); Dovecot — расширения только глобальным скриптам (`sieve_global_extensions`,
  https://doc.dovecot.org/main/core/plugins/sieve.html); `:regex` — Pigeonhole и Cyrus.
- Отличие: команда только добавляет риск с ограниченным весом — монотонность по построению языка; `:regex` — с
  линейным временем (`regex`, https://docs.rs/regex/latest/regex/#untrusted-input).

## Приёмка
- Тесты: `signal "tp.attach.encrypted_archive"` — вес каталога в итоге; `signal "org.x" :weight 5` — 5;
  `:weight 11` при пределе 10 и `:weight -1` — ошибка со строкой и столбцом; `require "vnd.thepost.risk"` в скрипте
  пользователя — ошибка.
- `environment "vnd.thepost.thread" "trusted"` истинно только по контексту; заголовок `X-ThePost-Thread` не влияет.
- `header :list "From" "tag:…:ti/feodo"` — совпадение по списку из памяти; `valid_ext_list` для неизвестного — ложь.
- `:regex` с шаблоном больше `size_limit` — ошибка проверки; шаблон `(a+)+$` на длинной строке — за линейное время.
