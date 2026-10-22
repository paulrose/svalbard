#!/usr/bin/env bash
# Starts a disposable PostgreSQL server with TLS in Docker for the PostgreSQL tests.
# Prints the environment for `cargo test --test postgresql -- --ignored`.
#
# Usage: export $(scripts/test-postgresql.sh <major version> [port])
set -euo pipefail

version=${1:?usage: $0 <major version> [port]}
port=${2:-5432}
name=svalbard-postgresql-$version
dir=$PWD/target/$name
mkdir -p "$dir"

# A test CA and a server certificate for localhost.
openssl req -x509 -newkey rsa:2048 -nodes -days 2 -subj /CN=svalbard-test-ca \
  -keyout "$dir/ca.key" -out "$dir/ca.crt" 2>/dev/null
openssl req -newkey rsa:2048 -nodes -subj /CN=localhost \
  -keyout "$dir/server.key" -out "$dir/server.csr" 2>/dev/null
printf 'subjectAltName=DNS:localhost\n' >"$dir/server.ext"
openssl x509 -req -days 2 -in "$dir/server.csr" -CA "$dir/ca.crt" -CAkey "$dir/ca.key" \
  -CAcreateserial -extfile "$dir/server.ext" -out "$dir/server.crt" 2>/dev/null

docker rm --force "$name" >/dev/null 2>&1 || true
# The server needs a key file that its own user owns.
docker run --detach --rm --name "$name" --publish "$port:5432" \
  --env POSTGRES_PASSWORD=postgres --volume "$dir:/certs:ro" \
  "postgres:$version" bash -c 'install -o postgres -m 600 /certs/server.crt /certs/server.key /tmp/ &&
    exec docker-entrypoint.sh postgres -c ssl=on \
      -c ssl_cert_file=/tmp/server.crt -c ssl_key_file=/tmp/server.key' >/dev/null

# The image starts a temporary server without TCP first, so wait for TCP.
for _ in $(seq 60); do
  docker exec "$name" pg_isready --quiet --host localhost && break
  sleep 1
done
docker exec "$name" pg_isready --quiet --host localhost

echo "SVALBARD_TEST_POSTGRESQL=postgresql://postgres:postgres@localhost:$port/postgres"
echo "SVALBARD_TEST_POSTGRESQL_CA=$dir/ca.crt"
