# Updating Handoff

## Check for a New Version

```powershell
.\handoff.exe update --check
```

This fetches `<relay>/dl/handoff-version.json`, prints the latest version and
any release notes, and exits without downloading anything.

## Download an Update

```powershell
.\handoff.exe update
```

If a newer version is available, it is downloaded from `<relay>/dl/handoff.exe`
and written next to the running binary as `handoff.exe.new`. The SHA-256 of the
download is verified against the manifest before the file is written.

Handoff does not swap a running executable on Windows. To finish the update:

1. Close any running `handoff` processes.
2. Replace `handoff.exe` with `handoff.exe.new` (rename or copy).
3. Start a new session.

## Relay Endpoints

| Endpoint | Purpose |
|---|---|
| `<relay>/dl/handoff-version.json` | Version manifest: `version`, `sha256`, `notes`, `url`. |
| `<relay>/dl/handoff.exe` | Current release binary. |

The relay in use is the value of `HANDOFF_RELAY` (or the default
`https://handoff.whyknot.dev`). See [[Configuration]].
