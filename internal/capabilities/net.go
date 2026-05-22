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
