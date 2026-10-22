#!/usr/bin/env bash
# Generates the development-only TLS material for the test Compose PostgreSQL
# (BS-020.T6), once. Run by `make services-up`; the output is ignored by git.
#
#   <dir>/ca.crt        development CA that signs the server certificate
#   <dir>/server.crt    certificate for localhost, 127.0.0.1, and postgres
#   <dir>/server.key    its private key
#   <dir>/other-ca.crt  an unrelated CA, for negative verification tests
#
# Nothing here is a production credential. Delete the directory and restart
# the services to rotate it.
set -euo pipefail

dir="${1:?usage: dev-pki.sh <output-directory>}"
if [ -s "$dir/ca.crt" ] && [ -s "$dir/server.crt" ] && [ -s "$dir/server.key" ] \
  && [ -s "$dir/other-ca.crt" ]; then
  exit 0
fi

mkdir -p "$dir"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

ca() {
  openssl req -x509 -newkey rsa:2048 -nodes -sha256 -days 3650 \
    -subj "/CN=$1" \
    -addext "basicConstraints=critical,CA:TRUE" \
    -addext "keyUsage=critical,keyCertSign,cRLSign" \
    -keyout "$work/$2.key" -out "$work/$2.crt" 2>/dev/null
}

ca "Bergsmith development CA" ca
ca "Bergsmith unrelated development CA" other-ca

openssl req -newkey rsa:2048 -nodes -sha256 -subj "/CN=localhost" \
  -keyout "$work/server.key" -out "$work/server.csr" 2>/dev/null
cat >"$work/server.ext" <<'EOF'
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature,keyEncipherment
extendedKeyUsage=serverAuth
subjectAltName=DNS:localhost,DNS:postgres,IP:127.0.0.1
EOF
openssl x509 -req -sha256 -days 825 -in "$work/server.csr" \
  -CA "$work/ca.crt" -CAkey "$work/ca.key" -set_serial "0x$(openssl rand -hex 8)" \
  -extfile "$work/server.ext" -out "$work/server.crt" 2>/dev/null

# The container copies the key and sets its owner; PostgreSQL refuses a key
# readable by others, so the copy, not this file, carries the final mode.
chmod 600 "$work/server.key"
cp "$work/ca.crt" "$work/other-ca.crt" "$work/server.crt" "$work/server.key" "$dir/"
