#requires -Version 5.1
<#
.SYNOPSIS
  Pull one official SWE-bench evaluation image and record its digest/size.

.DESCRIPTION
  Official image naming (SWE-bench v3.0.17 harness, swebench/harness/test_spec/test_spec.py):
    <namespace>/sweb.eval.<arch>.<instance_id with "__" -> "_1776_">:<tag>
  with defaults namespace="swebench", tag="latest" (run_evaluation.py --namespace/--instance_image_tag).
  Example:
    swebench/sweb.eval.x86_64.psf_1776_requests-2317:latest

  Docker Hub may be unreachable directly (DNS pollution / firewalled registry).
  -RegistryMirror lets the same official image be pulled through a Docker Hub
  mirror while the recorded Image name stays the canonical one. The mirror is
  only a transport; content is verified by Docker against the manifest digest.

  The script appends a complete transcript to -LogPath, retries failed pulls,
  records RepoDigests/Size, and runs a python/uname smoke test.

.PARAMETER Image
  Official image reference (no mirror host). Default is the smallest candidate
  measured on 2026-10-29 among ten official SWE-bench instance images.

.PARAMETER RegistryMirror
  Optional Docker Hub mirror host prepended at pull time, e.g. docker.1ms.run.
  Pass "" to pull directly from Docker Hub.

.PARAMETER Platform
  Pull platform. Default linux/amd64 (SWE-bench official images are x86_64).

.PARAMETER MaxRetries
  Number of pull attempts (default 3) with exponential-ish backoff.

.PARAMETER LogPath
  Transcript file, relative to the repo root unless absolute.
#>
[CmdletBinding()]
param(
    [string]$Image = "swebench/sweb.eval.x86_64.psf_1776_requests-2317:latest",
    [string]$RegistryMirror = "docker.1panel.live",
    [string]$Platform = "linux/amd64",
    [int]$MaxRetries = 3,
    [int]$RetryDelaySec = 15,
    [string]$LogPath = "logs/lane-docker/pull.log",
    [switch]$SkipSmoke
)

$ErrorActionPreference = "Stop"

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
if (-not [System.IO.Path]::IsPathRooted($LogPath)) {
    $LogPath = Join-Path $RepoRoot $LogPath
}
$LogDir = Split-Path -Parent $LogPath
New-Item -ItemType Directory -Force -Path $LogDir | Out-Null

$script:LogFile = $LogPath
function Write-Log {
    param([string]$Message)
    $line = "[{0:yyyy-MM-dd HH:mm:ss}] {1}" -f (Get-Date), $Message
    Write-Host $line
    Add-Content -Path $script:LogFile -Value $line -Encoding UTF8
}
function Invoke-Logged {
    param([string]$FilePath, [string[]]$ArgumentList)
    $output = & $FilePath @ArgumentList 2>&1
    foreach ($item in $output) {
        $line = $item.ToString()
        Write-Host $line
        Add-Content -Path $script:LogFile -Value $line -Encoding UTF8
    }
    return $LASTEXITCODE
}

Write-Log "=== pull_swebench.ps1 start ==="
Write-Log "Image (canonical): $Image"
Write-Log "Platform: $Platform"
Write-Log "RegistryMirror: '$RegistryMirror'"

# Pre-flight
$dockerVersion = & docker version --format '{{.Server.Version}}' 2>&1
if ($LASTEXITCODE -ne 0) {
    Write-Log "FATAL: docker server unavailable: $dockerVersion"
    exit 2
}
Write-Log "Docker server version: $dockerVersion"

Write-Log "--- docker system df (before) ---"
Invoke-Logged "docker" @("system", "df") | Out-Null
Write-Log "--- docker images (before) ---"
Invoke-Logged "docker" @("images", "--format", "table {{.Repository}}\t{{.Tag}}\t{{.ID}}\t{{.Size}}") | Out-Null

# Resolve pull reference. A first path component counts as a registry host only
# when it contains a dot, a port, or is localhost; otherwise it is Docker Hub.
$hasRegistryHost = $Image -match '^[^/]*(\.[^/]*|:[0-9]+)/' -or $Image -match '^localhost/'
if ([string]::IsNullOrWhiteSpace($RegistryMirror) -or $hasRegistryHost) {
    $pullRef = $Image
} else {
    $pullRef = ($RegistryMirror.TrimEnd('/') + "/" + $Image)
}
Write-Log "Pull reference: $pullRef"

$sw = [System.Diagnostics.Stopwatch]::StartNew()
$pulled = $false
for ($attempt = 1; $attempt -le $MaxRetries; $attempt++) {
    Write-Log "--- docker pull attempt $attempt/$MaxRetries ---"
    $code = Invoke-Logged "docker" @("pull", "--platform", $Platform, $pullRef)
    if ($code -eq 0) {
        $pulled = $true
        break
    }
    Write-Log "pull attempt $attempt failed with exit code $code"
    if ($attempt -lt $MaxRetries) {
        $delay = $RetryDelaySec * $attempt
        Write-Log "retrying in $delay s..."
        Start-Sleep -Seconds $delay
    }
}
$sw.Stop()
if (-not $pulled) {
    Write-Log "FATAL: pull failed after $MaxRetries attempts"
    exit 1
}
$pullSeconds = [math]::Round($sw.Elapsed.TotalSeconds, 1)
Write-Log "Pull succeeded in $pullSeconds s"

# Add the canonical (non-mirror) tag locally so later commands can use the
# official reference even when the image arrived through a transport mirror.
if ($pullRef -ne $Image) {
    Write-Log "Tagging $pullRef as canonical $Image"
    Invoke-Logged "docker" @("tag", $pullRef, $Image) | Out-Null
}

# Digest / size record
Write-Log "--- digest and size ---"
$repoDigests = & docker image inspect --format '{{json .RepoDigests}}' $pullRef 2>&1
Write-Log "RepoDigests: $repoDigests"
$imageId = & docker image inspect --format '{{.Id}}' $pullRef 2>&1
$sizeBytes = & docker image inspect --format '{{.Size}}' $pullRef 2>&1
$created = & docker image inspect --format '{{.Created}}' $pullRef 2>&1
Write-Log "ImageId: $imageId"
Write-Log "SizeBytes: $sizeBytes"
Write-Log "Created: $created"

$digestFile = Join-Path $LogDir "digest.txt"
$record = @(
    "image=$Image",
    "pull_ref=$pullRef",
    "platform=$Platform",
    "repo_digests=$repoDigests",
    "image_id=$imageId",
    "size_bytes=$sizeBytes",
    "created=$created",
    "pull_seconds=$pullSeconds",
    "recorded_at=$((Get-Date).ToString('o'))"
) -join "`n"
Add-Content -Path $digestFile -Value $record -Encoding UTF8
Write-Log "Digest record written to $digestFile"

Write-Log "--- docker system df (after) ---"
Invoke-Logged "docker" @("system", "df") | Out-Null

# Smoke test
if (-not $SkipSmoke) {
    Write-Log "--- smoke test ---"
    $code = Invoke-Logged "docker" @("run", "--rm", "--platform", $Platform, $pullRef, "/bin/sh", "-c", "python --version || true; uname -a")
    if ($code -ne 0) {
        Write-Log "SMOKE FAILED (exit $code)"
        exit 3
    }
    Write-Log "Smoke test passed"
}

Write-Log "=== pull_swebench.ps1 done ==="
exit 0
