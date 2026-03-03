param(
    [string]$WebhookUrl = "http://127.0.0.1:38081/webhook",
    [string]$MetricsUrl = "",
    [string]$VerificationToken = "",
    [string]$SigningSecret = "",
    [int[]]$BatchSizes = @(100, 300, 600, 1000),
    [int]$Parallelism = 40,
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
        [string]$MetricName
    )

    if ([string]::IsNullOrWhiteSpace($MetricsText)) {
        return $null
    }

    $line = ($MetricsText -split "`n" | Where-Object { $_ -match "^$MetricName(\{| )" } | Select-Object -First 1)
    if ([string]::IsNullOrWhiteSpace($line)) {
        return $null
    }

    $tokens = $line.Trim() -split "\s+"
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
        }
    } catch {
        Write-Warning "Unable to scrape metrics from $Url : $($_.Exception.Message)"
        return $null
    }
}

function Invoke-Batch {
    param(
        [string]$BatchTag,
        [int]$BatchSize
    )

    $results = 1..$BatchSize | ForEach-Object -Parallel {
        $index = $_
        $batchTag = $using:BatchTag
        $webhookUrl = $using:WebhookUrl
        $timeoutSec = $using:TimeoutSec
        $signingSecret = $using:SigningSecret
        $verificationToken = $using:VerificationToken
        $nowMs = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
        $eventId = "batch-$batchTag-$index"

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
                        open_id = "ou_batch"
                    }
                }
                message = @{
                    message_id = "om_$eventId"
                    chat_id = "oc_batch_chat"
                    chat_type = "group"
                    msg_type = "text"
                    create_time = "$nowMs"
                    content = '{"text":"stress batch message"}'
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

        $sw = [System.Diagnostics.Stopwatch]::StartNew()
        try {
            $response = Invoke-WebRequest -Method Post -Uri $webhookUrl -Headers $headers -Body $body -TimeoutSec $timeoutSec -UseBasicParsing
            $sw.Stop()
            [pscustomobject]@{
                Success = ($response.StatusCode -ge 200 -and $response.StatusCode -lt 300)
                StatusCode = [int]$response.StatusCode
                LatencyMs = [double]([Math]::Round($sw.Elapsed.TotalMilliseconds, 2))
                Error = ""
            }
        } catch {
            $sw.Stop()
            $statusCode = 0
            if ($_.Exception.Response -and $_.Exception.Response.StatusCode) {
                $statusCode = [int]$_.Exception.Response.StatusCode
            }
            [pscustomobject]@{
                Success = $false
                StatusCode = $statusCode
                LatencyMs = [double]([Math]::Round($sw.Elapsed.TotalMilliseconds, 2))
                Error = [string]$_.Exception.Message
            }
        }
    } -ThrottleLimit $Parallelism

    return $results
}

$summaries = New-Object System.Collections.Generic.List[object]

Write-Host "Batch webhook stress test started" -ForegroundColor Cyan
Write-Host "Target: $WebhookUrl"
if (-not [string]::IsNullOrWhiteSpace($MetricsUrl)) {
    Write-Host "Metrics: $MetricsUrl"
}
Write-Host "Batch sizes: $($BatchSizes -join ', ')"
Write-Host "Parallelism: $Parallelism"
Write-Host ""

foreach ($batchSize in $BatchSizes) {
    if ($batchSize -le 0) {
        continue
    }

    $tag = [Guid]::NewGuid().ToString("N").Substring(0, 10)
    Write-Host "Running batch_size=$batchSize ..." -ForegroundColor Yellow
    $before = Get-MetricSnapshot -Url $MetricsUrl
    $started = Get-Date

    $results = @(Invoke-Batch -BatchTag $tag -BatchSize $batchSize)

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
    $throughput = [Math]::Round($total / $elapsedSec, 2)

    $queueDepthMaxDelta = $null
    if ($before -and $after -and $null -ne $before.QueueDepthMax -and $null -ne $after.QueueDepthMax) {
        $queueDepthMaxDelta = [double]($after.QueueDepthMax - $before.QueueDepthMax)
    }

    $summary = [pscustomobject]@{
        BatchSize = $batchSize
        Success = $success
        Failed = $failed
        ErrorRate = [Math]::Round($errorRate, 4)
        ThroughputRps = $throughput
        P50Ms = $p50
        P95Ms = $p95
        P99Ms = $p99
        QueueDepthMaxDelta = $queueDepthMaxDelta
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
        ($null -eq $_.QueueDepthMaxDelta -or $_.QueueDepthMaxDelta -le ($Parallelism * 2))
    } |
    Sort-Object BatchSize |
    Select-Object -Last 1

Write-Host ""
if ($null -eq $capacity) {
    Write-Host "No stable batch boundary found under thresholds." -ForegroundColor Red
    Write-Host "Thresholds: error_rate <= $MaxErrorRate, p95 <= ${MaxP95Ms}ms"
    exit 1
}

$recommendedBatchSize = [int][Math]::Max(1, [Math]::Floor($capacity.BatchSize * 0.7))
$recommendedParallelism = [int][Math]::Max(1, [Math]::Floor($Parallelism * 0.8))

Write-Host "Batch capacity boundary" -ForegroundColor Green
Write-Host "  Max stable batch size: $($capacity.BatchSize)"
Write-Host "  Error rate: $($capacity.ErrorRate)"
Write-Host "  P95/P99 latency: $($capacity.P95Ms)ms / $($capacity.P99Ms)ms"
if ($null -ne $capacity.QueueDepthMaxDelta) {
    Write-Host "  Queue depth max delta: $($capacity.QueueDepthMaxDelta)"
}

Write-Host ""
Write-Host "Recommended starting parameters" -ForegroundColor Green
Write-Host "  batch_size: $recommendedBatchSize"
Write-Host "  parallelism: $recommendedParallelism"
Write-Host "  timeout_sec: $TimeoutSec"

exit 0
