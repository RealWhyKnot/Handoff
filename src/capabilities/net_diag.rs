use serde_json::{json, Map, Value};

use crate::dispatch::{handler, Args, CapResult, Ctx, HandlerError, Router};

use super::pwsh_out::{json_script_handler, run_pwsh_json};

const WIFI: &str = r##"
$lines = @(netsh wlan show interfaces 2>&1 | ForEach-Object { [string]$_ })
$code = $LASTEXITCODE
$svc = Get-Service -Name WlanSvc -ErrorAction SilentlyContinue
[ordered]@{
    exit_code = $code
    service = if ($svc) { [string]$svc.Status } else { 'NotInstalled' }
    lines = $lines
} | ConvertTo-Json -Compress -Depth 3
"##;

const PROXY: &str = r##"
$is = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Internet Settings'
$p = Get-ItemProperty -LiteralPath $is -ErrorAction SilentlyContinue
$flags = $null
$conn = (Get-ItemProperty -LiteralPath "$is\Connections" -Name DefaultConnectionSettings -ErrorAction SilentlyContinue).DefaultConnectionSettings
if ($conn -and $conn.Length -gt 8) { $flags = [int]$conn[8] }
$wh = @(netsh winhttp show proxy 2>&1 | ForEach-Object { [string]$_ })
$whCode = $LASTEXITCODE
[ordered]@{
    user = [ordered]@{
        proxy_enabled = [bool]$p.ProxyEnable
        proxy_server = if ($p.ProxyServer) { [string]$p.ProxyServer } else { $null }
        bypass = if ($p.ProxyOverride) { [string]$p.ProxyOverride } else { $null }
        auto_config_url = if ($p.AutoConfigURL) { [string]$p.AutoConfigURL } else { $null }
        auto_detect = if ($null -ne $flags) { [bool]($flags -band 8) } else { $null }
    }
    winhttp_exit_code = $whCode
    winhttp_lines = $wh
    environment = [ordered]@{
        HTTP_PROXY = $env:HTTP_PROXY
        HTTPS_PROXY = $env:HTTPS_PROXY
        ALL_PROXY = $env:ALL_PROXY
        NO_PROXY = $env:NO_PROXY
    }
} | ConvertTo-Json -Compress -Depth 4
"##;

const PROFILE: &str = r##"
$rows = @(Get-NetConnectionProfile -ErrorAction SilentlyContinue | Sort-Object InterfaceIndex | ForEach-Object {
    [ordered]@{
        name = [string]$_.Name
        interface = [string]$_.InterfaceAlias
        interface_index = [int]$_.InterfaceIndex
        category = [string]$_.NetworkCategory
        ipv4 = [string]$_.IPv4Connectivity
        ipv6 = [string]$_.IPv6Connectivity
        domain_authentication = [string]$_.DomainAuthenticationKind
    }
})
ConvertTo-Json -InputObject $rows -Compress -Depth 3
"##;

pub fn register(r: &mut Router) {
    super::register_specs(
        r,
        include_str!("specs/net_diag.json"),
        vec![
            ("net.wifi", handler(net_wifi)),
            ("net.proxy", handler(net_proxy)),
            ("net.profile", json_script_handler(PROFILE)),
        ],
    );
}

fn split_field(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim_end();
    let (key, value) = match trimmed.find(" : ") {
        Some(i) => (&trimmed[..i], &trimmed[i + 3..]),
        None => (trimmed.strip_suffix(" :")?, ""),
    };
    let key = key.trim();
    if key.is_empty() {
        return None;
    }
    Some((key.to_string(), value.trim().to_string()))
}

fn number(s: &str) -> Value {
    let t = s.trim().trim_end_matches('%').trim();
    if let Ok(n) = t.parse::<i64>() {
        return json!(n);
    }
    match t.parse::<f64>() {
        Ok(f) if f.is_finite() => json!(f),
        _ => Value::Null,
    }
}

fn wifi_field(key: &str) -> Option<&'static str> {
    Some(match key.to_ascii_lowercase().as_str() {
        "name" => "name",
        "description" => "description",
        "physical address" => "mac",
        "state" => "state",
        "ssid" => "ssid",
        "ap bssid" | "bssid" => "bssid",
        "band" => "band",
        "channel" => "channel",
        "radio type" => "radio_type",
        "authentication" => "authentication",
        "cipher" => "cipher",
        "connection mode" => "connection_mode",
        "receive rate (mbps)" => "receive_mbps",
        "transmit rate (mbps)" => "transmit_mbps",
        "signal" => "signal_percent",
        "rssi" => "rssi_dbm",
        "profile" => "profile",
        _ => return None,
    })
}

pub fn parse_wifi_interfaces(lines: &[String]) -> Vec<Value> {
    let mut out = Vec::new();
    let mut current: Option<Map<String, Value>> = None;
    for line in lines {
        let Some((key, value)) = split_field(line) else {
            continue;
        };
        let lower = key.to_ascii_lowercase();
        if lower == "name" {
            if let Some(done) = current.take() {
                out.push(Value::Object(done));
            }
            current = Some(Map::new());
        }
        if lower.starts_with("hosted network") {
            if let Some(done) = current.take() {
                out.push(Value::Object(done));
            }
            continue;
        }
        let Some(row) = current.as_mut() else {
            continue;
        };
        match wifi_field(&key) {
            Some(
                name @ ("channel" | "receive_mbps" | "transmit_mbps" | "signal_percent"
                | "rssi_dbm"),
            ) => {
                row.insert(name.to_string(), number(&value));
            }
            Some(name) => {
                row.insert(name.to_string(), Value::String(value));
            }
            None => {
                if let Some(m) = row
                    .entry("other")
                    .or_insert_with(|| Value::Object(Map::new()))
                    .as_object_mut()
                {
                    m.insert(key, Value::String(value));
                }
            }
        }
    }
    if let Some(done) = current.take() {
        out.push(Value::Object(done));
    }
    out
}

fn lines_of(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn joined_text(lines: &[String]) -> String {
    lines
        .iter()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn shape_wifi(raw: &Value) -> Value {
    let lines = lines_of(raw, "lines");
    let code = raw.get("exit_code").and_then(Value::as_i64).unwrap_or(-1);
    let service = raw.get("service").cloned().unwrap_or(Value::Null);
    let interfaces = if code == 0 {
        parse_wifi_interfaces(&lines)
    } else {
        Vec::new()
    };
    let message = if code != 0 {
        Value::String(joined_text(&lines))
    } else if interfaces.is_empty() {
        Value::String("Windows did not report any Wi-Fi adapters.".into())
    } else {
        Value::Null
    };
    json!({
        "available": code == 0 && !interfaces.is_empty(),
        "service": service,
        "message": message,
        "interfaces": interfaces,
    })
}

async fn net_wifi(ctx: Ctx, _args: Args) -> CapResult {
    let raw = run_pwsh_json(&ctx.cancel, WIFI)
        .await
        .map_err(HandlerError::from)?;
    Ok(shape_wifi(&raw))
}

pub fn parse_winhttp(lines: &[String]) -> Value {
    let direct = lines
        .iter()
        .any(|l| l.to_ascii_lowercase().contains("direct access"));
    let mut server = Value::Null;
    let mut bypass = Value::Null;
    for line in lines {
        let Some(i) = line.find(':') else { continue };
        let key = line[..i].trim().to_ascii_lowercase();
        let value = line[i + 1..].trim();
        if value.is_empty() {
            continue;
        }
        if key.starts_with("proxy server") {
            server = Value::String(value.to_string());
        } else if key.starts_with("bypass list") {
            bypass = Value::String(value.to_string());
        }
    }
    json!({
        "direct": direct || server.is_null(),
        "proxy_server": server,
        "bypass": bypass,
        "text": joined_text(lines),
    })
}

pub fn shape_proxy(raw: &Value) -> Value {
    let lines = lines_of(raw, "winhttp_lines");
    let code = raw
        .get("winhttp_exit_code")
        .and_then(Value::as_i64)
        .unwrap_or(-1);
    let winhttp = if code == 0 {
        parse_winhttp(&lines)
    } else {
        json!({"error": joined_text(&lines)})
    };
    let env: Map<String, Value> = raw
        .get("environment")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter(|(_, v)| v.as_str().is_some_and(|s| !s.is_empty()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();
    json!({
        "user": raw.get("user").cloned().unwrap_or(Value::Null),
        "winhttp": winhttp,
        "environment": env,
    })
}

async fn net_proxy(ctx: Ctx, _args: Args) -> CapResult {
    let raw = run_pwsh_json(&ctx.cancel, PROXY)
        .await
        .map_err(HandlerError::from)?;
    Ok(shape_proxy(&raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(str::to_string).collect()
    }

    const CONNECTED: &str = "
There is 2 interface on the system:

    Name                   : Wi-Fi
    Description            : Intel(R) Wi-Fi 6E AX211 160MHz
    GUID                   : 3f0b1c2d-1111-2222-3333-444455556666
    Physical address       : a4:c3:f0:12:34:56
    Interface type         : Primary
    State                  : connected
    SSID                   : Home : Upstairs
    AP BSSID               : 6c:5a:b0:aa:bb:cc
    Band                   : 5 GHz
    Channel                : 44
    Network type           : Infrastructure
    Radio type             : 802.11ax
    Authentication         : WPA2-Personal
    Cipher                 : CCMP
    Connection mode        : Auto Connect
    Receive rate (Mbps)    : 1201
    Transmit rate (Mbps)   : 960.5
    Signal                 : 92%
    Rssi                   : -48
    Profile                : Home : Upstairs
    QoS MSCS Configured         : 0

    Name                   : Wi-Fi 2
    Description            : USB adapter
    Physical address       : 00:11:22:33:44:55
    State                  : disconnected
    Radio status           : Hardware On
                             Software Off

    Hosted network status  : Not available
";

    #[test]
    fn wifi_output_is_parsed_per_interface() {
        let got = parse_wifi_interfaces(&lines(CONNECTED));
        assert_eq!(got.len(), 2);
        let w = &got[0];
        assert_eq!(w["name"], "Wi-Fi");
        assert_eq!(w["ssid"], "Home : Upstairs");
        assert_eq!(w["bssid"], "6c:5a:b0:aa:bb:cc");
        assert_eq!(w["mac"], "a4:c3:f0:12:34:56");
        assert_eq!(w["band"], "5 GHz");
        assert_eq!(w["channel"], 44);
        assert_eq!(w["signal_percent"], 92);
        assert_eq!(w["rssi_dbm"], -48);
        assert_eq!(w["receive_mbps"], 1201);
        assert_eq!(w["transmit_mbps"], 960.5);
        assert_eq!(w["radio_type"], "802.11ax");
        assert_eq!(w["other"]["QoS MSCS Configured"], "0");
        assert_eq!(got[1]["state"], "disconnected");
        assert!(got[1].get("ssid").is_none());
    }

    #[test]
    fn wifi_reports_a_stopped_service_plainly() {
        let raw = json!({
            "exit_code": 1,
            "service": "Stopped",
            "lines": ["", "The Wireless AutoConfig Service (wlansvc) is not running."],
        });
        let got = shape_wifi(&raw);
        assert_eq!(got["available"], false);
        assert_eq!(got["service"], "Stopped");
        assert_eq!(
            got["message"],
            "The Wireless AutoConfig Service (wlansvc) is not running."
        );
        assert_eq!(got["interfaces"], json!([]));
        let ok =
            shape_wifi(&json!({"exit_code": 0, "service": "Running", "lines": lines(CONNECTED)}));
        assert_eq!(ok["available"], true);
        assert!(ok["message"].is_null());
    }

    #[test]
    fn winhttp_direct_and_proxied_forms_parse() {
        let direct = parse_winhttp(&lines(
            "\nCurrent WinHTTP proxy settings:\n\n    Direct access (no proxy server).\n",
        ));
        assert_eq!(direct["direct"], true);
        assert!(direct["proxy_server"].is_null());
        let proxied = parse_winhttp(&lines(
            "Current WinHTTP proxy settings:\n\n    Proxy Server(s) :  proxy.corp:8080\n    Bypass List     :  <local>;*.corp\n",
        ));
        assert_eq!(proxied["direct"], false);
        assert_eq!(proxied["proxy_server"], "proxy.corp:8080");
        assert_eq!(proxied["bypass"], "<local>;*.corp");
    }

    #[test]
    fn proxy_drops_unset_environment_values() {
        let raw = json!({
            "user": {"proxy_enabled": false, "proxy_server": "127.0.0.1:8080"},
            "winhttp_exit_code": 0,
            "winhttp_lines": ["    Direct access (no proxy server)."],
            "environment": {"HTTP_PROXY": null, "HTTPS_PROXY": "http://p:3128", "NO_PROXY": ""},
        });
        let got = shape_proxy(&raw);
        assert_eq!(got["environment"], json!({"HTTPS_PROXY": "http://p:3128"}));
        assert_eq!(got["user"]["proxy_server"], "127.0.0.1:8080");
        assert_eq!(got["winhttp"]["direct"], true);
    }

    #[test]
    fn registers_the_network_diagnostics_with_time_limits() {
        let mut r = Router::new();
        register(&mut r);
        assert_eq!(r.kinds(), vec!["net.profile", "net.proxy", "net.wifi"]);
        for spec in r.specs() {
            assert!(!spec.label.is_empty() && !spec.risky, "{}", spec.kind);
            assert!(spec.default_timeout_ms > 0, "{}", spec.kind);
        }
        for script in [WIFI, PROXY, PROFILE] {
            assert!(script.contains("ConvertTo-Json"));
        }
    }
}
