#!/bin/sh
# Самоподписанный CA и серверный сертификат для профилей tls и smb (только разработка и стенд).
# Запуск: сервис tls-init в compose или вручную `sh gen-certs.sh <каталог> [<каталог для ca.pem>]`.
# Повторный запуск ничего не меняет; новый CA — удалить каталог (в compose: down -v).
set -eu

DIR=${1:-/certs}
OUT=${2:-}
# Имена сервисов compose и адреса хоста — в SAN серверного сертификата.
SAN="DNS:localhost,DNS:postgres-tls,DNS:valkey-tls,DNS:seaweedfs-tls,IP:127.0.0.1,IP:::1"

mkdir -p "$DIR"
cd "$DIR"
if [ -s ca.pem ] && [ -s server.pem ] && [ -s server.key ]; then
  echo "tls: сертификаты уже есть в $DIR"
else
  umask 077
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 3650 \
    -subj "/CN=The Post dev CA" -keyout ca.key -out ca.pem \
    -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign"
  openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
    -subj "/CN=thepost-dev" -keyout server.key -out server.csr
  printf '%s\n' "subjectAltName=$SAN" "basicConstraints=critical,CA:FALSE" \
    "keyUsage=critical,digitalSignature" "extendedKeyUsage=serverAuth" > server.ext
  openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 825 \
    -extfile server.ext -out server.pem
  rm -f server.csr server.ext ca.srl
  echo "tls: создан CA и сертификат ($SAN)"
fi

# Ключ — владельцу 999 (postgres и valkey в своих образах), 0600 — требование PostgreSQL.
chmod 0644 ca.pem server.pem
chmod 0600 server.key ca.key
if [ "$(id -u)" = 0 ]; then chown 999:999 server.key; fi

# Копия CA для клиентов на хосте (sslrootcert, --cacert).
if [ -n "$OUT" ]; then cp ca.pem "$OUT/ca.pem"; fi
