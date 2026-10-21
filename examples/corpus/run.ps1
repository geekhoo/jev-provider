#!/usr/bin/env pwsh
# Send the corpus cases to POST /v1/systemone and save the responses (PowerShell 7).
# Usage: ./run.ps1 [-BaseUrl <url>] [-Model <alias>] [-OutDir <dir>] [-Filter <domain|Ln|domain/Ln>]
# The token is read from the JEV_TOKEN environment variable and sent as a Bearer token.
# It is never printed.
param(
    [string]$BaseUrl = 'http://127.0.0.1:8080',
    [string]$Model = 'jev-latest',
    [string]$OutDir = '',
    [string]$Filter = ''
)
$ErrorActionPreference = 'Stop'

$root = $PSScriptRoot
if (-not $OutDir) { $OutDir = Join-Path '.' ("runs/{0}-{1}" -f $Model, (Get-Date -Format 'yyyyMMdd-HHmmss')) }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

$fDom = ''; $fLev = ''
if ($Filter) {
    if ($Filter -match '/') { $fDom, $fLev = $Filter -split '/', 2 }
    elseif ($Filter -match '^L\d$') { $fLev = $Filter }
    else { $fDom = $Filter }
}

$headers = @{ 'Content-Type' = 'application/json' }
if ($env:JEV_TOKEN) { $headers['Authorization'] = "Bearer $($env:JEV_TOKEN)" }
$utf8 = [System.Text.UTF8Encoding]::new($false)
$uri = $BaseUrl.TrimEnd('/') + '/v1/systemone'

$nOk = 0; $n4 = 0; $n5 = 0; $nOther = 0; $nConn = 0; $total = 0
Write-Host ("Base URL: {0}   Model: {1}   Output: {2}   Token: {3}" -f $BaseUrl, $Model, $OutDir, $(if ($env:JEV_TOKEN) { 'set' } else { 'not set' }))

$files = Get-ChildItem -Path $root -Directory | ForEach-Object {
    Get-ChildItem -Path $_.FullName -File -Filter 'L?.json'
} | Where-Object { $_.Name -match '^L\d\.json$' } | Sort-Object { $_.Directory.Name }, Name

foreach ($f in $files) {
    $lev = $f.BaseName
    $dom = $f.Directory.Name
    if ($fDom -and $fDom -ne $dom) { continue }
    if ($fLev -and $fLev -ne $lev) { continue }
    $caseId = "$dom-$lev"
    $total++

    $obj = [System.IO.File]::ReadAllText($f.FullName, $utf8) | ConvertFrom-Json -Depth 64
    $obj | Add-Member -NotePropertyName model -NotePropertyValue $Model -Force
    $bytes = $utf8.GetBytes(($obj | ConvertTo-Json -Depth 64 -Compress))

    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    try {
        $resp = Invoke-WebRequest -Uri $uri -Method Post -Headers $headers -Body $bytes `
            -SkipHttpErrorCheck -TimeoutSec 300
    } catch {
        $nConn++
        '{0,-14} {1,-8} {2}' -f $caseId, 'FAILED', ("connect error: " + $_.Exception.Message) | Write-Host
        continue
    }
    $sw.Stop()
    $ms = [int]$sw.ElapsedMilliseconds
    $status = [int]$resp.StatusCode

    $rid = $null; $mode = $null
    if ($resp.Headers['x-request-id']) { $rid = [string]($resp.Headers['x-request-id'] | Select-Object -First 1) }
    if ($resp.Headers['x-system-one-response-mode']) { $mode = [string]($resp.Headers['x-system-one-response-mode'] | Select-Object -First 1) }

    $raw = $resp.Content
    if ($raw -is [byte[]]) { $raw = $utf8.GetString($raw) }
    try { $bodyVal = $raw | ConvertFrom-Json -Depth 64 } catch { $bodyVal = [string]$raw }

    $record = [ordered]@{
        case = $caseId; http_status = $status; request_id = $rid
        response_mode = $mode; latency_ms = $ms; body = $bodyVal
    }
    $text = ($record | ConvertTo-Json -Depth 64) + "`n"
    [System.IO.File]::WriteAllText((Join-Path $OutDir "$caseId.response.json"), $text, $utf8)

    if ($status -ge 200 -and $status -lt 300) { $nOk++ }
    elseif ($status -ge 400 -and $status -lt 500) { $n4++ }
    elseif ($status -ge 500) { $n5++ }
    else { $nOther++ }
    '{0,-14} {1,-8} {2} ms' -f $caseId, $status, $ms | Write-Host
}

$failed = $n4 + $n5 + $nOther + $nConn
$rate = if ($total -eq 0) { 0.0 } else { [math]::Round(100.0 * $failed / $total, 1) }
Write-Host ''
'{0,-12} {1}' -f 'Total', $total | Write-Host
'{0,-12} {1}' -f 'ok (2xx)', $nOk | Write-Host
'{0,-12} {1}' -f '4xx', $n4 | Write-Host
'{0,-12} {1}' -f '5xx', $n5 | Write-Host
'{0,-12} {1}' -f 'no connect', $nConn | Write-Host
'{0,-12} {1}%' -f 'failure rate', $rate | Write-Host
Write-Host "Responses saved in: $OutDir"
if ($nConn -gt 0) { exit 1 }
exit 0
