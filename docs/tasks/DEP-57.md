# DEP-57: доставка пакета признаков — исходники пакета, ключи, обновление и закрытый контур; фиды через Rspamd и ClamAV

lane: deploy
deps: [APP-78, DEP-56]
class: A
files: [deploy/*]
read: [docs/decisions/2026-11-11-stage2-followups.md#126, docs/research/2026-11-14-anomaly-ti.md, docs/tasks/APP-78.md, docs/tasks/APP-79.md, deploy/ansible/roles/rspamd/tasks/main.yml, deploy/ansible/roles/clamav_milter/tasks/main.yml]
checks: [lint]
manual: no

## Сделать
- Исходники пакета поставщика — `deploy/risk/`: `signals.toml` (пять признаков беседы решения 126 г и признаки
  `list_match` по спискам поставки), `samples/*.eml` с ожидаемыми признаками, `sources.toml` — только Feodo Tracker,
  SSLBL (CC0) и Spamhaus DROP/EDROP (атрибуция: ссылка, дата, ©), а также Public Suffix List (MPL-2.0, файл без
  изменений, уведомление о лицензии в `NOTICE` пакета). Скрипт сборки `deploy/risk/build.sh`: скачать фиды и PSL (не
  чаще раза в сутки), проверить формат, `thepost risk pack` с ключом выпуска из OpenBao, номер версии — больше
  прошлого опубликованного. Источник из запрещённых (решение 126 ж) скрипт не собирает — проверку повторяет lint узла.
- Поставка: первый пакет и открытые ключи — в образ и пакеты ОС (`/usr/share/thepost/risk/`); роль `thepost` —
  `risk.trusted_keys`, `risk.update.url` (по умолчанию наш адрес обновлений), `risk.update.dir` и
  `risk.update.auto_activate` в шаблоне `config.yaml`. Закрытый контур: `thepost_risk_offline: true` — без URL, файл
  пакета копируется в `risk.update.dir` задачей роли (`thepost_risk_package_src`) или вручную.
- Ключ заказчика — через Rspamd: переменные роли `rspamd` для Spamhaus DQS (готовая конфигурация Spamhaus для Rspamd),
  SURBL и URIBL с ключом из OpenBao; по умолчанию выключены, бесплатные зеркала ZEN/DBL/ZRD не включаются.
- ClamAV: роль `clamav_milter` — частное зеркало заказчика (`PrivateMirror`/`DatabaseMirror`, cvdupdate) и
  SaneSecurity с учётными данными заказчика; по умолчанию выключены. Работа только через `clamd` по сокету.
- Балл признаков каталога в Rspamd не дублируется: правила для `X-ThePost-Risk` нет, аномалии беседы считает узел
  (SMTP-52); у DEP-56 остаётся только `THEPOST_REPLY`.

## Как делают другие
- Rspamd `rspamadm signtool` и `trusted_keys` (https://docs.rspamd.com/configuration/maps); sa-update каналы и
  `--install` (https://spamassassin.apache.org/full/4.0.x/doc/sa-update.html); зеркало ClamAV
  (https://docs.clamav.net/appendix/CvdPrivateMirror.html); Spamhaus DQS для Rspamd
  (https://github.com/spamhaus/rspamd-dqs); SaneSecurity (https://sanesecurity.com/usage/).
- Отличие: мы раздаём только фиды с разрешением на коммерцию; остальное — переменными роли с ключом заказчика.

## Приёмка
- `ansible-lint` и проверка шаблонов; `deploy/risk/build.sh` собирает пакет, `thepost risk verify` на нём зелёный;
  `sources.toml` с OpenPhish Community — сборка отказывает.
- Стенд: установка без сети с `thepost_risk_offline: true` ставит пакет из файла; включение DQS с ключом — символы
  Spamhaus в Rspamd; правила для `X-ThePost-Risk` в роли нет.
