#!/usr/bin/env bash
# Generates the test keys and certificates (they are not real secrets; they are in .gitignore).
#   keys/id_ed25519       SSH key without a passphrase
#   keys/id_ed25519_pass  SSH key with the passphrase "spelaion-test"
#   keys/ca.crt           test certificate authority
#   keys/server.crt/.key  PostgreSQL TLS certificate, valid for "localhost" and for
#                         "postgres" (its name behind the bastions), but not for
#                         127.0.0.1: that way verify-full is tested against verify-ca
#   keys/mysql.crt/.key   the same for MySQL and MariaDB ("localhost", "mysql", "mariadb")
#   keys/client.crt/.key  client certificate for the user "spelaion_cert", who logs in
#                         without a password (ADR-022)
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p keys
# Git Bash on Windows rewrites arguments that start with "/" (like -subj "/CN=…").
export MSYS_NO_PATHCONV=1

[ -f keys/id_ed25519 ] || ssh-keygen -q -t ed25519 -N "" -C "spelaion-test" -f keys/id_ed25519
[ -f keys/id_ed25519_pass ] || ssh-keygen -q -t ed25519 -N "spelaion-test" -C "spelaion-test-pass" -f keys/id_ed25519_pass
cat keys/id_ed25519.pub keys/id_ed25519_pass.pub > keys/authorized_keys

if [ ! -f keys/server.crt ]; then
  openssl req -x509 -new -nodes -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 \
    -keyout keys/ca.key -out keys/ca.crt -days 3650 -subj "/CN=SpelaionDB test CA" \
    -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign" 2>/dev/null
  openssl req -new -nodes -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 \
    -keyout keys/server.key -out keys/server.csr -subj "/CN=localhost" 2>/dev/null
  printf 'subjectAltName=DNS:localhost,DNS:postgres\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\n' > keys/server.ext
  openssl x509 -req -in keys/server.csr -CA keys/ca.crt -CAkey keys/ca.key -CAcreateserial \
    -out keys/server.crt -days 3650 -extfile keys/server.ext 2>/dev/null
  rm -f keys/server.csr keys/server.ext keys/ca.srl
fi

# MySQL and MariaDB: their own certificate from the same authority, valid for "localhost" and
# for their names behind the bastions. Generated separately so existing keys are kept.
if [ ! -f keys/mysql.crt ]; then
  openssl req -new -nodes -newkey ec -pkeyopt ec_paramgen_curve:prime256v1     -keyout keys/mysql.key -out keys/mysql.csr -subj "/CN=localhost" 2>/dev/null
  printf 'subjectAltName=DNS:localhost,DNS:mysql,DNS:mariadb
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature
extendedKeyUsage=serverAuth
' > keys/mysql.ext
  openssl x509 -req -in keys/mysql.csr -CA keys/ca.crt -CAkey keys/ca.key -CAcreateserial     -out keys/mysql.crt -days 3650 -extfile keys/mysql.ext 2>/dev/null
  rm -f keys/mysql.csr keys/mysql.ext keys/ca.srl
fi

# A client certificate from the same authority: PostgreSQL, MySQL and MariaDB let the user
# "spelaion_cert" in with it instead of a password.
if [ ! -f keys/client.crt ]; then
  openssl req -new -nodes -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 \
    -keyout keys/client.key -out keys/client.csr -subj "/CN=spelaion_cert" 2>/dev/null
  printf 'basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=clientAuth\n' > keys/client.ext
  openssl x509 -req -in keys/client.csr -CA keys/ca.crt -CAkey keys/ca.key -CAcreateserial \
    -out keys/client.crt -days 3650 -extfile keys/client.ext 2>/dev/null
  rm -f keys/client.csr keys/client.ext keys/ca.srl
fi

echo "Keys and certificates in $(pwd)/keys"
