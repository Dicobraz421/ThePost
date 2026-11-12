#!/bin/sh
# Самоподписанный CA и серверный сертификат для профилей tls и smb (только разработка и стенд).
# Запуск: сервис tls-init в compose или вручную `sh gen-certs.sh <каталог> [<каталог для ca.pem>]`.
# Повторный запуск ничего не меняет; сменился список SAN — перевыпускается только серверный сертификат;
# новый CA — удалить каталог (в compose: down -v).
set -eu

DIR=${1:-/certs}
OUT=${2:-}
# Имена сервисов compose и адреса хоста — в SAN серверного сертификата.
SAN="DNS:localhost,DNS:thepost,DNS:mail.example.test,DNS:postgres-tls,DNS:seaweedfs-tls,DNS:openbao-tls,IP:127.0.0.1,IP:::1"

mkdir -p "$DIR"
cd "$DIR"
umask 077
if [ -s ca.pem ] && [ -s ca.key ] && [ -s server.pem ] && [ -s server.key ] \
  && [ "$(cat server.san 2>/dev/null)" = "$SAN" ]; then
  echo "tls: сертификаты уже есть в $DIR"
else
  if [ -s ca.pem ] && [ -s ca.key ]; then
    echo "tls: CA уже есть в $DIR, перевыпуск серверного сертификата"
  else
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 3650 \
      -subj "/CN=The Post dev CA" -keyout ca.key -out ca.pem \
      -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign"
  fi
  openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
    -subj "/CN=thepost-dev" -keyout server.key -out server.csr
  printf '%s\n' "subjectAltName=$SAN" "basicConstraints=critical,CA:FALSE" \
    "keyUsage=critical,digitalSignature" "extendedKeyUsage=serverAuth" > server.ext
  openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 825 \
    -extfile server.ext -out server.pem
  rm -f server.csr server.ext ca.srl
  printf '%s\n' "$SAN" > server.san
  echo "tls: создан сертификат ($SAN)"
fi

# Ключ — владельцу 999 (postgres в своём образе), 0600 — требование PostgreSQL.
chmod 0644 ca.pem server.pem
chmod 0600 server.key ca.key
if [ "$(id -u)" = 0 ]; then chown 999:999 server.key; fi

# Цепочка thepost под путём секции tls (fullchain.pem) — при каждом запуске, вслед за перевыпуском. Ключ thepost —
# только в OpenBao (секрет thepost/tls#key, его пишет openbao/init.sh из server.key; решение 100): файла ключа для
# узла нет, прежний privkey.pem удаляется.
cp server.pem fullchain.pem
chmod 0644 fullchain.pem
rm -f privkey.pem

# Копия CA для клиентов на хосте (sslrootcert, --cacert).
if [ -n "$OUT" ]; then cp ca.pem "$OUT/ca.pem" && chmod 0644 "$OUT/ca.pem"; fi
