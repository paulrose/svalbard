---
title: "Run in CI"
description: "Run backups from CI: secrets, known_hosts, one backup per database, cancellation and results."
---

A CI job installs the [archive](install.md) and the [native clients](install.md#native-clients), gives the secrets in environment variables, and runs `svalbard backup --json`.
The exit code fails the job when the backup fails.
[`examples/github-actions/backup-cli.yml`](../examples/github-actions/backup-cli.yml) is a complete GitHub Actions workflow for PostgreSQL to SFTP.
The [GitHub Action](github-action.md) does the installation and the backup in one step.
The runner must reach the database and the destination.

## Secrets

- Database password: `SVALBARD_SOURCE_PASSWORD: ${{ secrets.DB_PASSWORD }}` in the `env` of the backup step.
- R2: `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` from secrets, in the same way.
- S3: access keys or a role, whichever your account uses. Both need the permissions in [Storage](storage.md#amazon-s3-and-s3-compatible-services).
  - Access keys: `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` from secrets, and the region in `destination.region` or `AWS_REGION`.
  - A role through OIDC: give the job `id-token: write`, and assume the role before the backup.
    The action sets temporary credentials with `AWS_SESSION_TOKEN`, and the region:

    ```yaml
    - uses: aws-actions/configure-aws-credentials@e1253824e5c10ff9df46874f81ed3ec929e19cfd # v6.3.0
      with:
        role-to-assume: arn:aws:iam::123456789012:role/svalbard-backup
        aws-region: eu-west-1
    ```

    The `sub` condition of the role's trust policy must match the subject of the repository's tokens.
    `gh api repos/<owner>/<repo>/actions/oidc/customization/sub` shows its prefix, which can hold the IDs of the owner and the repository, such as `repo:owner@123/repo@456`.
- SSH: write the private key from a secret to a file that only the job's user can read, and set `SVALBARD_DESTINATION_IDENTITY_FILE` to it.
  The runner deletes `$RUNNER_TEMP` after the job.
- known_hosts: commit the server's line next to the configuration. It is public, and a review shows every change.
  Take the line from the server, for example from `/etc/ssh/ssh_host_ed25519_key.pub`, not from an unchecked `ssh-keyscan`.
  For a port other than 22, the host is `[backup.example.com]:2222`.

GitHub masks secrets in the log, and no Svalbard output contains them.

## One backup per database

```yaml
concurrency:
  group: svalbard-db.example.com-app
  cancel-in-progress: false
```

GitHub runs one job of a concurrency group at a time, in one repository.
A new run waits for the running backup and never cancels it. A newer run replaces a run that waits, so at most one backup waits.
Use the same group in every workflow of the repository that backs up the database.

## Cancellation and timeouts

When GitHub cancels a job, or a job or step reaches `timeout-minutes`, it sends SIGINT and then SIGTERM to the entry process of the running step only.
After 10 s, it kills the processes of the step.
Start Svalbard with `exec`, so that the entry process is Svalbard and it can clean up. Without `exec`, the shell gets the signals, and Svalbard stops without cleanup.

Give the job a `timeout-minutes` above `safety.dump_timeout` and `destination.upload_timeout` together, so that the limits of Svalbard end a slow backup first.

## Results and failures

With `--json`, stdout gets the result, and the log shows it. Progress and errors go to stderr.
To use the result in later steps, write stdout to a file:

```yaml
- name: Back up
  id: backup
  run: exec svalbard backup --config svalbard.toml --json >"$RUNNER_TEMP/backup.json"
- name: Show the result
  if: always() && steps.backup.outcome != 'skipped'
  run: jq . "$RUNNER_TEMP/backup.json"
```

A failed backup exits with 4 or 5 (see [Output and exit codes](backup.md#output-and-exit-codes)), so the step and the job fail.
After 4, the next run can start normally. After 5, read the message: a backup session, the run directory or an unfinished upload may remain.
