#Requires -Version 5.1
<#
.SYNOPSIS
    Runs the Rust/resource gates that CI enforces in .github/workflows/ci.yml.

.DESCRIPTION
    Mirrors the four independent CI resource jobs locally:

        msrv         cargo +<MsrvToolchain> check --all-targets --locked
        deny         cargo deny check                     (repository deny.toml)
        audit        cargo audit --ignore RUSTSEC-2023-0071
        bench-smoke  cargo bench --no-run --locked

    A gate is green only when its process exits 0. A missing tool is a failure
    (RC 127), never a silent skip; use -SkipMsrv/-SkipDeny/-SkipAudit/
    -SkipBench to opt out explicitly.

    Logs are UTF-8, one file per stream (matching scripts/acc_final.ps1):
        logs/lane-resource-gates/<stamp>/<gate>.log
        logs/lane-resource-gates/<stamp>/<gate>.log.err
        logs/lane-resource-gates/<stamp>/summary.txt

    Exit code: 0 only when every selected gate exits 0; otherwise the first
    failing gate's RC.

.PARAMETER TargetDir
    CARGO_TARGET_DIR for the run. Defaults to $env:CARGO_TARGET_DIR when set,
    otherwise <repo>/target-alt (the GATE-README convention). A lane must pass
    its own pool directory, e.g. -TargetDir target-lane-f.

.PARAMETER MsrvToolchain
    Toolchain passed to cargo as +<toolchain>. Defaults to 1.97, matching
    [package].rust-version in Cargo.toml.

.EXAMPLE
    powershell -NoProfile -File scripts/gate_resources.ps1 -TargetDir target-lane-f

.EXAMPLE
    powershell -NoProfile -File scripts/gate_resources.ps1 -SkipDeny -SkipAudit
#>
[CmdletBinding()]
param(
    [string]$TargetDir = "",
    [string]$MsrvToolchain = "1.97",
    [switch]$SkipMsrv,
    [switch]$SkipDeny,
    [switch]$SkipAudit,
    [switch]$SkipBench
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = "Stop"

$repo = Split-Path -Parent $PSScriptRoot
if ([string]::IsNullOrWhiteSpace($TargetDir)) {
    $envTarget = [Environment]::GetEnvironmentVariable("CARGO_TARGET_DIR")
    if (-not [string]::IsNullOrWhiteSpace($envTarget)) {
        $TargetDir = $envTarget
    }
    else {
        $TargetDir = Join-Path $repo "target-alt"
    }
}
$TargetDir = [System.IO.Path]::GetFullPath($TargetDir)

$stamp = Get-Date -Format "yyyyMMdd-HHmmss"
$logDir = Join-Path $repo ("logs\lane-resource-gates\" + $stamp)
New-Item -ItemType Directory -Force -Path $logDir | Out-Null

$env:CARGO_TERM_COLOR = "never"
$env:CARGO_TARGET_DIR = $TargetDir
$env:CARGO_INCREMENTAL = "0"
$env:CARGO_PROFILE_BENCH_DEBUG = "0"

Write-Host "gate_resources: repo=$repo"
Write-Host "gate_resources: CARGO_TARGET_DIR=$TargetDir"
Write-Host "gate_resources: logs=$logDir"

$results = @()

function Invoke-Gate {
    param(
        [Parameter(Mandatory = $true)][string]$Name,
        [Parameter(Mandatory = $true)][string]$Exe,
        [Parameter(Mandatory = $true)][string]$ToolName,
        [Parameter(Mandatory = $true)][string[]]$Arguments,
        [Parameter(Mandatory = $true)][string]$InstallHint
    )

    $stdout = Join-Path $logDir ($Name + ".log")
    $stderr = Join-Path $logDir ($Name + ".log.err")

    Write-Host ""
    Write-Host ("== {0} ==" -f $Name) -ForegroundColor Cyan
    Write-Host ("   {0} {1}" -f $Exe, ($Arguments -join " "))

    if (-not (Get-Command $ToolName -ErrorAction SilentlyContinue)) {
        Write-Host ("   MISSING TOOL: {0}" -f $InstallHint) -ForegroundColor Red
        "" | Out-File -FilePath $stdout -Encoding utf8
        $InstallHint | Out-File -FilePath $stderr -Encoding utf8
        $script:results += [pscustomobject]@{ Gate = $Name; RC = 127; Seconds = 0.0 }
        return
    }

    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $proc = Start-Process -FilePath $Exe -ArgumentList $Arguments -NoNewWindow -Wait -PassThru `
        -RedirectStandardOutput $stdout -RedirectStandardError $stderr
    $sw.Stop()

    $rc = $proc.ExitCode
    $secs = [math]::Round($sw.Elapsed.TotalSeconds, 1)
    if ($rc -eq 0) {
        Write-Host ("   RC=0 {0}s" -f $secs) -ForegroundColor Green
    }
    else {
        Write-Host ("   RC={0} {1}s (see {2})" -f $rc, $secs, $stderr) -ForegroundColor Red
    }
    $script:results += [pscustomobject]@{ Gate = $Name; RC = $rc; Seconds = $secs }
}

if (-not $SkipMsrv) {
    Invoke-Gate -Name "msrv" -Exe "cargo" -ToolName "cargo" `
        -Arguments @("+$MsrvToolchain", "check", "--all-targets", "--locked") `
        -InstallHint "rustup toolchain install $MsrvToolchain --profile minimal"
}
if (-not $SkipDeny) {
    Invoke-Gate -Name "deny" -Exe "cargo" -ToolName "cargo-deny" `
        -Arguments @("deny", "check") `
        -InstallHint "cargo install cargo-deny --locked (or download the release binary)"
}
if (-not $SkipAudit) {
    Invoke-Gate -Name "audit" -Exe "cargo" -ToolName "cargo-audit" `
        -Arguments @("audit", "--ignore", "RUSTSEC-2023-0071") `
        -InstallHint "cargo install cargo-audit --locked (or download the release binary)"
}
if (-not $SkipBench) {
    Invoke-Gate -Name "bench-smoke" -Exe "cargo" -ToolName "cargo" `
        -Arguments @("bench", "--no-run", "--locked") `
        -InstallHint "cargo (ships with rustup)"
}

$summary = Join-Path $logDir "summary.txt"
$lines = @()
$lines += "gate_resources summary " + (Get-Date -Format "yyyy-MM-dd HH:mm:ss")
$lines += "CARGO_TARGET_DIR=$TargetDir"
$lines += ""
$lines += ("{0,-12} {1,4} {2,8}" -f "GATE", "RC", "SECONDS")
foreach ($r in $results) {
    $lines += ("{0,-12} {1,4} {2,8}" -f $r.Gate, $r.RC, $r.Seconds)
}
$lines | Out-File -FilePath $summary -Encoding utf8
$lines | ForEach-Object { Write-Host $_ }

$failed = @($results | Where-Object { $_.RC -ne 0 })
if ($failed.Count -gt 0) {
    Write-Host ""
    Write-Host ("FAILED gate(s): " + (($failed | ForEach-Object { $_.Gate }) -join ", ")) -ForegroundColor Red
    exit $failed[0].RC
}
Write-Host ""
Write-Host "All selected gates passed." -ForegroundColor Green
exit 0
