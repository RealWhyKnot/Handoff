# Troubleshooting

## SmartScreen Warning on First Run

Windows may show "Windows protected your PC" when you run a freshly downloaded
`handoff.exe`. Click **More info** then **Run anyway**. The binary is not
signed with an EV certificate; SmartScreen reputation clears over time.

## pico.* Commands Return an Error

The pico commands call `picotool`. If you get:

```
pico.list: could not find picotool: exec: "picotool": executable file not found in %PATH%
```

either:
- Install [picotool](https://github.com/raspberrypi/picotool) and add it to
  `PATH`, or
- Use the embedded build (`-tags embed_picotool`). See [[Build from Source]].

## Audit Log Location

The audit log is at:

```
C:\ProgramData\whyknot\handoff\audit\handoff-YYYY-MM-DD.jsonl
```

If the directory is missing, `handoff new` creates it on first run. If it still
does not appear, check that `PROGRAMDATA` is set (`$env:PROGRAMDATA`).

## The Operator Appears Hung / No Commands Arriving

1. Confirm the host console still shows `ready -- 29 capabilities registered`.
2. Have the operator reload the viewer page -- a stale WebSocket will reconnect.
3. If the relay URL has changed, end the session (Ctrl+C), update `HANDOFF_RELAY`
   if needed, and run `handoff new` again for a fresh URL.

## Ending a Stuck Session

Press **Ctrl+C** in the host terminal. This cancels the session context
immediately. If the terminal is unresponsive, close it; the relay session
expires automatically when the bridge WebSocket disconnects.

## Session Mint Fails

```
could not mint session: ...
```

Check:
- `HANDOFF_RELAY` points to a reachable relay.
- The relay is healthy (`curl https://handoff.whyknot.dev/health`).
- No corporate proxy is blocking WebSocket upgrades.
