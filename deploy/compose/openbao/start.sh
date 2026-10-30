#!/bin/sh
# Запуск OpenBao профилей tls и smb: ключ распечатывания (при первом запуске) и копия ключа TLS для пользователя openbao.
set -eu

umask 077
SEAL=/openbao/seal/unseal.key
if [ ! -s "$SEAL" ]; then
  head -c 32 /dev/urandom > "$SEAL.tmp"
  mv "$SEAL.tmp" "$SEAL"
  echo "openbao: создан ключ распечатывания $SEAL"
fi

# Ключ сервера в томе tls принадлежит 999 (postgres, valkey); OpenBao работает пользователем openbao.
mkdir -p /openbao/tls
cp /certs/server.pem /certs/server.key /openbao/tls/
chown -R openbao:openbao /openbao/tls /openbao/seal /openbao/data

exec /usr/local/bin/docker-entrypoint.sh server
