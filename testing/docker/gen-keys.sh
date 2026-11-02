#!/usr/bin/env bash
# Generates the test keys and certificates (they are not real secrets; they are in .gitignore).
#   keys/id_ed25519       SSH key without a passphrase
#   keys/id_ed25519_pass  SSH key with the passphrase "spelunk-test"
#   keys/ca.crt           test certificate authority
#   keys/server.crt/.key  PostgreSQL TLS certificate, valid for "localhost" and for
#                         "postgres" (its name behind the bastions), but not for
#                         127.0.0.1: that way verify-full is tested against verify-ca
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p keys
# Git Bash on Windows rewrites arguments that start with "/" (like -subj "/CN=…").
export MSYS_NO_PATHCONV=1

[ -f keys/id_ed25519 ] || ssh-keygen -q -t ed25519 -N "" -C "spelunk-test" -f keys/id_ed25519
[ -f keys/id_ed25519_pass ] || ssh-keygen -q -t ed25519 -N "spelunk-test" -C "spelunk-test-pass" -f keys/id_ed25519_pass
cat keys/id_ed25519.pub keys/id_ed25519_pass.pub > keys/authorized_keys

if [ ! -f keys/server.crt ]; then
  openssl req -x509 -new -nodes -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 \
    -keyout keys/ca.key -out keys/ca.crt -days 3650 -subj "/CN=SpelunkDB test CA" \
    -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign" 2>/dev/null
  openssl req -new -nodes -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 \
    -keyout keys/server.key -out keys/server.csr -subj "/CN=localhost" 2>/dev/null
  printf 'subjectAltName=DNS:localhost,DNS:postgres\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\n' > keys/server.ext
  openssl x509 -req -in keys/server.csr -CA keys/ca.crt -CAkey keys/ca.key -CAcreateserial \
    -out keys/server.crt -days 3650 -extfile keys/server.ext 2>/dev/null
  rm -f keys/server.csr keys/server.ext keys/ca.srl
fi

echo "Keys and certificates in $(pwd)/keys"
