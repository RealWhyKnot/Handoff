// SPDX-License-Identifier: GPL-3.0-or-later
package capabilities

import (
	"context"
	"encoding/json"

	"github.com/RealWhyKnot/Handoff/internal/dispatch"
)

// RegisterNet wires read-only net.* networking-inventory handlers.
// Active probes (ping/trace/curl) are input-guarded in their own files.
func RegisterNet(r *dispatch.Router) {
	r.Register("net.adapters", netAdapters)
	r.Register("net.routes", netRoutes)
	r.Register("net.arp", netArp)
	r.Register("net.dns-cache", netDnsCache)
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
