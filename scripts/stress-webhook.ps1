param(
    [string]$WebhookUrl = "http://127.0.0.1:38081/webhook",
    [string]$MetricsUrl = "",
    [string]$VerificationToken = "",
    [string]$SigningSecret = "",
    [int]$RequestsPerLevel = 500,
    [int[]]$ConcurrencyLevels = @(5, 10, 20, 40, 80),
    [int]$TimeoutSec = 10,
    [double]$MaxErrorRate = 0.01,
    [int]$MaxP95Ms = 1500
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Get-Percentile {
    param(
        [double[]]$Values,
        [double]$Percentile
    )

    if ($null -eq $Values -or $Values.Count -eq 0) {
        return 0.0
    }

    $sorted = $Values | Sort-Object
    $rank = [Math]::Ceiling(($Percentile / 100.0) * $sorted.Count)
    $index = [Math]::Max(1, [Math]::Min($sorted.Count, [int]$rank)) - 1
    return [double]$sorted[$index]
}

function Get-MetricValue {
    param(
        [string]$MetricsText,
        [string]$MetricName,
        [string]$LabelContains = ""
    )

    if ([string]::IsNullOrWhiteSpace($MetricsText)) {
        return $null
    }

    $lines = $MetricsText -split "`n" | Where-Object {
        $_ -match "^$MetricName(\{| )"
    }

    if (-not [string]::IsNullOrWhiteSpace($LabelContains)) {
        $lines = $lines | Where-Object { $_ -like "*$LabelContains*" }
    }

    if ($null -eq $lines -or $lines.Count -eq 0) {
        return $null
    }

    $tokens = $lines[0].Trim() -split "\s+"
    if ($tokens.Count -lt 2) {
        return $null
    }

    try {
        return [double]$tokens[-1]
    } catch {
        return $null
    }
}

function Get-MetricSnapshot {
    param([string]$Url)

    if ([string]::IsNullOrWhiteSpace($Url)) {
        return $null
    }

    try {
        $raw = Invoke-WebRequest -Uri $Url -TimeoutSec 5 -UseBasicParsing
        $text = [string]$raw.Content
        return [pscustomobject]@{
            QueueDepth = Get-MetricValue -MetricsText $text -MetricName "bridge_queue_depth"
            QueueDepthMax = Get-MetricValue -MetricsText $text -MetricName "bridge_queue_depth_max"
            AckCount = Get-MetricValue -MetricsText $text -MetricName "bridge_processing_duration_ms_count" -LabelContains 'stage="feishu_webhook_ack"'
            AckSum = Get-MetricValue -MetricsText $text -MetricName "bridge_processing_duration_ms_sum" -LabelContains 'stage="feishu_webhook_ack"'
        }
    } catch {
        Write-Warning "Unable to scrape metrics from $Url : $($_.Exception.Message)"
        return $null
    }
}

function Invoke-LevelLoad {
    param(
        [int]$Concurrency,
        [int]$RequestCount
    )

    $runId = [Guid]::NewGuid().ToString("N").Substring(0, 12)
    $results = 1..$RequestCount | ForEach-Object -Parallel {
        $index = $_
        $runId = $using:runId
        $webhookUrl = $using:WebhookUrl
        $timeoutSec = $using:TimeoutSec
        $signingSecret = $using:SigningSecret
        $verificationToken = $using:VerificationToken
        $nowMs = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
        $eventId = "stress-$runId-$index"

        $header = @{
            event_id = $eventId
            event_type = "im.message.receive_v1"
            create_time = "$nowMs"
        }
        if (-not [string]::IsNullOrWhiteSpace($verificationToken)) {
            $header.token = $verificationToken
        }

        $payload = @{
            schema = "2.0"
            header = $header
            event = @{
                sender = @{
                    sender_id = @{
                        open_id = "ou_stress"
                    }
                }
                message = @{
                    message_id = "om_$eventId"
                    chat_id = "oc_stress_chat"
                    chat_type = "group"
                    msg_type = "text"
                    create_time = "$nowMs"
                    content = '{"text":"stress webhook message"}'
                }
            }
        }

        $body = $payload | ConvertTo-Json -Depth 8 -Compress
        $headers = @{
            "Content-Type" = "application/json"
        }

        if (-not [string]::IsNullOrWhiteSpace($signingSecret)) {
            $timestamp = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds().ToString()
            $nonce = [Guid]::NewGuid().ToString("N").Substring(0, 16)
            $toSign = "$timestamp$nonce$signingSecret$body"
            $sha = [System.Security.Cryptography.SHA256]::Create()
            try {
                $bytes = [System.Text.Encoding]::UTF8.GetBytes($toSign)
                $hash = $sha.ComputeHash($bytes)
                $signature = [Convert]::ToHexString($hash).ToLowerInvariant()
            } finally {
                $sha.Dispose()
            }

            $headers["X-Lark-Request-Timestamp"] = $timestamp
            $headers["X-Lark-Request-Nonce"] = $nonce
            $headers["X-Lark-Signature"] = $signature
        }

        $stopwatch = [System.Diagnostics.Stopwatch]::StartNew()
        try {
            $response = Invoke-WebRequest -Method Post -Uri $webhookUrl -Headers $headers -Body $body -TimeoutSec $timeoutSec -UseBasicParsing
            $stopwatch.Stop()
            [pscustomobject]@{
                Success = ($response.StatusCode -ge 200 -and $response.StatusCode -lt 300)
                StatusCode = [int]$response.StatusCode
                LatencyMs = [double]([Math]::Round($stopwatch.Elapsed.TotalMilliseconds, 2))
                Error = ""
            }
        } catch {
            $stopwatch.Stop()
            $statusCode = 0
            if ($_.Exception.Response -and $_.Exception.Response.StatusCode) {
                $statusCode = [int]$_.Exception.Response.StatusCode
            }
            [pscustomobject]@{
                Success = $false
                StatusCode = $statusCode
                LatencyMs = [double]([Math]::Round($stopwatch.Elapsed.TotalMilliseconds, 2))
                Error = [string]$_.Exception.Message
            }
        }
    } -ThrottleLimit $Concurrency

    return $results
}

$summaries = New-Object System.Collections.Generic.List[object]

Write-Host "Webhook stress test started" -ForegroundColor Cyan
Write-Host "Target: $WebhookUrl"
if (-not [string]::IsNullOrWhiteSpace($MetricsUrl)) {
    Write-Host "Metrics: $MetricsUrl"
}
Write-Host "Requests per level: $RequestsPerLevel"
Write-Host "Concurrency levels: $($ConcurrencyLevels -join ', ')"
Write-Host ""

foreach ($level in $ConcurrencyLevels) {
    if ($level -le 0) {
        continue
    }

    Write-Host "Running concurrency=$level ..." -ForegroundColor Yellow
    $before = Get-MetricSnapshot -Url $MetricsUrl
    $started = Get-Date

    $results = @(Invoke-LevelLoad -Concurrency $level -RequestCount $RequestsPerLevel)

    $elapsedSec = [Math]::Max(0.001, ((Get-Date) - $started).TotalSeconds)
    $after = Get-MetricSnapshot -Url $MetricsUrl

    $total = @($results).Count
    $success = @($results | Where-Object { $_.Success }).Count
    $failed = $total - $success
    $errorRate = if ($total -eq 0) { 1.0 } else { $failed / $total }
    $latencies = @($results | ForEach-Object { [double]$_.LatencyMs })

    $p50 = [Math]::Round((Get-Percentile -Values $latencies -Percentile 50), 2)
    $p95 = [Math]::Round((Get-Percentile -Values $latencies -Percentile 95), 2)
    $p99 = [Math]::Round((Get-Percentile -Values $latencies -Percentile 99), 2)
    $rps = [Math]::Round($total / $elapsedSec, 2)

    $queueDepthMaxDelta = $null
    $ackAvgMs = $null
    if ($before -and $after) {
        if ($null -ne $before.QueueDepthMax -and $null -ne $after.QueueDepthMax) {
            $queueDepthMaxDelta = [double]($after.QueueDepthMax - $before.QueueDepthMax)
        }
        if ($null -ne $before.AckCount -and $null -ne $after.AckCount -and $null -ne $before.AckSum -and $null -ne $after.AckSum) {
            $ackCountDelta = [double]($after.AckCount - $before.AckCount)
            $ackSumDelta = [double]($after.AckSum - $before.AckSum)
            if ($ackCountDelta -gt 0) {
                $ackAvgMs = [Math]::Round($ackSumDelta / $ackCountDelta, 2)
            }
        }
    }

    $summary = [pscustomobject]@{
        Concurrency = $level
        Requests = $total
        Success = $success
        Failed = $failed
        ErrorRate = [Math]::Round($errorRate, 4)
        Rps = $rps
        P50Ms = $p50
        P95Ms = $p95
        P99Ms = $p99
        QueueDepthMaxDelta = $queueDepthMaxDelta
        AckAvgMs = $ackAvgMs
    }

    $summaries.Add($summary) | Out-Null
}

Write-Host ""
Write-Host "Summary" -ForegroundColor Cyan
$summaries | Format-Table -AutoSize

$capacity = $summaries |
    Where-Object {
        $_.ErrorRate -le $MaxErrorRate -and
        $_.P95Ms -le $MaxP95Ms -and
        ($null -eq $_.QueueDepthMaxDelta -or $_.QueueDepthMaxDelta -le ($_.Concurrency * 2))
    } |
    Sort-Object Concurrency |
    Select-Object -Last 1

Write-Host ""
if ($null -eq $capacity) {
    Write-Host "No stable capacity boundary found under thresholds." -ForegroundColor Red
    Write-Host "Thresholds: error_rate <= $MaxErrorRate, p95 <= ${MaxP95Ms}ms, queue_depth_max_delta <= concurrency*2"
    exit 1
}

$recommendedConcurrency = [int][Math]::Max(1, [Math]::Floor($capacity.Concurrency * 0.7))
$recommendedRetries = if ($capacity.ErrorRate -lt 0.002) { 2 } else { 1 }
$recommendedRetryBaseMs = if ($capacity.P95Ms -le 500) { 250 } else { 500 }
$recommendedWebhookTimeout = [int][Math]::Max(30, [Math]::Ceiling(($capacity.P99Ms * 3.0) / 1000.0))
$recommendedApiTimeout = [int][Math]::Max(60, [Math]::Ceiling(($capacity.P99Ms * 4.0) / 1000.0))

Write-Host "Capacity boundary" -ForegroundColor Green
Write-Host "  Max stable concurrency: $($capacity.Concurrency)"
Write-Host "  Error rate: $($capacity.ErrorRate)"
Write-Host "  P95/P99 latency: $($capacity.P95Ms)ms / $($capacity.P99Ms)ms"
if ($null -ne $capacity.QueueDepthMaxDelta) {
    Write-Host "  Queue depth max delta: $($capacity.QueueDepthMaxDelta)"
}

Write-Host ""
Write-Host "Recommended starting parameters" -ForegroundColor Green
Write-Host "  bridge worker concurrency: $recommendedConcurrency"
Write-Host "  FEISHU_API_MAX_RETRIES: $recommendedRetries"
Write-Host "  FEISHU_API_RETRY_BASE_MS: $recommendedRetryBaseMs"
Write-Host "  bridge.webhook_timeout (sec): $recommendedWebhookTimeout"
Write-Host "  bridge.api_timeout (sec): $recommendedApiTimeout"

exit 0
