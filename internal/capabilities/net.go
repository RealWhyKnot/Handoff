// SPDX-License-Identifier: GPL-3.0-or-later
package capabilities

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"

	"github.com/RealWhyKnot/Handoff/internal/dispatch"
)

// RegisterNet wires read-only net.* networking-inventory handlers.
// Active probes (ping/trace/curl) are input-guarded in their own files.
func RegisterNet(r *dispatch.Router) {
	r.Register("net.adapters", netAdapters)
	r.Register("net.routes", netRoutes)
	r.Register("net.arp", netArp)
	r.Register("net.dns-cache", netDnsCache)
	r.Register("net.listeners", netListeners)
	r.Register("net.connections", netConnections)
	r.Register("net.firewall", netFirewall)
	r.Register("net.wlan", netWlan)
	r.Register("net.tls", netTLS)
	r.Register("net.shares", netShares)
}

func netAdapters(ctx context.Context, _ map[string]json.RawMessage) (interface{}, error) {
	script := `
Get-NetAdapter | ForEach-Object {
    $cfg = Get-NetIPConfiguration -InterfaceIndex $_.ifIndex -ErrorAction SilentlyContinue
    [ordered]@{
        name = $_.Name
        description = $_.InterfaceDescription
        mac = $_.MacAddress
        status = $_.Status
        link_speed = $_.LinkSpeed
        ipv4 = @($cfg.IPv4Address.IPAddress)
        ipv6 = @($cfg.IPv6Address.IPAddress)
        gateway = $cfg.IPv4DefaultGateway.NextHop
        dns_servers = @($cfg.DNSServer.ServerAddresses)
    }
} | ConvertTo-Json -Compress -Depth 4
`
	return runPwshJSON(ctx, script)
}

func netRoutes(ctx context.Context, _ map[string]json.RawMessage) (interface{}, error) {
	return runPwshJSON(ctx, `Get-NetRoute | Select-Object DestinationPrefix,NextHop,InterfaceAlias,RouteMetric,InterfaceMetric | ConvertTo-Json -Compress`)
}

func netArp(ctx context.Context, _ map[string]json.RawMessage) (interface{}, error) {
	return runPwshJSON(ctx, `Get-NetNeighbor | Where-Object {$_.State -ne 'Permanent'} | Select-Object IPAddress,LinkLayerAddress,State,InterfaceAlias | ConvertTo-Json -Compress`)
}

func netDnsCache(ctx context.Context, _ map[string]json.RawMessage) (interface{}, error) {
	return runPwshJSON(ctx, `Get-DnsClientCache | Select-Object Entry,Name,Type,Status,Section,TimeToLive,DataLength,Data | ConvertTo-Json -Compress`)
}

func netListeners(ctx context.Context, _ map[string]json.RawMessage) (interface{}, error) {
	script := `
$tcp = @()
try {
    $tcp = Get-NetTCPConnection -State Listen -ErrorAction SilentlyContinue |
        ForEach-Object {
            $pid = $_.OwningProcess
            $procName = $null
            if ($pid -gt 0) {
                try {
                    $procName = (Get-Process -Id $pid -ErrorAction SilentlyContinue).ProcessName
                } catch {}
            }

            [ordered]@{
                protocol = 'tcp'
                local_address = $_.LocalAddress
                local_port = $_.LocalPort
                remote_address = $_.RemoteAddress
                remote_port = $_.RemotePort
                state = $_.State
                pid = $pid
                process_name = $procName
            }
        }
} catch {}

$udp = @()
try {
    $udp = Get-NetUDPEndpoint -ErrorAction SilentlyContinue |
        ForEach-Object {
            $pid = $_.OwningProcess
            $procName = $null
            if ($pid -gt 0) {
                try {
                    $procName = (Get-Process -Id $pid -ErrorAction SilentlyContinue).ProcessName
                } catch {}
            }

            [ordered]@{
                protocol = 'udp'
                local_address = $_.LocalAddress
                local_port = $_.LocalPort
                remote_address = $null
                remote_port = $null
                state = $null
                pid = $pid
                process_name = $procName
            }
        }
} catch {}

[ordered]@{
    tcp = @($tcp)
    udp = @($udp)
} | ConvertTo-Json -Compress -Depth 4
`
	return runPwshJSON(ctx, script)
}

func netConnections(ctx context.Context, args map[string]json.RawMessage) (interface{}, error) {
	var rawState string
	maxResults := 200

	if v, ok := args["state"]; ok {
		_ = json.Unmarshal(v, &rawState)
	}
	if v, ok := args["max_results"]; ok {
		_ = json.Unmarshal(v, &maxResults)
	}

	state, err := resolveConnectionState(rawState)
	if err != nil {
		return nil, err
	}
	if maxResults <= 0 {
		maxResults = 1
	}
	if maxResults > 1000 {
		maxResults = 1000
	}

	stateFilter := ""
	if state != "all" {
		stateFilter = " -State " + psSingleQuote(state)
	}

	script := fmt.Sprintf(`
$max_results = %d
Get-NetTCPConnection%s |
    Select-Object LocalAddress,LocalPort,RemoteAddress,RemotePort,State,OwningProcess,CreationTime |
    ForEach-Object {
        $pid = $_.OwningProcess
        $procName = $null
        if ($pid -gt 0) {
            try {
                $procName = (Get-Process -Id $pid -ErrorAction SilentlyContinue).ProcessName
            } catch {}
        }

        [ordered]@{
            local_address = $_.LocalAddress
            local_port = [int]$_.LocalPort
            remote_address = $_.RemoteAddress
            remote_port = [int]$_.RemotePort
            state = $_.State
            pid = $pid
            process_name = $procName
            created_utc = $_.CreationTime.ToUniversalTime().ToString("o")
        }
    } |
    Sort-Object state, local_address, local_port |
    Select-Object -First $max_results |
    ConvertTo-Json -Compress
 `, maxResults, stateFilter)

	return runPwshJSON(ctx, script)
}

func netFirewall(ctx context.Context, _ map[string]json.RawMessage) (interface{}, error) {
	script := `
$profiles = Get-NetFirewallProfile -ErrorAction SilentlyContinue |
    Select-Object @{n='profile';e={[string]$_.Name}},
                  @{n='enabled';e={[bool]$_.Enabled}},
                  @{n='default_inbound';e={[string]$_.DefaultInboundAction}},
                  @{n='default_outbound';e={[string]$_.DefaultOutboundAction}},
                  @{n='log_blocked';e={[bool]$_.LogBlocked}},
                  @{n='log_allowed';e={[bool]$_.LogAllowed}},
                  @{n='notify_on_listen';e={[bool]$_.NotifyOnListen}}

$rules = Get-NetFirewallRule -ErrorAction SilentlyContinue |
    Where-Object { $_.Enabled -eq 'True' } |
    Sort-Object DisplayName |
    Select-Object -First 200 |
    Select-Object @{n='name';e={[string]$_.DisplayName}},
                  @{n='direction';e={[string]$_.Direction}},
                  @{n='action';e={[string]$_.Action}},
                  @{n='profile';e={[string]$_.Profile}},
                  @{n='group';e={[string]$_.Group}}

[ordered]@{
    profiles = @($profiles)
    enabled_rules_sample = @($rules)
} | ConvertTo-Json -Compress -Depth 5
`
	return runPwshJSON(ctx, script)
}

func netWlan(ctx context.Context, _ map[string]json.RawMessage) (interface{}, error) {
	script := `
$adapters = @()
$intRaw = (netsh wlan show interfaces) 2>$null
if ($LASTEXITCODE -ne 0 -or -not $intRaw) {
    [ordered]@{
        supported = $false
        message = 'netsh wlan show interfaces failed (no wifi adapter or service stopped)'
    } | ConvertTo-Json -Compress
    return
}
$current = $null
foreach ($line in $intRaw) {
    $line = $line.Trim()
    if ($line -match '^Name\s+:\s+(.*)$') {
        if ($current) { $adapters += ,$current }
        $current = [ordered]@{ name = $matches[1] }
    } elseif ($current -and $line -match '^([A-Za-z][^:]*?)\s*:\s+(.*)$') {
        $current[$matches[1].Trim().ToLowerInvariant().Replace(' ', '_')] = $matches[2].Trim()
    }
}
if ($current) { $adapters += ,$current }

$profiles = @()
$profRaw = (netsh wlan show profiles) 2>$null
if ($profRaw) {
    foreach ($line in $profRaw) {
        if ($line -match 'All User Profile\s+:\s+(.*)$') {
            $profiles += $matches[1].Trim()
        }
    }
}

[ordered]@{
    supported = $true
    adapters  = @($adapters)
    profiles  = @($profiles)
} | ConvertTo-Json -Compress -Depth 4
`
	return runPwshJSON(ctx, script)
}

func netTLS(ctx context.Context, args map[string]json.RawMessage) (interface{}, error) {
	var host string
	port := 443
	timeoutMs := 5000
	if v, ok := args["host"]; ok {
		_ = json.Unmarshal(v, &host)
	}
	if v, ok := args["port"]; ok {
		_ = json.Unmarshal(v, &port)
	}
	if v, ok := args["timeout_ms"]; ok {
		_ = json.Unmarshal(v, &timeoutMs)
	}
	host = strings.TrimSpace(host)
	if host == "" {
		return nil, fmt.Errorf("net.tls: 'host' is required")
	}
	if len(host) > 253 || strings.ContainsAny(host, " \t\r\n'\";<>{}\\") {
		return nil, fmt.Errorf("net.tls: host contains invalid characters")
	}
	if port <= 0 || port > 65535 {
		return nil, fmt.Errorf("net.tls: port must be 1-65535")
	}
	if timeoutMs <= 0 || timeoutMs > 30000 {
		timeoutMs = 5000
	}

	script := fmt.Sprintf(`
$host_target = %s
$port_target = %d
$timeoutMs = %d
$start = Get-Date
$tcp = New-Object System.Net.Sockets.TcpClient
$out = [ordered]@{
    host        = $host_target
    port        = $port_target
    connected   = $false
    handshake_ok = $false
    elapsed_ms  = 0
}
try {
    $task = $tcp.ConnectAsync($host_target, $port_target)
    if (-not $task.Wait($timeoutMs)) {
        throw "tcp connect timed out after $timeoutMs ms"
    }
    $out.connected = $true
    $stream = $tcp.GetStream()
    $ssl = New-Object System.Net.Security.SslStream($stream, $false, ({ $true } -as [System.Net.Security.RemoteCertificateValidationCallback]))
    try {
        $ssl.AuthenticateAsClient($host_target)
        $out.handshake_ok = $true
        $cert = $ssl.RemoteCertificate
        if ($cert) {
            $cert2 = [System.Security.Cryptography.X509Certificates.X509Certificate2]$cert
            $out.protocol      = [string]$ssl.SslProtocol
            $out.cipher        = [string]$ssl.CipherAlgorithm
            $out.cipher_bits   = [int]$ssl.CipherStrength
            $out.subject       = $cert2.Subject
            $out.issuer        = $cert2.Issuer
            $out.not_before    = $cert2.NotBefore.ToUniversalTime().ToString("o")
            $out.not_after     = $cert2.NotAfter.ToUniversalTime().ToString("o")
            $out.thumbprint    = $cert2.Thumbprint
            $out.serial        = $cert2.SerialNumber
            $sanExt = $cert2.Extensions | Where-Object { $_.Oid.Value -eq '2.5.29.17' } | Select-Object -First 1
            if ($sanExt) {
                $out.san = ($sanExt.Format($false) -split ',\s*') | Where-Object { $_ }
            }
        }
    } finally {
        $ssl.Dispose()
    }
} catch {
    $out.error = $_.Exception.Message
} finally {
    $tcp.Close()
    $out.elapsed_ms = [int]((Get-Date) - $start).TotalMilliseconds
}
$out | ConvertTo-Json -Compress -Depth 5
`, psSingleQuote(host), port, timeoutMs)
	return runPwshJSON(ctx, script)
}

func normalizeConnectionState(s string) string {
	return strings.ToLower(strings.TrimSpace(s))
}

func resolveConnectionState(raw string) (string, error) {
	state := normalizeConnectionState(raw)
	if state == "" {
		state = "established"
	}
	if !isAllowedConnectionState(state) {
		return "", fmt.Errorf("net.connections: state must be established, listen, all, or a valid TCP state")
	}
	return state, nil
}

func isAllowedConnectionState(state string) bool {
	switch state {
	case "all", "established", "listen", "synsent", "synreceived", "establishing", "finwait1", "finwait2", "closewait", "closing", "lastack", "timewait", "deletetcb", "delete-tcb":
		return true
	default:
		return false
	}
}
