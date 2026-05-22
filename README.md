## Handoff

Handoff is a Windows CLI tool for token-gated remote debug sessions. The host
runs `handoff new`, which mints a one-time session on the relay and starts a
local agent loop. The view URL is passed to an operator, who opens it in a
browser (or runs `handoff connect <url>`) to queue diagnostic commands.
Commands run on the host and results stream back through the relay in real
time -- no inbound firewall rules, no VPN, no shared credentials beyond the
view token.

**Flow:** host runs `handoff new` and shares the printed URL -- operator opens
the URL and queues commands -- host console shows each command and result --
host presses Ctrl+C or types `q` to end the session.

GPL-3.0-or-later. Source at <https://github.com/RealWhyKnot/Handoff>.
