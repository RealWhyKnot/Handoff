# Capabilities

Operator-facing capabilities registered by `handoff new`. Commands marked
**risky** are blocked until the host approves the warning prompt for the
current session.

---

## Risky Command Consent

The first risky command in a session opens a yes/no warning popup on the host.
The warning explains that choosing **Yes** allows risky commands for the
remainder of that `handoff new` session without another prompt. Choosing **No**
blocks risky commands for the remainder of the session.

Risky commands include arbitrary PowerShell execution, filesystem writes and
deletes, process termination, service control, and Pico state-changing commands.
Readonly inventory commands do not prompt.

---

## System

### `sys.info`
Returns the output of `Get-ComputerInfo` as JSON (depth 4). No args.

### `sys.uptime`
Returns boot time (UTC ISO-8601), uptime in seconds, and last-boot in local
time. No args.

### `sys.resources`
Returns a quick resource snapshot: sampled time, total CPU percent, memory
totals/free/used percent, pagefile usage, and top processes by memory and
cumulative CPU.

| Arg | Type | Default | Notes |
|---|---|---|---|
| `top` | int | `10` | Number of top processes to return; clamped to 1-50. |

### `sys.hotfixes`
Installed Windows hotfixes sorted newest first. No args.

### `sys.reboot-required`
Checks common Windows pending-reboot markers. No args.

### `sys.env`
Environment variables by scope.

| Arg | Type | Default | Notes |
|---|---|---|---|
| `scope` | string | `all` | `all`, `machine`, `user`, or `process`. |
| `name_prefix` | string | empty | Optional prefix filter. |

### `sys.users`
Logged-on account and terminal-session snapshot. No args.

### `sys.timezone`
Time zone, current local/UTC time, culture, and daylight-saving state. No args.

---

## Hardware

### `hw.cpu`
CPU name, core count, logical processor count, max clock speed, and
manufacturer from WMI `Win32_Processor`. No args.

### `hw.ram`
Per-DIMM slot info: device locator, size (GB), speed, manufacturer, and part
number from WMI `Win32_PhysicalMemory`. No args.

### `hw.usb`
All present PnP devices with instance IDs starting `USB\`: instance ID,
friendly name, status, class, and manufacturer. No args.

### `hw.disks`
Physical disk inventory: device ID, friendly name, serial number, size (GB),
media type, health status, operational status, and bus type. No args.

### `hw.gpu`
GPU name, adapter RAM, driver version, video processor, and status from
`Win32_VideoController`. No args.

---

## Storage

### `storage.volumes`
Volume inventory from `Get-Volume`: drive letter, label, filesystem, size/free
GB, free percent, health, operational status, path, and unique ID.

| Arg | Type | Default | Notes |
|---|---|---|---|
| `drive_letter` | string | empty | Optional single drive letter such as `C` or `C:`. |

---

## Network (inventory)

### `net.adapters`
Per-adapter: name, description, MAC, status, link speed, IPv4/IPv6 addresses,
default gateway, and DNS servers. No args.

### `net.routes`
Full routing table: destination prefix, next hop, interface alias, route
metric, and interface metric. No args.

### `net.arp`
Non-permanent ARP/NDP cache entries: IP address, link-layer address, state,
and interface alias. No args.

### `net.dns-cache`
DNS client cache snapshot: entry, name, record type, status, section, TTL,
data length, and data. No args.

### `net.listeners`
TCP listeners and UDP endpoints with local address/port, owning PID, and
process name where available. No args.

### `net.connections`
Current TCP connections with address/port pairs, state, owning PID, process
name, and creation time.

| Arg | Type | Default | Notes |
|---|---|---|---|
| `state` | string | `established` | `all`, `listen`, `established`, or a valid TCP state. |
| `max_results` | int | `200` | Clamped to 1-1000. |

### `net.firewall`
Firewall profile state plus a sample of enabled firewall rules. No args.

### `net.wlan`
Wi-Fi adapter and profile snapshot via `netsh wlan`. No args.

### `net.tls`
TCP and TLS handshake check for a remote host, including certificate details
when the handshake succeeds.

| Arg | Type | Default | Notes |
|---|---|---|---|
| `host` | string | required | Hostname or IP; shell metacharacters are rejected. |
| `port` | int | `443` | TCP port, 1-65535. |
| `timeout_ms` | int | `5000` | Clamped to a safe default if out of range. |

### `net.shares`
SMB share inventory, with optional SMB session and open-file samples. Share,
session, and open-file collection errors are returned in the payload instead of
failing the whole command.

| Arg | Type | Default | Notes |
|---|---|---|---|
| `include_hidden` | bool | `false` | Include hidden SMB shares. |
| `include_sessions` | bool | `true` | Include SMB sessions and open files when permitted. |
| `max_results` | int | `200` | Clamped to 1-1000 per section. |

---

## Network (probes)

### `net.ping`
Sends ICMP pings to a named host or IP.

| Arg | Type | Default | Notes |
|---|---|---|---|
| `target` | string | required | Hostname or IP; shell metacharacters are rejected. |
| `count` | int | `4` | Clamped to 1-10. |

Returns: address, response time per reply, and status code.

### `net.trace`
Runs a traceroute to a named host or IP using `Test-NetConnection -TraceRoute`.

| Arg | Type | Notes |
|---|---|---|
| `target` | string | Hostname or IP; shell metacharacters are rejected. |

Returns: computer name, remote address, ping success, ping reply details, and
hop list. Can take 10-30 seconds on slow paths.

### `net.tcp-test`
Tests TCP connectivity to a host and port. Returns DNS resolution results, the
selected remote address, success state, elapsed milliseconds, and error text
when the connection fails.

| Arg | Type | Default | Notes |
|---|---|---|---|
| `target` | string | required | Hostname or IP; shell metacharacters are rejected. |
| `port` | int | required | TCP port, 1-65535. |
| `timeout_ms` | int | `5000` | Clamped to 1000-30000. |

### `net.curl`
Performs a GET or HEAD request to a public URL and returns the response.

| Arg | Type | Default | Notes |
|---|---|---|---|
| `url` | string | required | Must be `http://` or `https://`. |
| `method` | string | `GET` | `GET` or `HEAD` only. |

- **SSRF guard:** the resolved IP must be a public unicast address. RFC1918,
  loopback, link-local, CGNAT (100.64/10), multicast, and IPv6 ULA are
  rejected -- including after redirects.
- Response body returned as base64 for GET, capped at 1 MiB; truncated if
  larger. HEAD returns headers only.

---

## Processes and Services

### `proc.list`
All running processes: PID, name, executable path, command line, working-set
size (MB), and creation time from WMI `Win32_Process`. No args.

### `proc.find`
Finds processes by name, executable path, or command line.

| Arg | Type | Default | Notes |
|---|---|---|---|
| `query` | string | required | Search text; regex-escaped before use. |
| `max_results` | int | `100` | Clamped to 1-500. |

### `proc.kill`
**Risky.** Terminates a process by PID.

| Arg | Type | Notes |
|---|---|---|
| `pid` | int | Required. Process ID to terminate with `Stop-Process -Force`. |

Returns: PID, process name, executable path when available, and `killed=true`.

### `svc.list`
All Windows services: name, display name, status, and start type. No args.

### `svc.control`
**Risky.** Starts, stops, or restarts a Windows service.

| Arg | Type | Notes |
|---|---|---|
| `name` | string | Required. Windows service name. |
| `action` | string | Required. One of `start`, `stop`, or `restart`. |

Returns: service name, display name, requested action, status before, and status
after.

---

## Events

### `evt.snapshot`
Reads recent Windows Event Log entries from a named channel.

| Arg | Type | Default | Notes |
|---|---|---|---|
| `channel` | string | `System` | See allowlist below. |
| `max_events` | int | `200` | Clamped to 1-5000. |
| `since_minutes` | int | `60` | Events newer than this many minutes. |

**Channel allowlist:** `System`, `Application`, `Setup`,
`Microsoft-Windows-Kernel-PnP/Configuration`,
`Microsoft-Windows-USB-USBHUB3-Analytic`. `Security` is in the allowlist
mapping but is set to `false`; requests for it are rejected.

Returns: time (UTC ISO-8601), level, event ID, provider name, and message per
event.

### `evt.providers`
Lists known Windows Event Log channels and record counts.

| Arg | Type | Default | Notes |
|---|---|---|---|
| `name_prefix` | string | empty | Optional channel-name prefix filter. |
| `max_results` | int | `400` | Clamped to 1-4000. |

---

## Drivers

### `drv.list`
Currently-bound drivers per device from WMI `Win32_PnpSignedDriver`: device
name, class, driver version, driver date, manufacturer, signer, and INF name.
No args.

---

## Filesystem (read)

### `fs.ls`
Lists directory entries.

| Arg | Type | Notes |
|---|---|---|
| `path` | string | Required. Absolute path to a directory. |

Returns: path and an array of entries (name, is-directory, size, mode, mtime UTC).

### `fs.read`
Reads a file and returns its content as base64 plus a SHA-256 hash.

| Arg | Type | Notes |
|---|---|---|
| `path` | string | Required. Absolute path to a file. |

- Capped at 8 MiB; larger files are refused.
- Certain credential-holding system paths are always refused regardless of size
  (`\Windows\System32\config\`, `\Windows\System32\configstore\`,
  `\Users\All Users\Microsoft\Crypto\`).

### `fs.search`
Searches a directory tree for names matching a glob pattern.

| Arg | Type | Default | Notes |
|---|---|---|---|
| `path` | string | required | Absolute directory path. |
| `pattern` | string | `*` | Filepath glob pattern. |
| `max_results` | int | `200` | Clamped to 1-2000. |
| `max_depth` | int | `4` | Clamped to 0-20. |
| `include_dirs` | bool | `false` | Include directories in matches. |
| `include_hidden` | bool | `false` | Include dot-prefixed entries. |

### `fs.head` / `fs.tail`
Returns the first or last lines of a text file without reading the whole file.

| Arg | Type | Default | Notes |
|---|---|---|---|
| `path` | string | required | Absolute file path. |
| `lines` | int | `80` | Clamped to 1-5000. |

### `fs.stat`
Returns file metadata including size, mtime, mode, directory/symlink state, and
symlink target when applicable.

| Arg | Type | Notes |
|---|---|---|
| `path` | string | Required. Absolute path. |

### `fs.tree`
Returns a bounded directory tree.

| Arg | Type | Default | Notes |
|---|---|---|---|
| `path` | string | required | Absolute directory path. |
| `max_depth` | int | `3` | Clamped to 1-8. |
| `max_entries` | int | `500` | Clamped to 50-5000. |

---

## Filesystem (write)

### `fs.upload`
**Risky.** Opens the host risky-command prompt before writing.

Writes a file to the host from operator-supplied bytes (base64-encoded). Capped
at the relay's per-command body limit (2 MiB in v0.1).

| Arg | Type | Notes |
|---|---|---|
| `path` | string | Required. Destination path on the host. |
| `data_base64` | string | Required. Base64-encoded file content. |
| `sha256` | string | Optional. If supplied, the decoded bytes must match. |
| `overwrite` | bool | Default `false`. Set to `true` to replace an existing file. |

- Refuses writes under `C:\Windows\System32\`, `C:\Windows\SysWow64\`,
  `C:\Program Files\`, and `C:\Program Files (x86)\`.

### `fs.mkdir`
**Risky.** Creates a directory on the host.

| Arg | Type | Notes |
|---|---|---|
| `path` | string | Required. Directory path to create. Parent directories are created as needed. |

Uses the same protected-location guard as `fs.upload`.

### `fs.delete`
**Risky.** Deletes a file or directory from the host.

| Arg | Type | Default | Notes |
|---|---|---|---|
| `path` | string | required | Absolute path to delete. |
| `recursive` | bool | `false` | Required for directories. |

- Refuses relative paths.
- Refuses drive roots, the current user's profile root, Windows roots, Program
  Files roots, and the same protected system paths as `fs.upload`.

### `fs.download`
Same as `fs.read` -- reads a file from the host and returns it to the operator.
Exists as a separate command so chunked transport can be added to the download
path in a future revision without changing the `fs.read` contract.

| Arg | Type | Notes |
|---|---|---|
| `path` | string | Required. Absolute path to a file. |

---

## Registry and Tasks

### `reg.query`
Reads values and immediate subkeys from a registry key, or recursively walks a
bounded subtree.

| Arg | Type | Default | Notes |
|---|---|---|---|
| `hive` | string | `HKLM` | `HKLM`, `HKCU`, `HKCR`, `HKU`, or `HKCC`. |
| `key` | string | required | Registry key path below the hive. |
| `value` | string | empty | Optional specific value name. |
| `recursive` | bool | `false` | Walk child keys when true. |
| `max_results` | int | `200` | Clamped to 1-2000. |

### `task.list`
Lists scheduled tasks with state, author, description, run times, triggers, and
actions.

| Arg | Type | Default | Notes |
|---|---|---|---|
| `path_prefix` | string | empty | Optional task-path prefix filter. |
| `state` | string | `all` | `all`, `ready`, `running`, `disabled`, `queued`, or `unknown`. |
| `max_results` | int | `300` | Clamped to 1-2000. |

---

## Security and Updates

### `update.history`
Returns the last Windows Update history entries from the Microsoft Update COM
API. No args.

### `defender.status`
Windows Defender status, signature freshness, recent scan times, and selected
preference details. Returns `available=false` when Defender cmdlets are not
available. No args.

### `sec.local-admins`
Lists members of the local Administrators group with name, object class, SID,
and principal source. Returns `available=false` if `Get-LocalGroupMember` is
not available in the host PowerShell. No args.

---

## Applications and Startup

### `app.list`
Lists installed Win32 uninstall entries and current-user AppX packages.

| Arg | Type | Default | Notes |
|---|---|---|---|
| `name_prefix` | string | empty | Optional case-insensitive name prefix. |
| `max_results` | int | `300` | Clamped to 1-5000. |

### `startup.list`
Lists startup entries from `Win32_StartupCommand`: name, command, location,
user, user SID, and description.

| Arg | Type | Default | Notes |
|---|---|---|---|
| `max_results` | int | `300` | Clamped to 1-2000. |

---

## Pico

These commands shell out to `picotool`. If `picotool` is not on `PATH` and the
binary was not built with `-tags embed_picotool`, the commands return
`ok: false` with a descriptive error.

### `pico.list`
Runs `picotool info -a` and returns the raw text output. No args.

### `pico.info`
Runs `picotool info -a -m -d -l` against a specific device.

| Arg | Type | Notes |
|---|---|---|
| `serial` | string | Optional. Picotool `--id` value to target a specific board. |

### `pico.bootsel`
**Risky.**

Reboots the attached Pico into BOOTSEL mode (`picotool reboot -f -u`). No args.

### `pico.flash`
**Risky.**

Flashes a UF2 file (`picotool load -fx`).

| Arg | Type | Notes |
|---|---|---|
| `uf2_path` | string | Required. Absolute path to the UF2 file on the host. |

### `pico.save`
**Risky.**

Saves the current Pico flash to a local file (`picotool save -a`).

| Arg | Type | Notes |
|---|---|---|
| `out_path` | string | Required. Destination path for the saved binary. |

### `pico.reset`
**Risky.**

Reboots the attached Pico normally (`picotool reboot`). No args.

---

## PowerShell Exec

### `ps.exec`
**Risky.** Runs an arbitrary PowerShell script on the host and returns combined
stdout. The first risky command in the session asks the host to approve risky
commands for the remainder of the session.

| Arg | Type | Notes |
|---|---|---|
| `script` | string | Required. Script text; capped at 16 KiB. |

Rate-limited to 10 executions per rolling minute per session.
