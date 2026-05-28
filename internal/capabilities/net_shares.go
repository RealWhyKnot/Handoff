// SPDX-License-Identifier: GPL-3.0-or-later
package capabilities

import (
	"context"
	"encoding/json"
	"fmt"
)

type netSharesOptions struct {
	includeHidden   bool
	includeSessions bool
	maxResults      int
}

func parseNetSharesOptions(args map[string]json.RawMessage) netSharesOptions {
	opts := netSharesOptions{
		includeSessions: true,
		maxResults:      200,
	}
	if v, ok := args["include_hidden"]; ok {
		_ = json.Unmarshal(v, &opts.includeHidden)
	}
	if v, ok := args["include_sessions"]; ok {
		_ = json.Unmarshal(v, &opts.includeSessions)
	}
	if v, ok := args["max_results"]; ok {
		_ = json.Unmarshal(v, &opts.maxResults)
	}
	if opts.maxResults <= 0 {
		opts.maxResults = 200
	}
	if opts.maxResults > 1000 {
		opts.maxResults = 1000
	}
	return opts
}

func netShares(ctx context.Context, args map[string]json.RawMessage) (interface{}, error) {
	opts := parseNetSharesOptions(args)
	script := fmt.Sprintf(`
$includeHidden = $%t
$includeSessions = $%t
$max = %d
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
`, opts.includeHidden, opts.includeSessions, opts.maxResults)
	return runPwshJSON(ctx, script)
}
