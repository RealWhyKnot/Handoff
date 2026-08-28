// SPDX-License-Identifier: GPL-3.0-or-later
package capabilities

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"

	"github.com/RealWhyKnot/Handoff/internal/dispatch"
)

// RegisterApp wires app.* installed-software inventory.
//
// app.list walks the Win32 uninstall registry hives plus AppX/UWP store
// packages, returning a flat list. Optional name_prefix narrows the
// listing; max_results caps the response so large fleets don't ship
// thousands of rows.
func RegisterApp(r *dispatch.Router) {
	r.RegisterSpec(dispatch.Spec{
		Kind:        "app.list",
		Label:       "Installed apps",
		Description: "List installed applications.",
		Params: []dispatch.Param{
			{Name: "name_prefix", Type: dispatch.ParamString, Description: "Match applications whose name starts with this."},
			limitParam(300, 1, 5000),
		},
	}, appList)
}

func appList(ctx context.Context, args map[string]json.RawMessage) (interface{}, error) {
	var prefix string
	maxResults := 300
	if v, ok := args["name_prefix"]; ok {
		_ = json.Unmarshal(v, &prefix)
	}
	if v, ok := args["limit"]; ok {
		_ = json.Unmarshal(v, &maxResults)
	}
	prefix = strings.TrimSpace(prefix)
	if len(prefix) > 128 {
		return nil, fmt.Errorf("app.list: name_prefix too long")
	}
	if maxResults <= 0 {
		maxResults = 300
	}
	if maxResults > 5000 {
		maxResults = 5000
	}

	script := fmt.Sprintf(`
$prefix = %s
$max = %d

$paths = @(
    'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\*',
    'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*',
    'HKCU:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\*'
)
$win32 = foreach ($p in $paths) {
    Get-ItemProperty $p -ErrorAction SilentlyContinue |
        Where-Object { $_.DisplayName } |
        ForEach-Object {
            [ordered]@{
                kind        = 'win32'
                name        = [string]$_.DisplayName
                version     = [string]$_.DisplayVersion
                publisher   = [string]$_.Publisher
                install_date = [string]$_.InstallDate
                install_loc  = [string]$_.InstallLocation
                uninstall    = [string]$_.UninstallString
            }
        }
}

$appx = @()
try {
    $appx = Get-AppxPackage -AllUsers:$false -ErrorAction SilentlyContinue |
        ForEach-Object {
            [ordered]@{
                kind        = 'appx'
                name        = [string]$_.Name
                version     = [string]$_.Version
                publisher   = [string]$_.Publisher
                install_loc = [string]$_.InstallLocation
                family_name = [string]$_.PackageFamilyName
            }
        }
} catch {}

$all = @($win32 + $appx)
if ($prefix) {
    $needle = $prefix.ToLowerInvariant()
    $all = $all | Where-Object { $_.name -and $_.name.ToLowerInvariant().StartsWith($needle) }
}
$all = $all | Sort-Object @{Expression='kind'},@{Expression='name'} | Select-Object -First $max

[ordered]@{
    prefix  = $prefix
    count   = @($all).Count
    max     = $max
    entries = @($all)
} | ConvertTo-Json -Compress -Depth 4
`, psSingleQuote(prefix), maxResults)
	return runPwshJSON(ctx, script)
}
