// SPDX-License-Identifier: GPL-3.0-or-later
package capabilities

import (
	"context"
	"encoding/json"
	"fmt"
	"regexp"
	"strings"

	"github.com/RealWhyKnot/Handoff/internal/dispatch"
)

// RegisterReg wires read-only registry capabilities into the router.
//
// reg.query reads a single registry key: its values and immediate subkeys.
// With recursive=true it walks the subtree depth-first up to max_results.
// All access is through the safe PowerShell Registry provider so symlinks
// and reparse points cannot escape the chosen hive.
func RegisterReg(r *dispatch.Router) {
	r.Register("reg.query", regQuery)
}

var regHives = map[string]string{
	"HKLM": "HKLM",
	"HKCU": "HKCU",
	"HKCR": "HKCR",
	"HKU":  "HKU",
	"HKCC": "HKCC",
}

var regKeyPathPattern = regexp.MustCompile(`^[A-Za-z0-9 _\-\.\\\/()\[\]@:]*$`)

func regQuery(ctx context.Context, args map[string]json.RawMessage) (interface{}, error) {
	var (
		hive       string
		key        string
		value      string
		recursive  bool
		maxResults int
	)
	if v, ok := args["hive"]; ok {
		_ = json.Unmarshal(v, &hive)
	}
	if v, ok := args["key"]; ok {
		_ = json.Unmarshal(v, &key)
	}
	if v, ok := args["value"]; ok {
		_ = json.Unmarshal(v, &value)
	}
	if v, ok := args["recursive"]; ok {
		_ = json.Unmarshal(v, &recursive)
	}
	if v, ok := args["max_results"]; ok {
		_ = json.Unmarshal(v, &maxResults)
	}

	hive = strings.ToUpper(strings.TrimSpace(hive))
	if hive == "" {
		hive = "HKLM"
	}
	if _, ok := regHives[hive]; !ok {
		return nil, fmt.Errorf("reg.query: hive must be one of HKLM, HKCU, HKCR, HKU, HKCC")
	}

	key = strings.TrimSpace(key)
	key = strings.TrimLeft(key, "\\/")
	if key == "" {
		return nil, fmt.Errorf("reg.query: 'key' is required")
	}
	if len(key) > 512 {
		return nil, fmt.Errorf("reg.query: 'key' is too long")
	}
	if !regKeyPathPattern.MatchString(key) {
		return nil, fmt.Errorf("reg.query: 'key' contains unsupported characters")
	}
	if strings.Contains(key, "..") {
		return nil, fmt.Errorf("reg.query: 'key' may not contain '..'")
	}

	value = strings.TrimSpace(value)
	if len(value) > 256 {
		return nil, fmt.Errorf("reg.query: 'value' is too long")
	}

	if maxResults <= 0 {
		maxResults = 200
	}
	if maxResults > 2000 {
		maxResults = 2000
	}

	script := fmt.Sprintf(`
$hive = %s
$key  = %s
$value = %s
$recursive = $%t
$max = %d

$root = "$hive`+"`"+`:\$key"
if (-not (Test-Path -LiteralPath $root)) {
    [ordered]@{
        error = "key not found"
        path  = $root
    } | ConvertTo-Json -Compress
    return
}

function Get-RegEntry($psPath) {
    $item = Get-Item -LiteralPath $psPath -ErrorAction Stop
    $entry = [ordered]@{
        path     = $psPath
        subkeys  = @($item.GetSubKeyNames())
        values   = @()
    }
    foreach ($n in $item.GetValueNames()) {
        $kind = $item.GetValueKind($n).ToString()
        $data = $item.GetValue($n)
        if ($data -is [byte[]]) {
            $data = [Convert]::ToBase64String($data)
            $kind = "$kind+base64"
        } elseif ($data -is [string[]]) {
            $data = @($data)
        }
        $entry.values += [ordered]@{
            name = $n
            kind = $kind
            data = $data
        }
    }
    return $entry
}

if ($value) {
    $item = Get-Item -LiteralPath $root -ErrorAction SilentlyContinue
    if (-not $item -or -not ($item.GetValueNames() -contains $value)) {
        [ordered]@{
            error = "value not found"
            path  = $root
            name  = $value
        } | ConvertTo-Json -Compress
        return
    }
    $kind = $item.GetValueKind($value).ToString()
    $data = $item.GetValue($value)
    if ($data -is [byte[]]) {
        $data = [Convert]::ToBase64String($data)
        $kind = "$kind+base64"
    } elseif ($data -is [string[]]) {
        $data = @($data)
    }
    [ordered]@{
        path = $root
        name = $value
        kind = $kind
        data = $data
    } | ConvertTo-Json -Compress -Depth 6
    return
}

if (-not $recursive) {
    Get-RegEntry $root | ConvertTo-Json -Compress -Depth 6
    return
}

$visited = New-Object System.Collections.Generic.List[object]
$stack = New-Object System.Collections.Stack
$stack.Push($root)
while ($stack.Count -gt 0 -and $visited.Count -lt $max) {
    $current = $stack.Pop()
    try {
        $entry = Get-RegEntry $current
        $visited.Add($entry) | Out-Null
        foreach ($sub in $entry.subkeys) {
            if ($visited.Count + $stack.Count -ge $max) { break }
            $stack.Push(($current.TrimEnd('\') + '\' + $sub))
        }
    } catch {
        # ignore inaccessible subkeys
    }
}
[ordered]@{
    path     = $root
    count    = $visited.Count
    max      = $max
    entries  = @($visited)
} | ConvertTo-Json -Compress -Depth 6
`, psSingleQuote(hive), psSingleQuote(key), psSingleQuote(value), recursive, maxResults)
	return runPwshJSON(ctx, script)
}
