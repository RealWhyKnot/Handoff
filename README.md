# Handoff

Handoff is a Windows command-line tool for remote debugging through a relay. The person with the problem (the host) runs `handoff new`, which creates a one-time session on the relay, prints a view URL and starts a local bridge. Whoever is helping opens that URL in a browser or runs `handoff connect <url>`. Then they queue diagnostic commands. Each one runs on the host's machine and its output streams back as it comes in.

The host doesn't need to open firewall ports or set up a VPN. The view token in the URL is the only credential that gets shared.

Risky commands ask the host first: arbitrary PowerShell, deleting files, killing processes, service control, and Pico flash or reset. A yes allows them for the rest of the session and a no blocks them until it ends.

The host console shows every command and its result. Ctrl+C or `q` in that console ends the session.

## Install

Download `handoff.exe` from [Releases](https://github.com/RealWhyKnot/Handoff/releases) and run it from a terminal. There's no installer. It uses the relay at [https://handoff.whyknot.dev/](https://handoff.whyknot.dev/) unless you set `HANDOFF_RELAY` to another one.

## Build from source

```powershell
git clone https://github.com/RealWhyKnot/Handoff
cd Handoff
go build -o handoff.exe .
```

The `pico.*` commands call the `picotool` installed on the host. To build it into the exe instead:

```powershell
./scripts/fetch-picotool.ps1
go build -tags embed_picotool -o handoff.exe .
```

## Update

```powershell
./handoff.exe update --check    # check the relay for a newer release
./handoff.exe update            # download it next to the running exe
```

## License

GPL-3.0-or-later. Source at <https://github.com/RealWhyKnot/Handoff>.
