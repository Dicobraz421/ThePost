#!/bin/sh
# Начальная настройка OpenBao в Kubernetes для The Post: KV v2 `thepost`, политика `thepost`
# (deploy/openbao/thepost-policy.hcl), вход Kubernetes auth для сервисного аккаунта thepost, пароли учётных записей
# PostgreSQL, ключ TLS клиентских протоколов (секрет thepost/tls#key, решение 100).
# Запуск — после `bao operator init` (ключи восстановления и root-токен остаются у администратора):
#   BAO_TOKEN=<токен администратора> TLS_KEY=<ключ PEM> sh deploy/helm/openbao/bootstrap.sh \
#     [<пространство OpenBao> [<пространство thepost>]]
# TLS_KEY — ключ к цепочке tls.crt секрета tls.secretName чарта; другой ключ — новая версия секрета (узел перечитывает
# пару сам); без TLS_KEY ключ не меняется. Повторный запуск ничего не меняет. Токен и ключ передаются в под через
# stdin, не аргументами.
set -eu

NS=${1:-openbao}
APP_NS=${2:-thepost}
POD=${OPENBAO_POD:-openbao-0}
POLICY="$(dirname "$0")/../../openbao/thepost-policy.hcl"
: "${BAO_TOKEN:?нужен BAO_TOKEN (root-токен или токен администратора)}"
TLS_KEY=${TLS_KEY:-}
if [ -n "$TLS_KEY" ] && ! grep -q -- '-----BEGIN .*PRIVATE KEY-----' "$TLS_KEY"; then
  echo "bootstrap: TLS_KEY=$TLS_KEY — не ключ PEM" >&2
  exit 1
fi

{
  printf 'set -eu\nexport BAO_TOKEN=%s\nAPP_NS=%s\n' "$BAO_TOKEN" "$APP_NS"
  # В PEM нет кавычек: значение — одной строкой sh в одинарных кавычках.
  printf "TLS_KEY='%s'\n" "$(if [ -n "$TLS_KEY" ]; then cat "$TLS_KEY"; fi)"
  cat <<'EOF'
bao secrets list | grep -q '^thepost/' || bao secrets enable -path=thepost -version=2 kv
bao policy write thepost - <<'POLICY'
EOF
  cat "$POLICY"
  cat <<'EOF'
POLICY
bao auth list | grep -q '^kubernetes/' || bao auth enable kubernetes
# Внутри кластера: CA и токен для TokenReview — сервисного аккаунта OpenBao.
bao write auth/kubernetes/config kubernetes_host="https://$KUBERNETES_SERVICE_HOST:$KUBERNETES_SERVICE_PORT" >/dev/null
# Токен узла живёт час и продлевается до суток, затем — новый вход.
bao write auth/kubernetes/role/thepost bound_service_account_names=thepost \
  bound_service_account_namespaces="$APP_NS" token_policies=thepost token_ttl=1h token_max_ttl=24h >/dev/null
# Пароли учётных записей PostgreSQL (DEP-08) — случайные при первом запуске: узел thepost (thepost/db), владелец
# схемы thepost_owner (thepost/db-owner), только чтение thepost_readonly (thepost/db-readonly, узлу недоступен).
# В СУБД их задаёт администратор (deploy/README.md, compose/postgres/app-role.sh).
for s in db:thepost db-owner:thepost_owner db-readonly:thepost_readonly; do
  if ! bao kv get -mount=thepost "${s%%:*}" >/dev/null 2>&1; then
    head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n' \
      | bao kv put -mount=thepost "${s%%:*}" username="${s#*:}" password=- >/dev/null
    echo "bootstrap: записан секрет thepost/${s%%:*}"
  fi
done
# Ключ TLS слушателей: другой — новая версия секрета.
if [ -n "$TLS_KEY" ] && [ "$(bao kv get -mount=thepost -field=key tls 2>/dev/null || true)" != "$TLS_KEY" ]; then
  printf '%s\n' "$TLS_KEY" | bao kv put -mount=thepost tls key=- >/dev/null
  echo "bootstrap: записан ключ TLS thepost/tls"
fi
echo "bootstrap: KV thepost, политика thepost, Kubernetes auth (роль thepost, пространство $APP_NS) — готово"
EOF
} | kubectl exec -i -n "$NS" "$POD" -- sh -s
