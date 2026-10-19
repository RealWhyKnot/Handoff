use crate::dispatch::{handler, Args, CapResult, Ctx, HandlerError, Router};

use super::net::{arg_bool, arg_int};
use super::pwsh_out::run_pwsh_json;

const SHARES_TEMPLATE: &str = r##"
$includeHidden = $@@HIDDEN@@
$includeSessions = $@@SESSIONS@@
$max = @@MAX@@
$errors = [ordered]@{}
$shareEntries = @()
$sessionEntries = @()
$openFileEntries = @()

try {
    $shareArgs = @{ ErrorAction = 'Stop' }
    if ($includeHidden) { $shareArgs['IncludeHidden'] = $true }
    $shareEntries = Get-SmbShare @shareArgs |
        Sort-Object Name |
        Select-Object -First $max |
        ForEach-Object {
            [ordered]@{
                name = [string]$_.Name
                path = [string]$_.Path
                description = [string]$_.Description
                share_state = [string]$_.ShareState
                share_type = [string]$_.ShareType
                current_users = if ($null -ne $_.CurrentUsers) { [int]$_.CurrentUsers } else { $null }
                special = [bool]$_.Special
                encrypt_data = [bool]$_.EncryptData
                caching_mode = [string]$_.CachingMode
                folder_enumeration_mode = [string]$_.FolderEnumerationMode
            }
        }
} catch {
    $errors['shares'] = $_.Exception.Message
}

if ($includeSessions) {
    try {
        $sessionEntries = Get-SmbSession -ErrorAction Stop |
            Sort-Object ClientComputerName, ClientUserName |
            Select-Object -First $max |
            ForEach-Object {
                [ordered]@{
                    session_id = [string]$_.SessionId
                    client_computer_name = [string]$_.ClientComputerName
                    client_user_name = [string]$_.ClientUserName
                    num_opens = if ($null -ne $_.NumOpens) { [int]$_.NumOpens } else { $null }
                    seconds_exists = if ($null -ne $_.SecondsExists) { [int64]$_.SecondsExists } else { $null }
                    seconds_idle = if ($null -ne $_.SecondsIdle) { [int64]$_.SecondsIdle } else { $null }
                    dialect = [string]$_.Dialect
                }
            }
    } catch {
        $errors['sessions'] = $_.Exception.Message
    }

    try {
        $openFileEntries = Get-SmbOpenFile -ErrorAction Stop |
            Sort-Object ClientComputerName, ShareRelativePath |
            Select-Object -First $max |
            ForEach-Object {
                [ordered]@{
                    file_id = [string]$_.FileId
                    session_id = [string]$_.SessionId
                    client_computer_name = [string]$_.ClientComputerName
                    client_user_name = [string]$_.ClientUserName
                    share_relative_path = [string]$_.ShareRelativePath
                    permissions = [string]$_.Permissions
                    locks = if ($null -ne $_.Locks) { [int]$_.Locks } else { $null }
                }
            }
    } catch {
        $errors['open_files'] = $_.Exception.Message
    }
}

[ordered]@{
    include_hidden = $includeHidden
    include_sessions = $includeSessions
    max = $max
    share_count = @($shareEntries).Count
    session_count = @($sessionEntries).Count
    open_file_count = @($openFileEntries).Count
    shares = @($shareEntries)
    sessions = @($sessionEntries)
    open_files = @($openFileEntries)
    errors = $errors
} | ConvertTo-Json -Compress -Depth 5
"##;

pub fn register(r: &mut Router) {
    super::register_specs(
        r,
        include_str!("specs/net_shares.json"),
        vec![("net.shares", handler(net_shares))],
    );
}

#[derive(Debug, Clone, PartialEq)]
pub struct NetSharesOptions {
    pub include_hidden: bool,
    pub include_sessions: bool,
    pub max_results: i64,
}

pub fn parse_net_shares_options(args: &Args) -> NetSharesOptions {
    let mut opts = NetSharesOptions {
        include_hidden: false,
        include_sessions: true,
        max_results: 200,
    };
    if let Some(v) = arg_bool(args, "include_hidden") {
        opts.include_hidden = v;
    }
    if let Some(v) = arg_bool(args, "include_sessions") {
        opts.include_sessions = v;
    }
    if let Some(v) = arg_int(args, "max_results") {
        opts.max_results = v;
    }
    if opts.max_results <= 0 {
        opts.max_results = 200;
    }
    if opts.max_results > 1000 {
        opts.max_results = 1000;
    }
    opts
}

pub fn shares_script(opts: &NetSharesOptions) -> String {
    SHARES_TEMPLATE
        .replace("@@HIDDEN@@", &opts.include_hidden.to_string())
        .replace("@@SESSIONS@@", &opts.include_sessions.to_string())
        .replace("@@MAX@@", &opts.max_results.to_string())
}

async fn net_shares(ctx: Ctx, args: Args) -> CapResult {
    let script = shares_script(&parse_net_shares_options(&args));
    run_pwsh_json(&ctx.cancel, &script)
        .await
        .map_err(HandlerError::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn args(v: Value) -> Args {
        v.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn registers_net_shares() {
        let mut r = Router::new();
        register(&mut r);
        assert_eq!(r.kinds(), vec!["net.shares"]);
    }

    #[test]
    fn options_default_and_clamp() {
        let o = parse_net_shares_options(&Args::new());
        assert_eq!(
            o,
            NetSharesOptions {
                include_hidden: false,
                include_sessions: true,
                max_results: 200
            }
        );
        assert_eq!(
            parse_net_shares_options(&args(json!({"max_results": 0}))).max_results,
            200
        );
        assert_eq!(
            parse_net_shares_options(&args(json!({"max_results": 99999}))).max_results,
            1000
        );
        assert_eq!(
            parse_net_shares_options(&args(json!({"max_results": 7}))).max_results,
            7
        );
        let o = parse_net_shares_options(&args(
            json!({"include_hidden": true, "include_sessions": false}),
        ));
        assert!(o.include_hidden && !o.include_sessions);
        let o =
            parse_net_shares_options(&args(json!({"include_hidden": "yes", "max_results": "9"})));
        assert!(!o.include_hidden && o.max_results == 200);
    }

    #[test]
    fn script_embeds_the_options() {
        let script = shares_script(&NetSharesOptions {
            include_hidden: true,
            include_sessions: false,
            max_results: 12,
        });
        assert!(script.contains("$includeHidden = $true\n"));
        assert!(script.contains("$includeSessions = $false\n"));
        assert!(script.contains("$max = 12\n"));
        assert!(!script.contains("@@"));
    }
}
