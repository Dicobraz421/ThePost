# SIEVE-14: `processcalendar` (RFC 9671), расширение `vnd.thepost.peer`, админский скрипт — только сужение

lane: sieve
deps: [CTR-93]
class: A
files: [crates/tp-sieve/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md#124, docs/research/2026-11-14-trust-matrix.md, docs/tasks/CTR-93.md, docs/ADMIN-GUIDE.md#что-может-менять-администратор]
checks: [nextest]
manual: no

## Сделать
- Расширение `processcalendar` (RFC 9671): действие с `:allowpublic`, `:addresses`, `:updatesonly` или `:calendarid`,
  `:deletecancelled`, `:outcome`, `:reason`; итог — `calendar` в итоге правил (CTR-93), применяет приём iTIP. В
  пределах политики: новое приглашение вносится, только если потолок клетки `invite_new` для типа отправителя —
  `Auto` (скрипт организации — потолок уровня организации, скрипт пользователя — действующее значение для
  пользователя); иначе `:outcome` — `no_action`, `:reason` — «политика доверия». Не меняет `PARTSTAT` и не отменяет
  неявный `keep` (§3); на письма из «Спама» не действует (§5). `:organizers` — только вместе с `extlists`
  (RFC 6134); пока его нет — ошибка `require`.
- Расширение `vnd.thepost.peer` (вендорское, RFC 5228 §6): тест `peer [COMPARATOR] [MATCH-TYPE] <key-list>`, значения
  `own`, `partner`, `neighbor`, `external` — тип абонента из контекста ядра (CTR-93), не из заголовков.
- Профиль проверки «админский» для скрипта организации (этап before, доверенный): `vacation` запрещён; `redirect` — на
  внутренний адрес или на домен, куда открыта автопересылка (адреса литеральные: расширения `variables` нет); прочее —
  как у пользователя. Ошибка — `Diagnostic` со строкой и столбцом (проверку при сохранении вызывает ADM-49).
- Выполнение скрипта организации: сбой (ошибка выполнения, превышение `Limits`) — итог «отказ» (`Outcome::Failed`),
  который SMTP превращает во временный отказ (SMTP-50), а не доставку мимо скрипта; сбой скрипта пользователя — как
  сейчас.
- `EXTENSIONS`: добавить `processcalendar`, `vnd.thepost.peer` — их объявят ManageSieve и JMAP Sieve.
- Автоответ (`vacation.rs`): для чужого отправителя — внешний текст `Vacation::external_*`; нет его или `auto_reply =
  Off` — ответа нет.

## Как делают другие
- RFC 9671 (Fastmail, 10.2024), реализация — Cyrus `processimip`
  (https://www.cyrusimap.org/3.12/imap/reference/admin/sieve.html).
- Скрипты администратора до и после пользовательских: Dovecot `sieve_before`/`sieve_after`, расширения только глобальным
  скриптам — `sieve_global_extensions`; `after` не годится для принуждения (https://doc.dovecot.org/main/core/plugins/sieve.html).
- Stalwart — доверенный интерпретатор системных скриптов, при сбое письмо проходит
  (https://stalw.art/docs/sieve/interpreter/trusted/).
- Отличие: сбой админского скрипта — отказ, а не пропуск; тип абонента — наше расширение, стандартного теста нет.

## Приёмка
- Тесты: `processcalendar` для `partner` при потолке `Auto` — итог `calendar`; для `external` — `no_action`; на письмо в
  «Спам» — без действия; `peer "partner"` истинно только при типе из контекста, заголовок `X-Peer` не влияет.
- Профиль «админский»: `vacation` — ошибка со строкой и столбцом; `redirect "a@gmail.com"` при закрытой автопересылке —
  ошибка; `redirect` на свой адрес — допустим.
- Превышение `Limits` в скрипте организации — `Outcome::Failed`; в скрипте пользователя — неявный `keep`.
- Автоответ внешнему без внешнего текста — нет ответа; с текстом — внешний.
