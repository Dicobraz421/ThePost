# DEP-28: Helm: образ узла из ghcr.io

lane: deploy
deps: [CI-03]
class: A
files: [deploy/helm/thepost/*]
read: [docs/tasks/CI-03.md]
checks: [none]
manual: no

Хвост волны ci-03, PR #281: `deploy/helm/thepost/values.yaml` — `image.repository: thepost`, не указывает на ghcr.io.

## Сделать
- `image.repository: ghcr.io/dicobraz421/thepost` (имя образа из `beta.yml`), `tag` — `appVersion`; в комментарии —
  проверка подписи образа cosign и своё зеркало реестра.

## Приёмка
- `helm template` и kubeconform без ошибок, в манифесте — образ ghcr.io.
