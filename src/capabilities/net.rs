use serde_json::Value;

use crate::dispatch::{handler, Args, CapResult, Ctx, HandlerError, Router};

use super::pwsh::{json_script_handler, ps_single_quote, run_pwsh_json};

const ADAPTERS: &str = r##"
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
"##;

const ROUTES: &str = r##"Get-NetRoute | Select-Object DestinationPrefix,NextHop,InterfaceAlias,RouteMetric,InterfaceMetric | ConvertTo-Json -Compress"##;

const ARP: &str = r##"Get-NetNeighbor | Where-Object {$_.State -ne 'Permanent'} | Select-Object IPAddress,LinkLayerAddress,State,InterfaceAlias | ConvertTo-Json -Compress"##;

const DNS_CACHE: &str = r##"Get-DnsClientCache | Select-Object Entry,Name,Type,Status,Section,TimeToLive,DataLength,Data | ConvertTo-Json -Compress"##;

const LISTENERS: &str = r##"
$tcp = @()
try {
    $tcp = Get-NetTCPConnection -State Listen -ErrorAction SilentlyContinue |
        ForEach-Object {
            $ownerPid = $_.OwningProcess
            $procName = $null
            if ($ownerPid -gt 0) {
                try {
                    $procName = (Get-Process -Id $ownerPid -ErrorAction SilentlyContinue).ProcessName
                } catch {}
            }

            [ordered]@{
                protocol = 'tcp'
                local_address = $_.LocalAddress
                local_port = $_.LocalPort
                remote_address = $_.RemoteAddress
                remote_port = $_.RemotePort
                state = $_.State
                pid = $ownerPid
                process_name = $procName
            }
        }
} catch {}

$udp = @()
try {
    $udp = Get-NetUDPEndpoint -ErrorAction SilentlyContinue |
        ForEach-Object {
            $ownerPid = $_.OwningProcess
            $procName = $null
            if ($ownerPid -gt 0) {
                try {
                    $procName = (Get-Process -Id $ownerPid -ErrorAction SilentlyContinue).ProcessName
                } catch {}
            }

            [ordered]@{
                protocol = 'udp'
                local_address = $_.LocalAddress
                local_port = $_.LocalPort
                remote_address = $null
                remote_port = $null
                state = $null
                pid = $ownerPid
                process_name = $procName
            }
        }
} catch {}

[ordered]@{
    tcp = @($tcp)
    udp = @($udp)
} | ConvertTo-Json -Compress -Depth 4
"##;

const CONNECTIONS_TEMPLATE: &str = r##"
$max_results = @@MAX@@
Get-NetTCPConnection@@STATE@@ |
    Select-Object LocalAddress,LocalPort,RemoteAddress,RemotePort,State,OwningProcess,CreationTime |
    ForEach-Object {
        $ownerPid = $_.OwningProcess
        $procName = $null
        if ($ownerPid -gt 0) {
            try {
                $procName = (Get-Process -Id $ownerPid -ErrorAction SilentlyContinue).ProcessName
            } catch {}
        }

        [ordered]@{
            local_address = $_.LocalAddress
            local_port = [int]$_.LocalPort
            remote_address = $_.RemoteAddress
            remote_port = [int]$_.RemotePort
            state = $_.State
            pid = $ownerPid
            process_name = $procName
            created_utc = $_.CreationTime.ToUniversalTime().ToString("o")
        }
    } |
    Sort-Object state, local_address, local_port |
    Select-Object -First $max_results |
    ConvertTo-Json -Compress
 "##;

const FIREWALL: &str = r##"
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
"##;

const WLAN: &str = r##"
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
"##;

const TLS_TEMPLATE: &str = r##"
$host_target = @@HOST@@
$port_target = @@PORT@@
$timeoutMs = @@TIMEOUT@@
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
"##;

pub fn register(r: &mut Router) {
    r.register("net.adapters", json_script_handler(ADAPTERS));
    r.register("net.routes", json_script_handler(ROUTES));
    r.register("net.arp", json_script_handler(ARP));
    r.register("net.dns-cache", json_script_handler(DNS_CACHE));
    r.register("net.listeners", json_script_handler(LISTENERS));
    r.register("net.connections", handler(net_connections));
    r.register("net.firewall", json_script_handler(FIREWALL));
    r.register("net.wlan", json_script_handler(WLAN));
    r.register("net.tls", handler(net_tls));
}

pub fn arg_string(args: &Args, name: &str) -> Option<String> {
    match args.get(name) {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    }
}

pub fn arg_int(args: &Args, name: &str) -> Option<i64> {
    args.get(name).and_then(Value::as_i64)
}

pub fn arg_bool(args: &Args, name: &str) -> Option<bool> {
    args.get(name).and_then(Value::as_bool)
}

pub fn normalize_connection_state(s: &str) -> String {
    s.trim().to_lowercase()
}

pub fn is_allowed_connection_state(state: &str) -> bool {
    matches!(
        state,
        "all"
            | "established"
            | "listen"
            | "synsent"
            | "synreceived"
            | "establishing"
            | "finwait1"
            | "finwait2"
            | "closewait"
            | "closing"
            | "lastack"
            | "timewait"
            | "deletetcb"
            | "delete-tcb"
    )
}

pub fn resolve_connection_state(raw: &str) -> Result<String, HandlerError> {
    let mut state = normalize_connection_state(raw);
    if state.is_empty() {
        state = "established".to_string();
    }
    if !is_allowed_connection_state(&state) {
        return Err(HandlerError::plain(
            "net.connections: state must be established, listen, all, or a valid TCP state",
        ));
    }
    Ok(state)
}

pub fn connections_script(args: &Args) -> Result<String, HandlerError> {
    let raw_state = arg_string(args, "state").unwrap_or_default();
    let mut max_results = arg_int(args, "max_results").unwrap_or(200);

    let state = resolve_connection_state(&raw_state)?;
    if max_results <= 0 {
        max_results = 1;
    }
    if max_results > 1000 {
        max_results = 1000;
    }

    let state_filter = if state != "all" {
        format!(" -State {}", ps_single_quote(&state))
    } else {
        String::new()
    };

    Ok(CONNECTIONS_TEMPLATE
        .replace("@@MAX@@", &max_results.to_string())
        .replace("@@STATE@@", &state_filter))
}

async fn net_connections(ctx: Ctx, args: Args) -> CapResult {
    let script = connections_script(&args)?;
    run_pwsh_json(&ctx.cancel, &script)
        .await
        .map_err(HandlerError::from)
}

pub fn tls_script(args: &Args) -> Result<String, HandlerError> {
    let host = arg_string(args, "host").unwrap_or_default();
    let port = arg_int(args, "port").unwrap_or(443);
    let mut timeout_ms = arg_int(args, "timeout_ms").unwrap_or(5000);
    let host = host.trim().to_string();
    if host.is_empty() {
        return Err(HandlerError::plain("net.tls: 'host' is required"));
    }
    if host.len() > 253 || host.chars().any(|c| " \t\r\n'\";<>{}\\".contains(c)) {
        return Err(HandlerError::plain(
            "net.tls: host contains invalid characters",
        ));
    }
    if port <= 0 || port > 65535 {
        return Err(HandlerError::plain("net.tls: port must be 1-65535"));
    }
    if timeout_ms <= 0 || timeout_ms > 30000 {
        timeout_ms = 5000;
    }

    Ok(TLS_TEMPLATE
        .replace("@@HOST@@", &ps_single_quote(&host))
        .replace("@@PORT@@", &port.to_string())
        .replace("@@TIMEOUT@@", &timeout_ms.to_string()))
}

async fn net_tls(ctx: Ctx, args: Args) -> CapResult {
    let script = tls_script(&args)?;
    run_pwsh_json(&ctx.cancel, &script)
        .await
        .map_err(HandlerError::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(v: Value) -> Args {
        v.as_object().cloned().unwrap_or_default()
    }

    fn err_of<T>(r: Result<T, HandlerError>) -> String {
        match r {
            Ok(_) => panic!("expected an error"),
            Err(e) => e.message,
        }
    }

    #[test]
    fn registers_the_net_inventory_kinds() {
        let mut r = Router::new();
        register(&mut r);
        assert_eq!(
            r.kinds(),
            vec![
                "net.adapters",
                "net.arp",
                "net.connections",
                "net.dns-cache",
                "net.firewall",
                "net.listeners",
                "net.routes",
                "net.tls",
                "net.wlan"
            ]
        );
        for spec in r.specs() {
            assert!(spec.params.is_empty() && spec.label.is_empty());
        }
    }

    #[tokio::test]
    async fn net_connections_rejects_invalid_state() {
        let mut r = Router::new();
        register(&mut r);
        let out = r
            .dispatch(
                Ctx::default(),
                "net.connections",
                args(json!({"state": "bad-state"})),
            )
            .await;
        assert!(!out.ok);
        assert!(out.error.contains("state must"), "{}", out.error);
    }

    #[test]
    fn resolve_connection_state_defaults_to_established() {
        assert_eq!(resolve_connection_state("").unwrap(), "established");
    }

    #[test]
    fn resolve_connection_state_normalizes_case_and_whitespace() {
        assert_eq!(resolve_connection_state("  Listen ").unwrap(), "listen");
    }

    #[test]
    fn connections_script_embeds_state_filter_and_clamped_max() {
        let script =
            connections_script(&args(json!({"state": "Listen", "max_results": 5000}))).unwrap();
        assert!(script.contains("$max_results = 1000\n"));
        assert!(script.contains("Get-NetTCPConnection -State 'listen' |"));
        assert!(!script.contains("@@"));

        let script = connections_script(&args(json!({"state": "all", "max_results": 0}))).unwrap();
        assert!(script.contains("$max_results = 1\n"));
        assert!(script.contains("Get-NetTCPConnection |"));

        let script = connections_script(&Args::new()).unwrap();
        assert!(script.contains("$max_results = 200\n"));
        assert!(script.contains("-State 'established'"));
    }

    #[test]
    fn net_tls_rejects_bad_host() {
        let msg = err_of(tls_script(&args(json!({"host": "example.com; rm -rf /"}))));
        assert!(msg.contains("invalid characters"), "{msg}");
        let msg = err_of(tls_script(&Args::new()));
        assert_eq!(msg, "net.tls: 'host' is required");
        let msg = err_of(tls_script(&args(json!({"host": "a".repeat(254)}))));
        assert!(msg.contains("invalid characters"), "{msg}");
    }

    #[test]
    fn net_tls_rejects_bad_port() {
        let msg = err_of(tls_script(&args(
            json!({"host": "example.com", "port": 70000}),
        )));
        assert!(msg.contains("port"), "{msg}");
        let msg = err_of(tls_script(&args(json!({"host": "example.com", "port": 0}))));
        assert_eq!(msg, "net.tls: port must be 1-65535");
    }

    #[test]
    fn net_tls_script_quotes_host_and_defaults_timeout() {
        let script =
            tls_script(&args(json!({"host": " example.com ", "timeout_ms": 99999}))).unwrap();
        assert!(script.contains("$host_target = 'example.com'\n"));
        assert!(script.contains("$port_target = 443\n"));
        assert!(script.contains("$timeoutMs = 5000\n"));
        assert!(!script.contains("@@"));
    }

    #[test]
    fn scripts_end_in_json_output() {
        for script in [
            ADAPTERS,
            ROUTES,
            ARP,
            DNS_CACHE,
            LISTENERS,
            CONNECTIONS_TEMPLATE,
            FIREWALL,
            WLAN,
            TLS_TEMPLATE,
        ] {
            assert!(script.contains("ConvertTo-Json -Compress"));
        }
    }
}
