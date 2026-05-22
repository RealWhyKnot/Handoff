# Configuration

Handoff has no config file. Behaviour is controlled by environment variables
set before running `handoff new`.

## Environment Variables

| Variable | Default | Purpose |
|---|---|---|
| `HANDOFF_RELAY` | `https://handoff.whyknot.dev` | Relay base URL. Override to use a self-hosted relay. Include the scheme; do not include a trailing slash. |
| `HANDOFF_ALLOW_PSEXEC` | (unset) | Set to `1` to enable the `ps.exec` capability. Any other value (or absent) leaves it disabled. |

## Audit Log

Every command that runs during a session is appended to a JSONL file at:

```
%PROGRAMDATA%\whyknot\handoff\audit\handoff-YYYY-MM-DD.jsonl
```

On a default Windows install this expands to:

```
C:\ProgramData\whyknot\handoff\audit\handoff-2026-05-22.jsonl
```

One file per UTC day; new days roll automatically. If `PROGRAMDATA` is unset,
the log falls back to `%TEMP%\whyknot\handoff\audit\`.

Each line is a JSON object with fields: `ts`, `sid`, `op`, `cap`, `args`,
`consent`, `result`, `elapsed_ms`, and (when an error occurred) `detail`.
