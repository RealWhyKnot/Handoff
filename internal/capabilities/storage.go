// SPDX-License-Identifier: GPL-3.0-or-later
package capabilities

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"

	"github.com/RealWhyKnot/Handoff/internal/dispatch"
)

// RegisterStorage wires read-only local storage inventory handlers.
func RegisterStorage(r *dispatch.Router) {
	r.Register("storage.volumes", storageVolumes)
}

func storageDriveLetterArg(args map[string]json.RawMessage) (string, error) {
	var driveLetter string
	if v, ok := args["drive_letter"]; ok {
		_ = json.Unmarshal(v, &driveLetter)
	}
	driveLetter = strings.TrimSuffix(strings.TrimSpace(driveLetter), ":")
	if driveLetter == "" {
		return "", nil
	}
	if len(driveLetter) != 1 {
		return "", fmt.Errorf("storage.volumes: drive_letter must be a single letter")
	}
	ch := driveLetter[0]
	if (ch < 'A' || ch > 'Z') && (ch < 'a' || ch > 'z') {
		return "", fmt.Errorf("storage.volumes: drive_letter must be A-Z")
	}
	return strings.ToUpper(driveLetter), nil
}

func storageVolumes(ctx context.Context, args map[string]json.RawMessage) (interface{}, error) {
	driveLetter, err := storageDriveLetterArg(args)
	if err != nil {
		return nil, err
	}
	script := fmt.Sprintf(`
$driveLetter = %s
if ($driveLetter) {
    $volumes = @(Get-Volume -DriveLetter $driveLetter -ErrorAction SilentlyContinue)
} else {
    $volumes = @(Get-Volume -ErrorAction SilentlyContinue)
}

$entries = $volumes |
    Sort-Object DriveLetter, FileSystemLabel |
    ForEach-Object {
        $sizeGB = $null
        $freeGB = $null
        $freePercent = $null
        if ($null -ne $_.Size) {
            $sizeGB = [math]::Round([double]$_.Size / 1GB, 2)
        }
        if ($null -ne $_.SizeRemaining) {
            $freeGB = [math]::Round([double]$_.SizeRemaining / 1GB, 2)
        }
        if ($null -ne $_.Size -and $_.Size -gt 0 -and $null -ne $_.SizeRemaining) {
            $freePercent = [math]::Round(([double]$_.SizeRemaining / [double]$_.Size) * 100, 1)
        }
        [ordered]@{
            drive_letter = if ($null -ne $_.DriveLetter) { [string]$_.DriveLetter } else { $null }
            label = [string]$_.FileSystemLabel
            file_system = [string]$_.FileSystem
            size_gb = $sizeGB
            free_gb = $freeGB
            free_percent = $freePercent
            health_status = [string]$_.HealthStatus
            operational_status = [string]$_.OperationalStatus
            path = if ($_.Path) { [string]$_.Path } else { $null }
            unique_id = if ($_.UniqueId) { [string]$_.UniqueId } else { $null }
        }
    }

[ordered]@{
    drive_letter = $driveLetter
    count = @($entries).Count
    entries = @($entries)
} | ConvertTo-Json -Compress -Depth 4
`, psSingleQuote(driveLetter))
	return runPwshJSON(ctx, script)
}
