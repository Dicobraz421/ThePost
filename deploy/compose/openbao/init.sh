#!/bin/sh
# Начальная настройка OpenBao: KV v2 `thepost`, политика `thepost`, вход AppRole `thepost`, секреты стенда.
# Повторный запуск ничего не меняет.
#   Профиль dev: BAO_TOKEN задан (dev-режим), идентификаторы AppRole фиксированы (TP_APPROLE_ROLE_ID, TP_APPROLE_SECRET_ID).
#   Профили tls и smb: первый запуск — `bao operator init`; ключ восстановления и root-токен — /admin/init.txt
#   (только стенд); role_id и secret_id узла — /approle; новый пароль роли PostgreSQL — /bootstrap/db-password
#   (его применяет и удаляет postgres-role-tls).
set -eu
umask 077

ADMIN=/admin
APPROLE=/approle
BOOTSTRAP=/bootstrap
APP_UID=${TP_APP_UID:-65532}

# bao status: 0 — распечатан, 2 — запечатан или не инициализирован, 1 — недоступен.
wait_bao() {
  want=$1
  i=0
  while :; do
    rc=0
    bao status >/dev/null 2>&1 || rc=$?
    if [ "$rc" = 0 ] || { [ "$want" = up ] && [ "$rc" = 2 ]; }; then return 0; fi
    i=$((i + 1))
    if [ "$i" -ge 60 ]; then
      echo "openbao-init: OpenBao $BAO_ADDR не готов (bao status = $rc)" >&2
      return 1
    fi
    sleep 1
  done
}

wait_bao up
if [ -z "${BAO_TOKEN:-}" ]; then
  if bao status -format=json | grep -q '"initialized": *false'; then
    bao operator init -recovery-shares=1 -recovery-threshold=1 > "$ADMIN/init.txt"
    echo "openbao-init: хранилище инициализировано; ключ восстановления и root-токен — том openbao-admin"
  fi
  BAO_TOKEN=$(awk '/Initial Root Token/ { print $NF }' "$ADMIN/init.txt")
  export BAO_TOKEN
fi
wait_bao unsealed

bao secrets list | grep -q '^thepost/' || bao secrets enable -path=thepost -version=2 kv
bao policy write thepost /openbao-policy/thepost-policy.hcl >/dev/null
bao auth list | grep -q '^approle/' || bao auth enable approle
# Токен узла живёт час и продлевается до суток, затем — новый вход; secret_id бессрочный (выдаёт администратор).
bao write auth/approle/role/thepost token_policies=thepost token_ttl=1h token_max_ttl=24h \
  secret_id_ttl=0 secret_id_num_uses=0 >/dev/null

if [ -n "${TP_APPROLE_ROLE_ID:-}" ]; then
  bao write auth/approle/role/thepost/role-id role_id="$TP_APPROLE_ROLE_ID" >/dev/null
fi
if [ -n "${TP_APPROLE_SECRET_ID:-}" ] \
  && ! bao write -format=json auth/approle/role/thepost/secret-id/lookup secret_id="$TP_APPROLE_SECRET_ID" 2>/dev/null \
    | grep -q secret_id_accessor; then
  bao write auth/approle/role/thepost/custom-secret-id secret_id="$TP_APPROLE_SECRET_ID" >/dev/null
fi
if [ -d "$APPROLE" ]; then
  bao read -field=role_id auth/approle/role/thepost/role-id > "$APPROLE/role_id"
  if [ ! -s "$APPROLE/secret_id" ]; then
    bao write -f -field=secret_id auth/approle/role/thepost/secret-id > "$APPROLE/secret_id.tmp"
    mv "$APPROLE/secret_id.tmp" "$APPROLE/secret_id"
    echo "openbao-init: выдан secret_id AppRole thepost"
  fi
  chown "$APP_UID:$APP_UID" "$APPROLE/role_id" "$APPROLE/secret_id"
  chmod 0400 "$APPROLE/role_id" "$APPROLE/secret_id"
fi

# Секреты стенда: пароль роли thepost в PostgreSQL (новый — случайный), ключи S3 (compose/seaweedfs-s3.json).
if ! bao kv get -mount=thepost db >/dev/null 2>&1; then
  pw=${TP_DB_PASSWORD:-$(head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n')}
  printf %s "$pw" | bao kv put -mount=thepost db username=thepost password=- >/dev/null
  if [ -d "$BOOTSTRAP" ]; then printf %s "$pw" > "$BOOTSTRAP/db-password"; fi
  echo "openbao-init: записан секрет thepost/db"
fi
if ! bao kv get -mount=thepost s3 >/dev/null 2>&1; then
  printf %s "$TP_S3_SECRET_ACCESS_KEY" \
    | bao kv put -mount=thepost s3 access_key_id="$TP_S3_ACCESS_KEY_ID" secret_access_key=- >/dev/null
  echo "openbao-init: записан секрет thepost/s3"
fi
echo "openbao-init: готово"
