param(
    [string]$ConfigPath = "config.yaml",
    [switch]$SkipHttpChecks,
    [switch]$SkipDbChecks,
    [switch]$SkipFeishuApiChecks,
    [switch]$SkipEventChecks,
    [string]$SubscribedEvents = "",
    [string]$FeishuApiBase = "https://open.feishu.cn/open-apis",
    [string]$ScopeProbeChatId = "",
    [string]$ScopeProbeUserId = ""
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$failures = New-Object System.Collections.Generic.List[string]
$warnings = New-Object System.Collections.Generic.List[string]

function Add-Failure {
    param([string]$Message)
    $script:failures.Add($Message)
    Write-Host "[FAIL] $Message" -ForegroundColor Red
}

function Add-Pass {
    param([string]$Message)
    Write-Host "[PASS] $Message" -ForegroundColor Green
}

function Add-Warn {
    param([string]$Message)
    $script:warnings.Add($Message)
    Write-Host "[WARN] $Message" -ForegroundColor Yellow
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

function Normalize-Url {
    param([string]$Value)
    if ([string]::IsNullOrWhiteSpace($Value)) { return $null }
    if ($Value.StartsWith("http://") -or $Value.StartsWith("https://")) {
        return $Value.TrimEnd("/")
    }
    return ("http://{0}" -f $Value.Trim()).TrimEnd("/")
}

function Get-FirstNonEmpty {
    param([string[]]$Candidates)
    foreach ($value in $Candidates) {
        if (-not [string]::IsNullOrWhiteSpace($value)) {
            return $value
        }
    }
    return ""
}

function Split-EventList {
    param([string]$Raw)
    if ([string]::IsNullOrWhiteSpace($Raw)) { return @() }
    return @(
        $Raw -split "[,\s;]+" |
            ForEach-Object { $_.Trim() } |
            Where-Object { -not [string]::IsNullOrWhiteSpace($_) } |
            ForEach-Object { $_.ToLowerInvariant() } |
            Select-Object -Unique
    )
}

function Invoke-Http {
    param(
        [string]$Method,
        [string]$Url,
        [int]$TimeoutSec,
        [hashtable]$Headers,
        [object]$Body
    )

    $params = @{
        Method = $Method
        Uri = $Url
        TimeoutSec = $TimeoutSec
        UseBasicParsing = $true
    }

    if ($null -ne $Headers -and $Headers.Count -gt 0) {
        $params.Headers = $Headers
    }

    if ($null -ne $Body) {
        $params.ContentType = "application/json"
        $params.Body = ($Body | ConvertTo-Json -Depth 12 -Compress)
    }

    try {
        $resp = Invoke-WebRequest @params
        return [pscustomobject]@{
            Ok = $true
            StatusCode = [int]$resp.StatusCode
            Body = [string]$resp.Content
            Error = ""
        }
    } catch {
        $status = 0
        if ($_.Exception.Response -and $_.Exception.Response.StatusCode) {
            $status = [int]$_.Exception.Response.StatusCode
        }
        return [pscustomobject]@{
            Ok = $false
            StatusCode = $status
            Body = ""
            Error = [string]$_.Exception.Message
        }
    }
}

function Invoke-AuthorizedJson {
    param(
        [string]$Method,
        [string]$Url,
        [string]$Token,
        [object]$Body = $null
    )
    $headers = @{}
    if (-not [string]::IsNullOrWhiteSpace($Token)) {
        $headers["Authorization"] = "Bearer $Token"
    }
    return Invoke-Http -Method $Method -Url $Url -TimeoutSec 8 -Headers $headers -Body $Body
}

function Parse-SqlitePath {
    param(
        [string]$DbUri,
        [string]$BaseDir
    )

    if ([string]::IsNullOrWhiteSpace($DbUri)) {
        return $null
    }

    $path = $DbUri
    if ($path.StartsWith("sqlite://")) {
        $path = $path.Substring("sqlite://".Length)
    } elseif ($path.StartsWith("sqlite:")) {
        $path = $path.Substring("sqlite:".Length)
    }
    $path = $path.TrimStart("/")
    if ([string]::IsNullOrWhiteSpace($path)) {
        return $null
    }

    if ([System.IO.Path]::IsPathRooted($path)) {
        return $path
    }
    return [System.IO.Path]::GetFullPath([System.IO.Path]::Combine($BaseDir, $path))
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

$resolvedConfigPath = (Resolve-Path $ConfigPath).Path
$configDir = Split-Path -Parent $resolvedConfigPath
$cfg = (Get-Content -Path $resolvedConfigPath -Raw) | ConvertFrom-Yaml

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
    @{ Name = "appservice.as_token"; Value = [string]$cfg.appservice.as_token },
    @{ Name = "appservice.hs_token"; Value = [string]$cfg.appservice.hs_token },
    @{ Name = "bridge.app_id"; Value = [string]$cfg.bridge.app_id },
    @{ Name = "bridge.app_secret"; Value = [string]$cfg.bridge.app_secret },
    @{ Name = "bridge.listen_secret"; Value = [string]$cfg.bridge.listen_secret }
)

foreach ($item in $requiredStrings) {
    if (Is-Placeholder -Value $item.Value) {
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

$hsAddress = Normalize-Url -Value ([string]$cfg.homeserver.address)
$appserviceAddress = Normalize-Url -Value ([string]$cfg.appservice.address)
$listenAddress = Normalize-Url -Value ([string]$cfg.bridge.listen_address)

try {
    [void][Uri]$hsAddress
    Add-Pass "homeserver.address format is valid"
} catch {
    Add-Failure "homeserver.address is not a valid URL: $($cfg.homeserver.address)"
}

try {
    $appUri = [Uri]$appserviceAddress
    Add-Pass "appservice.address format is valid"
    if ($cfg.appservice.hostname -and $cfg.appservice.port) {
        if ($appUri.Host -ne [string]$cfg.appservice.hostname -or $appUri.Port -ne [int]$cfg.appservice.port) {
            Add-Failure "appservice.address does not match appservice.hostname/appservice.port"
        } else {
            Add-Pass "appservice.address is consistent with hostname/port"
        }
    }
} catch {
    Add-Failure "appservice.address is not a valid URL: $($cfg.appservice.address)"
}

try {
    [void][Uri]$listenAddress
    Add-Pass "bridge.listen_address format is valid"
} catch {
    Add-Failure "bridge.listen_address is not a valid URL: $($cfg.bridge.listen_address)"
}

$dbPath = Parse-SqlitePath -DbUri ([string]$cfg.appservice.database.uri) -BaseDir $configDir
if ([string]::IsNullOrWhiteSpace($dbPath)) {
    Add-Failure "Invalid sqlite URI path in appservice.database.uri"
} else {
    $dbDir = Split-Path -Parent $dbPath
    if (-not [string]::IsNullOrWhiteSpace($dbDir) -and -not (Test-Path -Path $dbDir)) {
        Add-Failure "SQLite directory does not exist: $dbDir"
    } else {
        Add-Pass "SQLite path resolved: $dbPath"
    }
}

$readToken = Get-FirstNonEmpty @(
    [string]$env:MATRIX_BRIDGE_FEISHU_PROVISIONING_READ_TOKEN,
    [string]$env:MATRIX_BRIDGE_FEISHU_PROVISIONING_TOKEN,
    [string]$env:MATRIX_BRIDGE_FEISHU_PROVISIONING_ADMIN_TOKEN,
    [string]$cfg.appservice.as_token
)
$writeToken = Get-FirstNonEmpty @(
    [string]$env:MATRIX_BRIDGE_FEISHU_PROVISIONING_WRITE_TOKEN,
    [string]$env:MATRIX_BRIDGE_FEISHU_PROVISIONING_TOKEN,
    [string]$env:MATRIX_BRIDGE_FEISHU_PROVISIONING_ADMIN_TOKEN,
    [string]$cfg.appservice.as_token
)
$deleteToken = Get-FirstNonEmpty @(
    [string]$env:MATRIX_BRIDGE_FEISHU_PROVISIONING_DELETE_TOKEN,
    [string]$env:MATRIX_BRIDGE_FEISHU_PROVISIONING_ADMIN_TOKEN,
    [string]$env:MATRIX_BRIDGE_FEISHU_PROVISIONING_TOKEN,
    [string]$cfg.appservice.as_token
)

if (Is-Placeholder -Value $readToken) {
    Add-Failure "Read provisioning token is empty/placeholder"
} else {
    Add-Pass "Read provisioning token resolved"
}
if (Is-Placeholder -Value $writeToken) {
    Add-Failure "Write provisioning token is empty/placeholder"
} else {
    Add-Pass "Write provisioning token resolved"
}
if (Is-Placeholder -Value $deleteToken) {
    Add-Failure "Delete provisioning token is empty/placeholder"
} else {
    Add-Pass "Delete provisioning token resolved"
}

if ($readToken -eq $writeToken -and $writeToken -eq $deleteToken) {
    Add-Warn "Read/write/delete provisioning tokens are identical; consider split tokens for least privilege"
} else {
    Add-Pass "Provisioning token scopes are distinguishable"
}

if (-not $SkipDbChecks) {
    if ([string]::IsNullOrWhiteSpace($dbPath) -or -not (Test-Path -Path $dbPath)) {
        Add-Failure "SQLite file is missing for DB health checks: $dbPath"
    } else {
        $sqlite = Get-Command sqlite3 -ErrorAction SilentlyContinue
        if ($null -eq $sqlite) {
            Add-Failure "sqlite3 command not found; install sqlite3 or rerun with -SkipDbChecks"
        } else {
            $quickCheck = (& $sqlite.Source $dbPath "PRAGMA quick_check;") 2>&1
            if ($LASTEXITCODE -ne 0 -or -not ($quickCheck -join "`n").ToLowerInvariant().Contains("ok")) {
                Add-Failure "SQLite quick_check failed: $($quickCheck -join ' ')"
            } else {
                Add-Pass "SQLite quick_check passed"
            }

            $requiredTables = @(
                "room_mappings",
                "user_mappings",
                "message_mappings",
                "processed_events",
                "dead_letters",
                "media_cache"
            )
            $tableQuery = "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name;"
            $tableRows = (& $sqlite.Source $dbPath $tableQuery) 2>&1
            if ($LASTEXITCODE -ne 0) {
                Add-Failure "Failed to list SQLite tables: $($tableRows -join ' ')"
            } else {
                $existing = @($tableRows | ForEach-Object { $_.Trim() } | Where-Object { $_ })
                $missing = @($requiredTables | Where-Object { $existing -notcontains $_ })
                if ($missing.Count -gt 0) {
                    Add-Failure "Missing required SQLite tables: $($missing -join ', ')"
                } else {
                    Add-Pass "Required SQLite tables exist"
                }

                $countQuery = @"
SELECT 'room_mappings', COUNT(*) FROM room_mappings
UNION ALL SELECT 'user_mappings', COUNT(*) FROM user_mappings
UNION ALL SELECT 'message_mappings', COUNT(*) FROM message_mappings
UNION ALL SELECT 'processed_events', COUNT(*) FROM processed_events
UNION ALL SELECT 'dead_letters', COUNT(*) FROM dead_letters
UNION ALL SELECT 'media_cache', COUNT(*) FROM media_cache;
"@
                $countRows = (& $sqlite.Source $dbPath $countQuery) 2>&1
                if ($LASTEXITCODE -ne 0) {
                    Add-Failure "Failed to read key table counters: $($countRows -join ' ')"
                } else {
                    Add-Pass ("Key table counters: " + (($countRows | ForEach-Object { $_.Trim() } | Where-Object { $_ }) -join "; "))
                }
            }
        }
    }
} else {
    Add-Pass "Skipped DB health checks by request"
}

if (-not $SkipEventChecks) {
    $requiredEvents = @(
        "im.message.receive_v1",
        "im.message.recalled_v1",
        "im.chat.member.user.added_v1",
        "im.chat.member.user.deleted_v1",
        "im.chat.updated_v1",
        "im.chat.disbanded_v1"
    )
    $rawSubscribed = $SubscribedEvents
    if ([string]::IsNullOrWhiteSpace($rawSubscribed)) {
        $rawSubscribed = [string]$env:MATRIX_BRIDGE_FEISHU_SUBSCRIBED_EVENTS
    }

    $subscribed = Split-EventList -Raw $rawSubscribed
    if ($subscribed.Count -eq 0) {
        Add-Failure "Event subscription list is missing. Provide -SubscribedEvents or MATRIX_BRIDGE_FEISHU_SUBSCRIBED_EVENTS"
    } else {
        $missingEvents = @($requiredEvents | Where-Object { $subscribed -notcontains $_.ToLowerInvariant() })
        if ($missingEvents.Count -gt 0) {
            Add-Failure "Missing required subscribed events: $($missingEvents -join ', ')"
        } else {
            Add-Pass "Required event subscriptions are present"
        }
    }
} else {
    Add-Pass "Skipped event subscription checks by request"
}

if (-not $SkipFeishuApiChecks) {
    $apiBase = $FeishuApiBase.TrimEnd("/")
    $tokenUrl = "$apiBase/auth/v3/tenant_access_token/internal"
    $tokenResp = Invoke-Http -Method "POST" -Url $tokenUrl -TimeoutSec 8 -Headers @{} -Body @{
        app_id = [string]$cfg.bridge.app_id
        app_secret = [string]$cfg.bridge.app_secret
    }

    if (-not $tokenResp.Ok) {
        Add-Failure "Feishu tenant token request failed: HTTP=$($tokenResp.StatusCode) $($tokenResp.Error)"
    } else {
        try {
            $tokenJson = $tokenResp.Body | ConvertFrom-Json
            if ($tokenJson.code -ne 0 -or [string]::IsNullOrWhiteSpace([string]$tokenJson.tenant_access_token)) {
                Add-Failure "Feishu tenant token API returned code=$($tokenJson.code) msg=$($tokenJson.msg)"
            } else {
                Add-Pass "Feishu app credentials verified via tenant token API"

                $tenantToken = [string]$tokenJson.tenant_access_token
                $feishuHeaders = @{ Authorization = "Bearer $tenantToken" }

                if (-not [string]::IsNullOrWhiteSpace($ScopeProbeChatId)) {
                    $chatProbe = Invoke-Http -Method "GET" -Url "$apiBase/im/v1/chats/$ScopeProbeChatId" -TimeoutSec 8 -Headers $feishuHeaders -Body $null
                    if (-not $chatProbe.Ok) {
                        Add-Failure "Feishu chat scope probe failed: HTTP=$($chatProbe.StatusCode) $($chatProbe.Error)"
                    } else {
                        $chatJson = $chatProbe.Body | ConvertFrom-Json
                        if ($chatJson.code -eq 0) {
                            Add-Pass "Feishu chat read scope probe passed"
                        } else {
                            Add-Failure "Feishu chat read scope probe returned code=$($chatJson.code) msg=$($chatJson.msg)"
                        }
                    }
                } else {
                    Add-Warn "Skipped Feishu chat scope probe (set -ScopeProbeChatId to enforce)"
                }

                if (-not [string]::IsNullOrWhiteSpace($ScopeProbeUserId)) {
                    $userProbe = Invoke-Http -Method "GET" -Url "$apiBase/contact/v3/users/$ScopeProbeUserId" -TimeoutSec 8 -Headers $feishuHeaders -Body $null
                    if (-not $userProbe.Ok) {
                        Add-Failure "Feishu user scope probe failed: HTTP=$($userProbe.StatusCode) $($userProbe.Error)"
                    } else {
                        $userJson = $userProbe.Body | ConvertFrom-Json
                        if ($userJson.code -eq 0) {
                            Add-Pass "Feishu user read scope probe passed"
                        } else {
                            Add-Failure "Feishu user read scope probe returned code=$($userJson.code) msg=$($userJson.msg)"
                        }
                    }
                } else {
                    Add-Warn "Skipped Feishu user scope probe (set -ScopeProbeUserId to enforce)"
                }
            }
        } catch {
            Add-Failure "Failed to parse Feishu tenant token response: $($_.Exception.Message)"
        }
    }
} else {
    Add-Pass "Skipped Feishu API credential/scope checks by request"
}

if (-not $SkipHttpChecks) {
    $appBase = "http://$($cfg.appservice.hostname):$($cfg.appservice.port)"
    $encodedReadToken = [Uri]::EscapeDataString($readToken)

    $health = Invoke-Http -Method "GET" -Url "$appBase/health" -TimeoutSec 5 -Headers @{} -Body $null
    if ($health.Ok -and $health.StatusCode -eq 200) {
        Add-Pass "Appservice health endpoint reachable: $appBase/health"
    } else {
        Add-Failure "Appservice health endpoint failed: HTTP=$($health.StatusCode) $($health.Error)"
    }

    $statusRead = Invoke-AuthorizedJson -Method "GET" -Url "$appBase/admin/status?access_token=$encodedReadToken" -Token $readToken
    if ($statusRead.Ok -and $statusRead.StatusCode -eq 200) {
        Add-Pass "Read token can access /admin/status"
    } else {
        Add-Failure "Read token failed on /admin/status: HTTP=$($statusRead.StatusCode) $($statusRead.Error)"
    }

    $mappingsRead = Invoke-AuthorizedJson -Method "GET" -Url "$appBase/admin/mappings?limit=1&offset=0" -Token $readToken
    if ($mappingsRead.Ok -and $mappingsRead.StatusCode -eq 200) {
        Add-Pass "Read token can access /admin/mappings"
    } else {
        Add-Failure "Read token failed on /admin/mappings: HTTP=$($mappingsRead.StatusCode) $($mappingsRead.Error)"
    }

    $cleanupRead = Invoke-AuthorizedJson -Method "POST" -Url "$appBase/admin/dead-letters/cleanup" -Token $readToken -Body @{
        status = "replayed"
        limit = 1
        dry_run = $true
    }
    if ($cleanupRead.StatusCode -in @(401, 403)) {
        Add-Pass "Read token is blocked from delete-scope endpoint"
    } else {
        Add-Failure "Read token unexpectedly accessed delete endpoint: HTTP=$($cleanupRead.StatusCode)"
    }

    $replayWrite = Invoke-AuthorizedJson -Method "POST" -Url "$appBase/admin/dead-letters/replay" -Token $writeToken -Body @{
        status = "pending"
        limit = 1
    }
    if ($replayWrite.Ok -and $replayWrite.StatusCode -eq 200) {
        Add-Pass "Write token can access replay endpoint"
    } else {
        Add-Failure "Write token failed on replay endpoint: HTTP=$($replayWrite.StatusCode) $($replayWrite.Error)"
    }

    $cleanupWrite = Invoke-AuthorizedJson -Method "POST" -Url "$appBase/admin/dead-letters/cleanup" -Token $writeToken -Body @{
        status = "replayed"
        limit = 1
        dry_run = $true
    }
    if ($cleanupWrite.StatusCode -in @(401, 403)) {
        Add-Pass "Write token is blocked from delete-scope endpoint"
    } else {
        Add-Failure "Write token unexpectedly accessed delete endpoint: HTTP=$($cleanupWrite.StatusCode)"
    }

    $cleanupDelete = Invoke-AuthorizedJson -Method "POST" -Url "$appBase/admin/dead-letters/cleanup" -Token $deleteToken -Body @{
        status = "replayed"
        limit = 1
        dry_run = $true
    }
    if ($cleanupDelete.Ok -and $cleanupDelete.StatusCode -eq 200) {
        Add-Pass "Delete token can access cleanup endpoint"
    } else {
        Add-Failure "Delete token failed on cleanup endpoint: HTTP=$($cleanupDelete.StatusCode) $($cleanupDelete.Error)"
    }

    $feishuHealth = Invoke-Http -Method "GET" -Url "$listenAddress/health" -TimeoutSec 5 -Headers @{} -Body $null
    if ($feishuHealth.Ok -and $feishuHealth.StatusCode -eq 200) {
        Add-Pass "Feishu webhook health endpoint reachable: $listenAddress/health"
    } else {
        Add-Failure "Feishu webhook health endpoint failed: HTTP=$($feishuHealth.StatusCode) $($feishuHealth.Error)"
    }
} else {
    Add-Pass "Skipped HTTP checks by request"
}

if ($warnings.Count -gt 0) {
    Write-Host ""
    Write-Host "Warnings ($($warnings.Count)):" -ForegroundColor Yellow
    foreach ($warning in $warnings) {
        Write-Host "  - $warning" -ForegroundColor Yellow
    }
}

if ($failures.Count -gt 0) {
    Write-Host ""
    Write-Host "Release checks failed with $($failures.Count) issue(s)." -ForegroundColor Red
    exit 1
}

Write-Host ""
Write-Host "All release checks passed." -ForegroundColor Green
exit 0
