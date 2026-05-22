# Handoff

Handoff lets a Windows user share a diagnostic session with a helper without
opening firewall ports, installing agents on both sides, or handing over
credentials.

```
Host
  runs `handoff new`
  shares the printed view URL

Helper (operator)
  opens the view URL in a browser
  queues diagnostic commands

Host
  console shows each command and result
  presses Ctrl+C to end
```

The view URL is the only shared secret. It is single-use per session; once the
host ends the session the token is gone.

## Pages

- [[Getting Started]] - host-side walkthrough from download to Ctrl+C.
- [[Operator Guide]] - how to open the view URL and queue commands.
- [[Capabilities]] - the full command catalogue with args and return shapes.
- [[Configuration]] - environment variables and audit log path.
- [[Update]] - checking for and applying updates.
- [[Build from Source]] - build with or without the embedded picotool.
- [[Troubleshooting]] - common errors and fixes.

## What Ships In A Release

| File | Purpose |
|---|---|
| `handoff.exe` | Windows CLI. Run as host or operator. |

No installer. Drop `handoff.exe` somewhere on `PATH` or run it from any
directory.
