---
title: "Development"
description: "Checks, tests, measurements, CI jobs, releases and the website."
---

## Checks

Run these checks before you submit a change:

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

The [CI workflow](../.github/workflows/ci.yml) runs the same checks on Ubuntu for each pull request and each push to `main`.
It uses the toolchain in `rust-toolchain.toml`. The `check` and `integration` jobs need no credentials or paid services.

## Integration tests

The PostgreSQL, MySQL, S3 and SSH tests run in Docker, which is their only requirement.
[`scripts/test-integration.sh`](../scripts/test-integration.sh) starts both database servers with TLS, a PostgreSQL hot standby, a MySQL replica, a MySQL server without `performance_schema`, the [Floci](https://github.com/floci-io/floci) S3 emulator and an OpenSSH server on a private network, and a runner container that installs Rust and the native clients.
The arguments are the PostgreSQL version and the MySQL series. The defaults are 18 and 8.4.
The script writes nothing to the host. At the end, it removes the containers, the volumes and the images that it pulled.

```sh
scripts/test-integration.sh
scripts/test-integration.sh 15 9.7
```

CI runs these tests against PostgreSQL 15, 16, 17 and 18, MySQL 8.4 and 9.7, Floci, and OpenSSH on Alpine Linux.

`tests/matrix.rs` runs `svalbard backup` for each source with each container destination: S3 (Floci), SFTP and SCP.
Each test downloads the backup without Svalbard, restores it with `pg_restore` or `mysql`, and compares the schema and the data, including an empty table and a larger one.
`tests/backup.rs` runs the CLI into failures: a wrong password, denied writes, a missing record, signals, a full staging filesystem, rejected credentials, and broken transfers.
Every failure must exit with an error, leave no completion record and no staged files, and keep secrets out of the output.
The matrix tests print one `matrix:` line each, with the client and server versions, in the `successes` output of the test run.

## Safety scenarios

`tests/scenarios.rs` runs the [safety scenarios](database-safety.md#scenarios) that need real servers: contention on the primary, the standby and the replica, `dump_timeout`, signals during preflight and the dump, `SIGKILL`, detection without `performance_schema`, two runs on one database, and a lost control connection.
Each test prints a `scenario:` line with the results and the timings against budgets B5 and B6.

## Workload measurements

[`scripts/measure.sh`](../scripts/measure.sh) measures the workload budgets of the contract for one PostgreSQL version and one MySQL series.
It loads the primary with pgbench or sysbench at half of the throughput that the servers sustain, runs backups back to back on the primary and on the standby or replica, and restores the last dump of each.
It prints Markdown tables: latency, throughput, errors, server CPU and I/O, the memory of Svalbard and the duration of the detection query.
On a host with 8 or more CPUs, the servers, the load and the backup get their own CPUs.
A pair takes about an hour and needs about 15 GB of Docker disk. Like the tests, the script leaves nothing behind.

```sh
scripts/measure.sh 18 8.4
```

The [Measure workflow](../.github/workflows/measure.yml) runs the script for each version pair on demand. Its runners have 2 CPUs, which the servers, the load and the backup share.

## Live storage tests

The live tests need a real bucket in Cloudflare R2 or Amazon S3. [`scripts/test-live.sh`](../scripts/test-live.sh) runs them in Docker.
They upload in one request and in parts, and check the SHA-256 checksums that the service keeps, the record, the cleanup after a cancellation and the access errors.
They also back up PostgreSQL and MySQL through the CLI and restore the dumps with the native clients, so the script starts both servers.

- R2: set `SVALBARD_TEST_R2_ENDPOINT` and `SVALBARD_TEST_R2_BUCKET`, and the keys of an R2 API token with Object Read & Write on the bucket in `SVALBARD_TEST_R2_ACCESS_KEY_ID` and `SVALBARD_TEST_R2_SECRET_ACCESS_KEY`.
- S3: set `SVALBARD_TEST_AWS_BUCKET` and `AWS_REGION`, and the credentials in `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`, with `AWS_SESSION_TOKEN` for temporary credentials.
  They need the permissions in [Storage](storage.md#amazon-s3-and-s3-compatible-services) on `svalbard-test/`, and `s3:GetObject`, `s3:DeleteObject` and `s3:ListBucket` for the checks and the cleanup.

```sh
scripts/test-live.sh r2
scripts/test-live.sh aws
```

The tests write under `svalbard-test/` and remove what they wrote.
Give the bucket a lifecycle rule that deletes objects under `svalbard-test/` after a day, for tests that fail before their cleanup, and one with `AbortIncompleteMultipartUpload`.

CI runs the live tests for pull requests from this repository, for pushes to `main`, and on demand:

- `r2` in the `svalbard-ci` bucket, with the keys in the repository secrets `R2_ACCESS_KEY_ID` and `R2_SECRET_ACCESS_KEY`, and the endpoint and bucket in the variables `R2_ENDPOINT` and `R2_BUCKET`.
- `aws-keys` in the `svalbard-testing` bucket, with the access keys in the repository secrets `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`.
- `aws-oidc` in the same bucket, with temporary credentials of the role in the repository variable `AWS_ROLE_ARN`, which the job assumes through OIDC.

## Release archive and CI setup

The `package` CI job builds the release archive with [`scripts/package.sh`](../scripts/package.sh).
The `clean-runner` job installs it on a fresh runner that builds nothing, with the native clients from [`action/clients.sh`](../action/clients.sh), which runs the commands in [Native clients](install.md#native-clients). Then it runs the steps of the [CI example](ci.md).
Its servers are those of the Docker tests, which [`tests/docker/ports.yaml`](../tests/docker/ports.yaml) publishes on 127.0.0.1.
It backs up PostgreSQL to SFTP and MySQL to S3, restores both as in [Restore](restore.md), and checks a wrong password, a cancellation by a step timeout, and that no output contains a password.
The `action` job runs the Action of the commit with a released Svalbard on the same servers. It checks a backup, a wrong password and a cancellation through the outputs of the Action.

## Pull requests

Database adapters must obey the [database safety contract](database-safety.md).

Create a branch for each change. Submit changes through a pull request.
Keep PRs focused and include relevant check results. Review the diff before a solo merge.
GitHub deletes merged head branches automatically.

## Releases

1. Change `version` in `Cargo.toml` in a pull request.
2. After the merge, when CI is green on `main`, tag the merge commit and push the tag:

   ```sh
   git tag v0.1.0 <commit>
   git push origin v0.1.0
   ```

3. The [Release workflow](../.github/workflows/release.yml) builds the archive, checks that the tag matches the version, and creates the GitHub release with the archive and `SHA256SUMS`.
4. In a pull request, add the line of the archive from `SHA256SUMS` to [`action/checksums.txt`](../action/checksums.txt), and change the version in the `action` CI job and the examples.

## Website

The website in [`site/`](../site/astro.config.mjs) is an [Astro](https://astro.build/) site with [Starlight](https://starlight.astro.build/).
It renders the splash page, and these pages from `docs/`, so the same Markdown is the documentation on GitHub and on [withsvalbard.com](https://withsvalbard.com).
Links between the pages are relative `.md` links, which work on GitHub. The site turns them into its own routes, and turns links to other files of the repository into links to GitHub.

[`scripts/site.sh`](../scripts/site.sh) runs the site in Docker, which is its only requirement.
Like the tests, it writes nothing to the host and leaves nothing behind.

```sh
scripts/site.sh dev     # http://localhost:7246, with live reload
scripts/site.sh build   # builds the site, checks every link, and runs the site tests
```

[Hatch](https://httphatch.com) serves the dev server at <https://svalbard.test>, through [`hatch.yml`](../hatch.yml).

The colours follow the estimated solar elevation in Longyearbyen, on every page. The estimate excludes weather and terrain.
The theme switch can fix light or dark instead, and the browser remembers the choice.
On the splash page, reduced-motion settings show one frame of the upload, after the snapshot has closed.
For previews, `?theme=light` or `?theme=dark` overrides the theme, and `?at=<seconds>` holds the animation at one moment.

For language models, the site serves [`/llms.txt`](https://withsvalbard.com/llms.txt): a setup checklist from [`site/src/llms.md`](../site/src/llms.md), and links to a Markdown copy of each page, such as `/docs/install.md`.
`/llms-full.txt` holds all pages in one file. The lists follow the sidebar in [`site/src/pages.mjs`](../site/src/pages.mjs), so a new page appears in both.

The logo is a Font Awesome Pro icon, which the repository must not contain.
With `FONTAWESOME_API_TOKEN` set to a Font Awesome API token, [`site/brand.mjs`](../site/brand.mjs) fetches it in the container. Without the token, the site uses the original mark in `site/src/assets/`.

`site/package.json` pins the dependencies. After a change to it, `scripts/site.sh lock` updates `site/package-lock.json`.
The `site` CI job builds the site for each pull request, and the [Pages workflow](../.github/workflows/pages.yml) publishes `main` to withsvalbard.com.
