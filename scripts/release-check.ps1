param(
    [string]$ConfigPath = "config.yaml",
    [switch]$SkipHttpChecks
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$failures = New-Object System.Collections.Generic.List[string]

function Add-Failure {
    param([string]$Message)
    $script:failures.Add($Message)
    Write-Host "[FAIL] $Message" -ForegroundColor Red
}

function Add-Pass {
    param([string]$Message)
    Write-Host "[PASS] $Message" -ForegroundColor Green
}

function Is-Placeholder {
    param([string]$Value)
    if ([string]::IsNullOrWhiteSpace($Value)) { return $true }
    $lower = $Value.Trim().ToLowerInvariant()
    return $lower.Contains("your_") -or
        $lower.Contains("example") -or
        $lower.Contains("changeme") -or
        $lower.EndsWith("_here")
}

if (-not (Test-Path -Path $ConfigPath)) {
    Add-Failure "Config file not found: $ConfigPath"
    exit 1
}
Add-Pass "Config file exists: $ConfigPath"

if (-not (Get-Command ConvertFrom-Yaml -ErrorAction SilentlyContinue)) {
    Add-Failure "ConvertFrom-Yaml is unavailable. Use PowerShell 7+."
    exit 1
}

$raw = Get-Content -Path $ConfigPath -Raw
$cfg = $raw | ConvertFrom-Yaml

if ($null -eq $cfg.homeserver -or $null -eq $cfg.appservice -or $null -eq $cfg.bridge) {
    Add-Failure "Missing top-level keys: homeserver/appservice/bridge"
} else {
    Add-Pass "Top-level config sections are present"
}

if ($cfg.appservice.database.type -ne "sqlite") {
    Add-Failure "appservice.database.type must be 'sqlite' (current: '$($cfg.appservice.database.type)')"
} else {
    Add-Pass "Database type is sqlite"
}

$requiredStrings = @(
    @{ Name = "appservice.as_token"; Value = $cfg.appservice.as_token },
    @{ Name = "appservice.hs_token"; Value = $cfg.appservice.hs_token },
    @{ Name = "bridge.app_id"; Value = $cfg.bridge.app_id },
    @{ Name = "bridge.app_secret"; Value = $cfg.bridge.app_secret },
    @{ Name = "bridge.listen_secret"; Value = $cfg.bridge.listen_secret }
)

foreach ($item in $requiredStrings) {
    if (Is-Placeholder -Value ([string]$item.Value)) {
        Add-Failure "$($item.Name) is empty or placeholder"
    } else {
        Add-Pass "$($item.Name) configured"
    }
}

if ($cfg.bridge.encrypt_key -and -not $cfg.bridge.verification_token) {
    Add-Failure "bridge.verification_token is required when bridge.encrypt_key is configured"
} else {
    Add-Pass "Webhook encryption/token combination is valid"
}

$dbUri = [string]$cfg.appservice.database.uri
if ($dbUri.StartsWith("sqlite:")) {
    $dbPath = $dbUri.Substring("sqlite:".Length).TrimStart("/")
    if ([string]::IsNullOrWhiteSpace($dbPath)) {
        Add-Failure "Invalid sqlite URI path in appservice.database.uri"
    } else {
        $dbDir = Split-Path -Parent $dbPath
        if (-not [string]::IsNullOrWhiteSpace($dbDir) -and -not (Test-Path -Path $dbDir)) {
            Add-Failure "SQLite directory does not exist: $dbDir"
        } else {
            Add-Pass "SQLite path looks valid: $dbPath"
        }
    }
} else {
    Add-Failure "Unsupported sqlite URI format: $dbUri"
}

if (-not $SkipHttpChecks) {
    $appBase = "http://$($cfg.appservice.hostname):$($cfg.appservice.port)"
    $feishuListen = [string]$cfg.bridge.listen_address
    $provisionToken = [string]$env:MATRIX_BRIDGE_FEISHU_PROVISIONING_READ_TOKEN
    if ([string]::IsNullOrWhiteSpace($provisionToken)) {
        $provisionToken = [string]$env:MATRIX_BRIDGE_FEISHU_PROVISIONING_TOKEN
    }
    if ([string]::IsNullOrWhiteSpace($provisionToken)) {
        $provisionToken = [string]$env:MATRIX_BRIDGE_FEISHU_PROVISIONING_ADMIN_TOKEN
    }
    if ([string]::IsNullOrWhiteSpace($provisionToken)) {
        $provisionToken = [string]$cfg.appservice.as_token
    }
    $encodedToken = [uri]::EscapeDataString($provisionToken)
    if (-not $feishuListen.StartsWith("http://") -and -not $feishuListen.StartsWith("https://")) {
        $feishuListen = "http://$feishuListen"
    }

    try {
        $resp = Invoke-WebRequest -UseBasicParsing -Uri "$appBase/health" -TimeoutSec 3
        if ($resp.StatusCode -eq 200) {
            Add-Pass "Appservice health endpoint reachable: $appBase/health"
        } else {
            Add-Failure "Appservice health endpoint returned $($resp.StatusCode)"
        }
    } catch {
        Add-Failure "Appservice health endpoint unreachable: $appBase/health"
    }

    try {
        $resp = Invoke-WebRequest -UseBasicParsing -Uri "$appBase/admin/status?access_token=$encodedToken" -TimeoutSec 3
        if ($resp.StatusCode -eq 200) {
            Add-Pass "Provisioning status endpoint reachable: $appBase/admin/status"
        } else {
            Add-Failure "Provisioning status endpoint returned $($resp.StatusCode)"
        }
    } catch {
        Add-Failure "Provisioning status endpoint unreachable: $appBase/admin/status"
    }

    try {
        $resp = Invoke-WebRequest -UseBasicParsing -Uri "$appBase/admin/mappings?limit=1&offset=0&access_token=$encodedToken" -TimeoutSec 3
        if ($resp.StatusCode -eq 200) {
            Add-Pass "Provisioning mappings endpoint reachable: $appBase/admin/mappings"
        } else {
            Add-Failure "Provisioning mappings endpoint returned $($resp.StatusCode)"
        }
    } catch {
        Add-Failure "Provisioning mappings endpoint unreachable: $appBase/admin/mappings"
    }

    try {
        $resp = Invoke-WebRequest -UseBasicParsing -Uri "$feishuListen/health" -TimeoutSec 3
        if ($resp.StatusCode -eq 200) {
            Add-Pass "Feishu webhook health endpoint reachable: $feishuListen/health"
        } else {
            Add-Failure "Feishu webhook health endpoint returned $($resp.StatusCode)"
        }
    } catch {
        Add-Failure "Feishu webhook health endpoint unreachable: $feishuListen/health"
    }
} else {
    Add-Pass "Skipped HTTP checks by request"
}

if ($failures.Count -gt 0) {
    Write-Host ""
    Write-Host "Release checks failed with $($failures.Count) issue(s)." -ForegroundColor Red
    exit 1
}

Write-Host ""
Write-Host "All release checks passed." -ForegroundColor Green
exit 0
