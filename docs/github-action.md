---
title: "GitHub Action"
description: "Install a released Svalbard and run a backup in one step."
---

The Action in [`action/`](../action/action.yml) installs a released Svalbard and checks the archive against its SHA-256 in [`action/checksums.txt`](../action/checksums.txt).
It can install the database clients, and then it runs one backup with `exec`.
[`examples/github-actions/backup-action.yml`](../examples/github-actions/backup-action.yml) is a complete workflow.

```yaml
- uses: paulrose/svalbard/action@<commit> # v0.1.0
  with:
    version: 0.1.0
    config: svalbard.toml
    postgresql: "18"
  env:
    SVALBARD_SOURCE_PASSWORD: ${{ secrets.DB_PASSWORD }}
```

Pin the Action to a commit. Each commit of the Action installs only the Svalbard versions in its `checksums.txt`.

## Inputs

| Input | Meaning |
| --- | --- |
| `version` | Required. The Svalbard version, such as `0.1.0`. |
| `config` | Required. The configuration file. |
| `postgresql` | Installs `pg_dump` of this major version: `15` to `18`. |
| `mysql` | Installs `mysqldump` of this series: `8.4` or `9.7`. |
| `token` | The token for the release download. The default is `github.token`. |

`postgresql` and `mysql` change the system packages as in [Native clients](install.md#native-clients), so they work only on GitHub's `ubuntu-24.04` runner.
On another runner, install the clients before the Action.
The other settings and the secrets go in the `env` of the step, as for the CLI. The Action has no inputs for secrets.

## Outputs

The outputs come from the JSON result: `result`, `run-id`, `data`, `record`, `size`, `sha256`, `stage` and `error`, and `json` with the complete result.
A failed backup fails the step with the exit code of Svalbard, and the outputs describe the failure.
The Action also writes the result to `$RUNNER_TEMP/svalbard-result.json`.
When the step reaches its `timeout-minutes`, GitHub stops the Action before it sets the outputs, so only the file has the result. A later step with `if: always()` can read it.
