# Getting Started (Host)

This page covers the host side -- the person sharing their machine.

## 1. Download

Go to [Releases](https://github.com/RealWhyKnot/Handoff/releases) and download
the latest `handoff.exe`. No installer; drop it anywhere.

## 2. Start a Session

Open a terminal in the folder where `handoff.exe` lives and run:

```powershell
.\handoff.exe new
```

Output looks like:

```
relay: https://handoff.whyknot.dev

session live -- share the view URL with your helper:
   https://handoff.whyknot.dev/v/n1_AbCdEfGhIjK

press Ctrl+C to end the session at any time.

ready -- <n> capabilities registered
```

Copy the URL and send it to your helper (chat, email, phone -- any channel
works).

## 3. Watch the Console

Each command the operator queues prints to your console:

```
[cmd] cmd_1 kind=sys.info
       -> ok=true elapsed=312ms
[cmd] cmd_2 kind=hw.usb
       -> ok=true elapsed=88ms
```

Every command is also appended to the audit log so you have a record of what
ran. See [[Configuration]] for the log path.

Risky commands, such as arbitrary PowerShell execution, filesystem writes or
deletes, process termination, service control, and Pico flashing/reset actions,
open a yes/no warning popup the first time they are requested. Choosing **Yes**
allows risky commands for the remainder of the session without another prompt.
Only approve this if you trust the person using the view URL.

## 4. End the Session

Press **Ctrl+C**. The session closes immediately; the view URL stops working.

## Relay URL

The default relay is `https://handoff.whyknot.dev`. To use a different relay:

```powershell
$env:HANDOFF_RELAY = "https://your-relay.example.com"
.\handoff.exe new
```

See [[Configuration]] for all environment variables.
