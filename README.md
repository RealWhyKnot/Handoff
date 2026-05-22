## Handoff

Handoff is a Windows CLI tool for token-gated remote debug sessions. The host
runs `handoff new`, which mints a one-time session on the relay and starts a
local bridge loop. The view URL is passed to an operator, who opens it in a
browser (or runs `handoff connect <url>`) to queue diagnostic commands.
Commands run on the host and results stream back through the relay in real
time -- no inbound firewall rules, no VPN, no shared credentials beyond the
view token.

Risky commands, such as arbitrary PowerShell execution, file deletion, process
termination, service control, and Pico flashing/reset actions, require a host
yes/no warning prompt. A yes allows risky commands for the remainder of that
session; a no blocks them for that session.

**Flow:** host runs `handoff new` and shares the printed URL -- operator opens
the URL and queues commands -- host console shows each command and result --
host presses Ctrl+C or types `q` to end the session.

### Install

Download the latest `handoff.exe` from
[Releases](https://github.com/RealWhyKnot/Handoff/releases) and run it from a
terminal. No installer. The default relay is
[https://handoff.whyknot.dev/](https://handoff.whyknot.dev/); set
`HANDOFF_RELAY` to point at a different one.

### Build from source

```powershell
git clone https://github.com/RealWhyKnot/Handoff
cd Handoff
go build -o handoff.exe .
```

The default build calls into the system-installed `picotool` for the `pico.*`
commands. To bundle picotool into the binary (so the host doesn't need to
install it separately):

```powershell
./scripts/fetch-picotool.ps1
go build -tags embed_picotool -o handoff.exe .
```

### Update

```powershell
./handoff.exe update --check    # check the relay for a newer release
./handoff.exe update            # download it next to the running exe
```

### License

GPL-3.0-or-later. Source at <https://github.com/RealWhyKnot/Handoff>.
