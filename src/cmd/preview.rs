use std::path::PathBuf;

use serde_json::{json, Map, Value};

use super::host::{self, Ending};
use super::summary::{describe_args, summarize_result};
use crate::console;
use crate::dispatch::Outcome;

pub struct Sample {
    pub label: Option<&'static str>,
    pub kind: &'static str,
    pub args: Map<String, Value>,
    pub out: Outcome,
    pub ending: Ending,
}

const WIFI: &str = "$ErrorActionPreference = 'SilentlyContinue'\r
$report = [ordered]@{}\r
$adapters = Get-NetAdapter -Physical | Where-Object { $_.MediaType -match '802.11' -or $_.Name -match 'Wi-?Fi' }\r
foreach ($a in $adapters) {\r
\t$ip = Get-NetIPConfiguration -InterfaceIndex $a.ifIndex\r
\t$report[$a.Name] = [ordered]@{\r
\t\tStatus      = $a.Status\r
\t\tLinkSpeed   = $a.LinkSpeed\r
\t\tMac         = $a.MacAddress\r
\t\tIPv4        = ($ip.IPv4Address | Select-Object -ExpandProperty IPAddress) -join ', '\r
\t\tGateway     = ($ip.IPv4DefaultGateway | Select-Object -ExpandProperty NextHop) -join ', '\r
\t\tDns         = ($ip.DNSServer | Where-Object AddressFamily -eq 2 | Select-Object -ExpandProperty ServerAddresses) -join ', '\r
\t}\r
}\r
\r
$wlan = netsh wlan show interfaces | Out-String\r
$ssid = if ($wlan -match '(?m)^\\s*SSID\\s*:\\s*(.+)$') { $Matches[1].Trim() } else { '' }\r
$signal = if ($wlan -match '(?m)^\\s*Signal\\s*:\\s*(\\d+)%') { [int]$Matches[1] } else { -1 }\r
$channel = if ($wlan -match '(?m)^\\s*Channel\\s*:\\s*(\\d+)') { [int]$Matches[1] } else { -1 }\r
\r
$gw = ($adapters | ForEach-Object { (Get-NetIPConfiguration -InterfaceIndex $_.ifIndex).IPv4DefaultGateway.NextHop } | Select-Object -First 1)\r
$gwPing = if ($gw) { Test-Connection -TargetName $gw -Count 4 -ErrorAction SilentlyContinue } else { @() }\r
$netPing = Test-Connection -TargetName 1.1.1.1 -Count 4 -ErrorAction SilentlyContinue\r
$dnsTest = Resolve-DnsName -Name 'www.microsoft.com' -Type A -DnsOnly -ErrorAction SilentlyContinue\r
\r
[pscustomobject]@{\r
    Adapters   = $report\r
    Ssid       = $ssid\r
    Signal     = $signal\r
    Channel    = $channel\r
    GatewayMs  = ($gwPing | Measure-Object -Property Latency -Average).Average\r
    InternetMs = ($netPing | Measure-Object -Property Latency -Average).Average\r
    DnsOk      = [bool]$dnsTest\r
} | ConvertTo-Json -Depth 5\r
";

fn pipeline() -> String {
    "Get-WinEvent -FilterHashtable @{LogName='System'; Level=1,2,3; StartTime=(Get-Date).AddDays(-7)} -MaxEvents 500 | Where-Object { $_.ProviderName -notmatch 'DistributedCOM|Service Control Manager' } | Group-Object ProviderName, Id | Sort-Object Count -Descending | Select-Object -First 15 Count, @{n='Provider';e={$_.Group[0].ProviderName}}, @{n='Id';e={$_.Group[0].Id}}, @{n='Latest';e={($_.Group | Sort-Object TimeCreated -Descending | Select-Object -First 1).TimeCreated}}, @{n='Message';e={($_.Group[0].Message -split \"`n\")[0]}} | Format-Table -AutoSize".to_string()
}

fn args(v: Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap()
}

fn done(ms: i64, result: Value) -> Outcome {
    Outcome {
        ok: true,
        result,
        elapsed_ms: ms,
        ..Outcome::default()
    }
}

fn failed(ms: i64, error: &str) -> Outcome {
    Outcome {
        ok: false,
        error: error.to_string(),
        elapsed_ms: ms,
        ..Outcome::default()
    }
}

pub fn samples() -> Vec<Sample> {
    let huge: String = (1..=2000)
        .map(|i| format!("Write-Output \"line {i}\"\n"))
        .collect();
    let long_line = format!(
        "$x = '{}'; Write-Output $x.Length",
        "abcdefghij".repeat(1000)
    );
    let cjk = "Write-Output '网络诊断：正在检查无线适配器和默认网关的连接状态，请稍候。这是一个很长的中文句子用于测试换行。'\nWrite-Output '🚀🔥✅ 接続テスト完了 ✅🔥🚀 状態は良好です'\n$名前 = 'テスト'";
    let ansi = "Write-Output \"\x1b[31mred text\x1b[0m\"\n\x1b]0;evil title\x07Write-Output 'bell\x07 and backspace\x08\x08 here'\n\u{202E}Write-Output 'reversed'\u{202C}\nWrite-Output 'zero\u{200B}width'";
    let long_path = "C:\\Users\\ADMIN\\AppData\\Local\\Packages\\Microsoft.WindowsTerminal_8wekyb3d8bbwe\\LocalState\\DiagOutputDir\\Logs\\2026-10-19\\session-0f3a9c2e-1d4b-4a7e-9f21-8c3b5d6e7f80\\terminal-host-diagnostics.log";
    vec![
        Sample {
            label: Some("Run script"),
            kind: "ps.exec",
            args: args(json!({"script": WIFI})),
            out: done(
                3420,
                json!({"exit_code": 0, "stdout": "x".repeat(1843)}),
            ),
            ending: Ending::Done,
        },
        Sample {
            label: Some("Run script"),
            kind: "ps.exec",
            args: args(json!({"script": pipeline()})),
            out: failed(
                1210,
                "Get-WinEvent : There is not an event log on the localhost computer that matches \"System\".\r\nAt line:1 char:1\r\n+ Get-WinEvent -FilterHashtable @{LogName='System'; Level=1,2,3; StartTime=(Get-Date).AddDays(-7)} -MaxEvents 500 | Where-Object ...\r\n+ ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~\r\n    + CategoryInfo          : ObjectNotFound: (System:String) [Get-WinEvent], Exception\r\n    + FullyQualifiedErrorId : NoMatchingLogsFound,Microsoft.PowerShell.Commands.GetWinEventCommand",
            ),
            ending: Ending::Failed,
        },
        Sample {
            label: Some("Run script"),
            kind: "ps.exec",
            args: args(json!({"script": "Get-Date"})),
            out: done(380, json!({"exit_code": 0, "stdout": "Monday, 19 October 2026 14:02:11\r\n"})),
            ending: Ending::Done,
        },
        Sample {
            label: Some("Read file"),
            kind: "fs.read",
            args: args(json!({"path": long_path, "start_line": 120, "end_line": 180})),
            out: done(45, json!({"size": 48213})),
            ending: Ending::Done,
        },
        Sample {
            label: Some("Ping"),
            kind: "net.ping",
            args: args(json!({"target": "example.com", "count": 4})),
            out: done(3100, json!({"Name": "example.com"})),
            ending: Ending::Done,
        },
        Sample {
            label: Some("Fetch URL"),
            kind: "net.curl",
            args: args(json!({"url": format!("https://example.com/api/v2/search?q={}&page=1", "network%20diagnostics%20".repeat(12)), "method": "GET"})),
            out: done(640, json!({"size": 1520})),
            ending: Ending::Done,
        },
        Sample {
            label: Some("Run script"),
            kind: "ps.exec",
            args: args(json!({"script": huge})),
            out: done(5200, json!({"exit_code": 0, "stdout": "y".repeat(26000)})),
            ending: Ending::Done,
        },
        Sample {
            label: Some("Run script"),
            kind: "ps.exec",
            args: args(json!({"script": long_line})),
            out: done(410, json!({"exit_code": 0, "stdout": "10000\r\n"})),
            ending: Ending::Done,
        },
        Sample {
            label: Some("Run script"),
            kind: "ps.exec",
            args: args(json!({"script": cjk})),
            out: done(520, json!({"exit_code": 0, "stdout": "z".repeat(120)})),
            ending: Ending::Done,
        },
        Sample {
            label: Some("Run script"),
            kind: "ps.exec",
            args: args(json!({"script": ansi})),
            out: done(330, json!({"exit_code": 0, "stdout": "red text"})),
            ending: Ending::Done,
        },
    ]
}

fn result_detail(s: &Sample) -> String {
    match s.ending {
        Ending::Refused => String::new(),
        _ => summarize_result(&s.out),
    }
}

fn stamped(mut rows: Vec<String>) -> Vec<String> {
    let clock = console::paint(console::Tone::Dim, "14:02:11");
    let first = rows.remove(0);
    let mut out = vec![format!("{clock}  {first}")];
    out.extend(
        rows.into_iter()
            .map(|r| format!("{}{r}", " ".repeat(console::STAMP_WIDTH))),
    );
    out
}

fn after(num: usize, s: &Sample, columns: usize) -> Vec<String> {
    let width = console::body_width_for(columns);
    let mut out = stamped(host::command_lines(
        num,
        s.label,
        s.kind,
        &describe_args(&s.args),
        width,
    ));
    out.extend(stamped(host::result_lines(
        num,
        &s.out,
        s.ending,
        false,
        &result_detail(s),
        width,
    )));
    out
}

fn out_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("ux-preview")
        .join("console");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn render(colour: bool, columns: usize) -> String {
    console::set_color(colour);
    let mut text = String::new();
    for (i, s) in samples().iter().enumerate() {
        for line in after(i + 1, s, columns) {
            text.push_str(&line);
            text.push('\n');
        }
    }
    console::set_color(false);
    text
}

#[test]
#[ignore]
fn write_after_preview() {
    let dir = out_dir();
    for columns in [80, 120] {
        std::fs::write(
            dir.join(format!("after-{columns}.ansi")),
            render(true, columns),
        )
        .unwrap();
        std::fs::write(
            dir.join(format!("after-{columns}.txt")),
            render(false, columns),
        )
        .unwrap();
    }
}
