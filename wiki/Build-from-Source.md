# Build from Source

## Prerequisites

- [Go 1.22 or later](https://go.dev/dl/)
- Git

## Default Build

```powershell
git clone https://github.com/RealWhyKnot/Handoff
cd Handoff
go build -o handoff.exe .
```

The resulting `handoff.exe` uses the system-installed `picotool` for `pico.*`
commands. If `picotool` is not on `PATH`, those commands return an error.

## Embedded Picotool Build

To bundle `picotool` into the binary so the host does not need it installed
separately, fetch the prebuilt binary first, then build with the embed tag:

```powershell
./scripts/fetch-picotool.ps1
go build -tags embed_picotool -o handoff-embedded.exe .
```

`scripts/fetch-picotool.ps1` downloads the appropriate picotool release for
the current platform and places it where the embed tag expects to find it.

## Version Stamp

The binary version shown by `handoff version` and used by `handoff update
--check` is baked in at build time. Releases use a `go build -ldflags` stamp;
ad-hoc builds report the default in `cmd/new.go` (`0.1.0`) unless you pass:

```powershell
go build -ldflags "-X 'github.com/RealWhyKnot/Handoff/cmd.Version=0.1.x'" -o handoff.exe .
```
