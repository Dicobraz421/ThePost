# SMTP-52: детекторы каталога признаков и применение балла на smtp-in

lane: smtp
deps: [CTR-95, DEPS-19, SMTP-51]
class: A
files: [crates/tp-smtp/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md#126, docs/research/2026-11-14-anomaly-ti.md, docs/tasks/CTR-95.md, docs/tasks/SMTP-51.md, docs/tasks/SIEVE-15.md]
checks: [nextest]
manual: no

## Сделать
- Детекторы `Detector` (CTR-95) на smtp-in после `DATA`, до Milter и Sieve, по разобранному письму (вложения с учётом
  вложенности до `smtp.risk.max_depth`, тела до `smtp.risk.max_scan_bytes`):
  `thread_new_attachment_type` — тип по содержимому (`infer`) и расширению, которого не было в памяти беседы (CTR-94);
  `encrypted_archive` — ZIP с флагом шифрования записи (`zip`), 7z с зашифрованным заголовком (AES в `kEncodedHeader`),
  RAR 5 с заголовком шифрования архива — по сигнатурам, без распаковки; `disk_image_or_shortcut` — ISO, IMG, VHD, VHDX,
  LNK по сигнатуре или расширению; `thread_new_link_domain` — регистрируемый домен ссылок (`publicsuffix` по списку из
  пакета), которого не было в беседе; `reply_to_changed` — `Reply-To` не совпадает с `From` и прежним `Reply-To`
  беседы; `list_match` — поле письма в списке TI (индексы списков передаёт узел через интерфейс, APP-79: префиксы IP —
  `ipnet`, домены, URL, SHA-256).
- Пять признаков беседы заменяют жёстко заданный перечень аномалий SMTP-51 (решение 125 д): признак ответа снимается,
  если сработал признак с `drops_reply()`; заголовка `X-ThePost-Reply-Anomaly` нет — вместо него `X-ThePost-Risk`.
  Своего перечня аномалий в SMTP-51 нет (карточка правлена решением 126) — второго пути не появляется.
- Балл: действующий каталог организации получателя (`EffectiveCatalog`); источник списка устарел — его признаки по
  `StaleAction` (вес снижен или «только отчёт»), предупреждение в метриках. Затем признаки от `signal` скрипта
  организации (SIEVE-15) — с пределом веса. Итог — `Verdict::combine` с действием Rspamd (самое строгое).
- Действия: `tag` — заголовок `X-ThePost-Risk: score=N; signals=…` (входящий с тем же именем удаляется до проверок);
  `junk` — в «Спам»; `quarantine` — карантин узла; `reject` — `550 5.7.1` в сессии, а если адресаты разных
  организаций расходятся — карантин для тех, у кого `reject` (после приёма отказ письмом не шлём — backscatter).
  Признаки `Mode::Report` — только в журнал срабатываний (`record_hits`) и метрики.
- Событие журнала доставки — балл, признаки и действие; `thepost admin delivery show` их показывает.
- Конфигурация — секция `smtp.risk` с `Default` (SRS 5.5): пределы разбора, включение, таймаут детекторов; метрики
  срабатываний по признаку и режиму, времени детекторов.

## Как делают другие
- Exchange: условия «вложение защищено паролем», «исполняемое содержимое», проверка вложений с учётом вложенности,
  первые 150 КБ содержимого (https://learn.microsoft.com/en-us/exchange/security-and-compliance/mail-flow-rules/conditions-and-exceptions).
- Rspamd: символы с баллом и порогами действий, `multimap` по `filename` и `url`, модуль `phishing`
  (https://docs.rspamd.com/modules/multimap, https://docs.rspamd.com/modules/phishing).
- ISO, IMG, LNK как носители после отключения макросов — Proofpoint
  (https://www.proofpoint.com/us/blog/threat-insight/how-threat-actors-are-adapting-post-macro-world).
- Отличие: признаки и веса — из подписанного каталога с режимом «только отчёт»; отказ только в сессии SMTP.

## Приёмка
- Тесты каждого детектора: ZIP с шифрованием и без; 7z и RAR 5 с зашифрованным заголовком; ISO и LNK под чужим
  расширением; ссылка `a.b.example.co.uk` при беседе с `example.co.uk` — не новая; `Reply-To` сменился.
- Ответ в доверенной беседе с ISO — признак ответа снят, `X-ThePost-Risk` с `tp.attach.disk_image_or_shortcut`.
- Балл ≥ порога `quarantine` — карантин; признак в `Report` — балл не изменился, срабатывание записано; Rspamd
  `reject` при малом балле — отказ; `signal :weight 3` скрипта организации прибавлен.
- Устаревший источник с `Reduce { 50 }` — вес признака вдвое меньше; входящий `X-ThePost-Risk` удалён.
