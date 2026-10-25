#!/usr/bin/env bash
# Prepares the servers of tests/docker/ports.yaml for the clean-runner and action CI jobs: copies
# the CA, the SSH key and known_hosts to RUNNER_TEMP, creates the bucket, the SFTP directories and
# the `app` databases, and a `backup` account with a random password on both servers. The
# passwords go to GITHUB_ENV as BACKUP_PASSWORD and WRONG_PASSWORD, masked in the log.
set -euo pipefail

# Without --no-deps, Compose runs `certs` again, which replaces the CA.
docker compose up --wait --no-deps --no-recreate postgresql mysql floci ssh
docker compose cp postgresql:/certs/ca.crt "$RUNNER_TEMP/ca.crt"
docker compose cp ssh:/keys/client "$RUNNER_TEMP/ssh-key"
docker compose cp ssh:/keys/known_hosts "$RUNNER_TEMP/known_hosts"
docker compose exec -T ssh install -d -o backup /srv/backups/app /srv/backups/cancelled
aws --endpoint-url http://127.0.0.1:4566 s3 mb s3://backups
mkdir "$RUNNER_TEMP/staging" "$RUNNER_TEMP/out" "$RUNNER_TEMP/restore"

password=$(openssl rand -hex 16)
wrong=$(openssl rand -hex 16)
echo "::add-mask::$password"
echo "::add-mask::$wrong"
printf 'BACKUP_PASSWORD=%s\nWRONG_PASSWORD=%s\n' "$password" "$wrong" >>"$GITHUB_ENV"
# The passwords go through stdin, never through arguments.
docker compose exec -T postgresql psql --quiet --username=postgres --set=ON_ERROR_STOP=1 <<SQL
CREATE ROLE backup LOGIN PASSWORD '$password' IN ROLE pg_read_all_data;
CREATE DATABASE app;
\c app
CREATE TABLE items (id int PRIMARY KEY, name text NOT NULL);
INSERT INTO items SELECT i, md5(i::text) FROM generate_series(1, 1000) AS i;
SQL
docker compose exec -T --env MYSQL_PWD=mysql mysql mysql --user=root <<SQL
CREATE DATABASE app;
USE app;
CREATE TABLE items (id INT PRIMARY KEY, name VARCHAR(32) NOT NULL);
INSERT INTO items WITH RECURSIVE n (i) AS (
  SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 1000
) SELECT i, CONCAT('item ', i) FROM n;
CREATE USER backup IDENTIFIED BY '$password';
GRANT SELECT, SHOW VIEW, TRIGGER, EVENT ON app.* TO backup;
GRANT SHOW_ROUTINE ON *.* TO backup;
GRANT SELECT ON performance_schema.metadata_locks TO backup;
GRANT SELECT ON performance_schema.threads TO backup;
GRANT SELECT ON performance_schema.user_variables_by_thread TO backup;
SQL
