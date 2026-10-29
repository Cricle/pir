#requires -Version 5.1
<#
.SYNOPSIS
  Pull every SWE-bench Lite (300 instances, 12 repos) evaluation image from
  Docker Hub mirrors, grouped by repo/version so shared layers are downloaded
  once, with resume, digest verification, disk protection and a full report.

.DESCRIPTION
  Official image naming (SWE-bench v3.0.17 harness, test_spec.py):
    swebench/sweb.eval.<arch>.<instance_id with "__" -> "_1776_">:<tag>
  e.g. psf__requests-1142 -> swebench/sweb.eval.x86_64.psf_1776_requests-1142:latest

  The instance inventory is read from logs/swebench/data/test.parquet through
  scripts/docker/lite_instances.py (pyarrow). Nothing is hand-maintained.

  Work is done in three phases:
    1. manifest scan  - index digest, linux/amd64 digest, layer list/sizes for
                        all instances (cached in manifest-cache.json, resumable)
    2. local scan     - classify images as skipped (present + digest matches),
                        existing_mismatch, or pending
    3. pulls          - queued group by group (repo, then version, then issue
                        number) so same-version images share base layers; up to
                        MaxParallel workers (default 2); each worker retries
                        with two attempts per mirror before rotating and
                        exponential backoff. `docker pull` resumes layers.

  Disk protection is checked before every dispatch:
    * stop when C: free space < -MinFreeBytes (default 60 GiB)
    * stop when Docker's total reported usage > -MaxDockerBytes (default 500 GiB)
  The script never runs prune and never deletes images, containers or volumes.
  When it stops for disk reasons it writes the remaining (pending) list and a
  report with the exact watermarks.

  Outputs (under -LogDir, default logs/lane-docker-lite):
    REPORT.md, summary.json, summary.csv, pending.txt, pending.csv,
    manifest-cache.json, disk-baseline.json, disk-final.json, progress.log,
    pull-lite.log, pull-<instance>.log / -attemptN.out|err.log, worker-<instance>.json

.PARAMETER Dataset
  Parquet dataset path (default logs/swebench/data/test.parquet).

.PARAMETER InstancesJson
  Optional pre-extracted inventory JSON (skips the Python/parquet read).

.PARAMETER PythonExe
  Python interpreter with pyarrow. Default: PIR_PYTHON env var, then
  C:\wp\empty\tools\python312\python.exe, then `python`/`python3` on PATH.

.PARAMETER Repo
  Only pull these repos (repeatable), e.g. -Repo django/django.

.PARAMETER Instance
  Only pull these instance ids (repeatable).

.PARAMETER Mirror
  Registry mirrors in fallback order. Default: docker.1panel.live, docker.1ms.run.
  Use "direct" as an entry to try Docker Hub directly.

.PARAMETER MaxParallel
  Concurrent `docker pull` workers. 1-2 recommended (each pull opens ~3 layer
  connections); values above 2 are clamped to 2 to protect the mirrors.

.PARAMETER MaxRetries
  Total pull attempts per instance across the mirror rotation.

.PARAMETER RetryDelaySec
  Base backoff delay; attempt N waits RetryDelaySec * min(N,5) seconds + jitter.

.PARAMETER AttemptTimeoutMin
  Kill and retry one `docker pull` after this many minutes (layers resume).

.PARAMETER SkipPull
  Only build the inventory, scan manifests and classify local images.

.PARAMETER ForcePull
  Pull even when a matching image already exists.

.PARAMETER ForceManifest
  Re-query manifest data instead of using manifest-cache.json.

.PARAMETER CrossCheckManifests
  Query every mirror for every manifest and compare digests (slower).

.PARAMETER Limit
  Stop after N instances (debug/smoke test).
#>
[CmdletBinding()]
param(
    [string]$Dataset = 'logs/swebench/data/test.parquet',
    [string]$InstancesJson = '',
    [string]$PythonExe = '',
    [string[]]$Repo = @(),
    [string[]]$Instance = @(),
    [string[]]$Mirror = @('docker.1panel.live', 'docker.1ms.run'),
    [int]$MaxParallel = 2,
    [int]$MaxRetries = 6,
    [int]$RetryDelaySec = 20,
    [int]$AttemptTimeoutMin = 60,
    [string]$Platform = 'linux/amd64',
    [string]$LogDir = 'logs/lane-docker-lite',
    [int64]$MinFreeBytes = 60GB,
    [int64]$MaxDockerBytes = 500GB,
    [double]$DockerEstimateFactor = 4.0,
    [int]$Limit = 0,
    [switch]$SkipPull,
    [switch]$ForcePull,
    [switch]$ForceManifest,
    [switch]$CrossCheckManifests,
    [switch]$NoPythonManifestScan,
    [Parameter(DontShow = $true)][switch]$Worker,
    [Parameter(DontShow = $true)][string]$WorkerInstance,
    [Parameter(DontShow = $true)][string]$WorkerRepo,
    [Parameter(DontShow = $true)][string]$WorkerVersion,
    [Parameter(DontShow = $true)][string]$WorkerImage,
    [Parameter(DontShow = $true)][string]$WorkerMirrors,
    [Parameter(DontShow = $true)][string]$WorkerLog,
    [Parameter(DontShow = $true)][string]$WorkerIndexDigest,
    [Parameter(DontShow = $true)][string]$WorkerAmd64Digest
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
if (-not [System.IO.Path]::IsPathRooted($LogDir)) { $LogDir = Join-Path $RepoRoot $LogDir }
New-Item -ItemType Directory -Force -Path $LogDir | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $LogDir 'manifests') | Out-Null
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

if ($MaxParallel -lt 1) { $MaxParallel = 1 }
if ($MaxParallel -gt 2) {
    Write-Host "MaxParallel=$MaxParallel clamped to 2 (mirror rate-limit protection)"
    $MaxParallel = 2
}

# ---------------------------------------------------------------- utilities

function Write-Utf8NoBom {
    param([string]$Path, [string]$Text)
    $enc = New-Object System.Text.UTF8Encoding($false)
    [System.IO.File]::WriteAllText($Path, $Text, $enc)
}

function Write-JsonAtomic {
    param([string]$Path, $Object, [int]$Depth = 10)
    $text = ConvertTo-Json -InputObject $Object -Depth $Depth
    $tmp = "$Path.tmp"
    Write-Utf8NoBom -Path $tmp -Text $text
    Move-Item -Force -Path $tmp -Destination $Path
}

function Format-Bytes {
    param($Bytes)
    if ($null -eq $Bytes) { return '-' }
    $b = [double]$Bytes
    if ($b -ge 1TB) { return ('{0:N2} TiB' -f ($b / 1TB)) }
    if ($b -ge 1GB) { return ('{0:N2} GiB' -f ($b / 1GB)) }
    if ($b -ge 1MB) { return ('{0:N1} MiB' -f ($b / 1MB)) }
    if ($b -ge 1KB) { return ('{0:N1} KiB' -f ($b / 1KB)) }
    return ('{0} B' -f [int64]$b)
}

function Format-BytesDecimal {
    param($Bytes)
    if ($null -eq $Bytes) { return '-' }
    $b = [double]$Bytes
    if ($b -ge 1e12) { return ('{0:N2} TB' -f ($b / 1e12)) }
    if ($b -ge 1e9) { return ('{0:N2} GB' -f ($b / 1e9)) }
    if ($b -ge 1e6) { return ('{0:N1} MB' -f ($b / 1e6)) }
    if ($b -ge 1e3) { return ('{0:N1} kB' -f ($b / 1e3)) }
    return ('{0} B' -f [int64]$b)
}

function Write-Log {
    param([string]$Path, [string]$Message)
    $line = '[{0:yyyy-MM-dd HH:mm:ss}] {1}' -f (Get-Date), $Message
    Write-Host $line
    Add-Content -Path $Path -Value $line -Encoding UTF8
}

function ConvertTo-ImageRef {
    param([string]$Value)
    if ($Value -match '/' -or $Value -match ':') { return $Value }
    $norm = $Value -replace '__', '_1776_'
    return "swebench/sweb.eval.x86_64.${norm}:latest"
}

function Convert-HumanSizeToBytes {
    param([string]$Text)
    if (-not $Text) { return [int64]0 }
    $t = $Text.Trim()
    if ($t -match '^([0-9]+(?:\.[0-9]+)?)\s*([kKmMgGtT]?)[bB]$') {
        $v = [double]$matches[1]
        switch ($matches[2].ToUpperInvariant()) {
            'K' { $v = $v * 1e3 }
            'M' { $v = $v * 1e6 }
            'G' { $v = $v * 1e9 }
            'T' { $v = $v * 1e12 }
        }
        return [int64]$v
    }
    return [int64]0
}

function Get-CFreeBytes {
    try {
        $d = Get-CimInstance Win32_LogicalDisk -Filter "DeviceID='C:'"
        return [int64]$d.FreeSpace
    } catch { return $null }
}

function Get-DockerVhdxBytes {
    $candidates = @(
        (Join-Path $env:LOCALAPPDATA 'Docker\wsl\disk\docker_data.vhdx'),
        (Join-Path $env:LOCALAPPDATA 'Docker\wsl\data\ext4.vhdx'),
        (Join-Path $env:ProgramData 'DockerDesktop\vm-data\DockerDesktop.vhdx')
    )
    foreach ($p in $candidates) {
        if ($p -and (Test-Path $p)) {
            try { return [pscustomobject]@{ Path = $p; Bytes = [int64](Get-Item $p).Length } } catch { }
        }
    }
    return [pscustomobject]@{ Path = $null; Bytes = $null }
}

function Get-DockerUsage {
    $rows = @()
    $prevEap = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $lines = & docker system df --format '{{json .}}' 2>$null
        $code = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $prevEap
    }
    if ($code -ne 0) { return [pscustomobject]@{ TotalBytes = $null; ImagesBytes = $null; Rows = @(); Error = 'docker system df failed' } }
    foreach ($line in @($lines)) {
        if (-not $line) { continue }
        try {
            $o = $line | ConvertFrom-Json
            $rows += [pscustomobject]@{
                Type = [string]$o.Type
                Count = [int]$o.TotalCount
                Active = [int]$o.Active
                Bytes = Convert-HumanSizeToBytes ([string]$o.Size)
            }
        } catch { }
    }
    $total = ($rows | Measure-Object -Property Bytes -Sum).Sum
    $images = ($rows | Where-Object { $_.Type -eq 'Images' } | Measure-Object -Property Bytes -Sum).Sum
    return [pscustomobject]@{
        TotalBytes = [int64]$total
        ImagesBytes = [int64]$images
        Rows = $rows
        Error = $null
    }
}

function Get-DiskGuardState {
    $free = Get-CFreeBytes
    $usage = Get-DockerUsage
    $reason = $null
    if ($null -ne $free -and $free -lt $MinFreeBytes) {
        $reason = "C: free $(Format-Bytes $free) is below threshold $(Format-Bytes $MinFreeBytes)"
    } elseif ($null -ne $usage.TotalBytes -and $usage.TotalBytes -gt $MaxDockerBytes) {
        $reason = "Docker usage $(Format-BytesDecimal $usage.TotalBytes) exceeds threshold $(Format-BytesDecimal $MaxDockerBytes)"
    }
    return [pscustomobject]@{
        FreeBytes = $free
        Docker    = $usage
        Reason    = $reason
        CheckedAt = (Get-Date).ToString('o')
    }
}

function Get-ImageSnapshot {
    param([string]$Ref)
    $prevEap = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $out = & docker image inspect --format '{{json .}}' $Ref 2>$null
        $code = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $prevEap
    }
    if ($code -ne 0 -or -not $out) { return $null }
    try {
        $obj = ($out -join "`n") | ConvertFrom-Json
        if ($obj -is [array]) { $obj = $obj[0] }
        return [pscustomobject]@{
            Ref         = $Ref
            Id          = [string]$obj.Id
            SizeBytes   = [int64]$obj.Size
            Created     = [string]$obj.Created
            RepoDigests = (@($obj.RepoDigests) -join ';')
        }
    } catch { return $null }
}

function Test-DigestMatch {
    param($Snapshot, [string]$IndexDigest, [string]$Amd64Digest)
    if (-not $Snapshot) { return $false }
    foreach ($d in @($IndexDigest, $Amd64Digest)) {
        if (-not $d) { continue }
        if ($Snapshot.Id -eq $d) { return $true }
        foreach ($rd in ($Snapshot.RepoDigests -split ';')) {
            if ($rd.EndsWith("@$d")) { return $true }
        }
    }
    return $false
}

function Find-LocalImage {
    param([string]$Image, [string]$IndexDigest, [string]$Amd64Digest)
    $candidates = @($Image)
    foreach ($m in ($Mirror | Where-Object { $_ -ne 'direct' })) { $candidates += "$m/$Image" }
    $first = $null
    foreach ($ref in $candidates) {
        $snap = Get-ImageSnapshot $ref
        if (-not $snap) { continue }
        if (-not $first) { $first = [pscustomobject]@{ Snapshot = $snap; Ref = $ref; Match = $false } }
        if (Test-DigestMatch $snap $IndexDigest $Amd64Digest) {
            return [pscustomobject]@{ Snapshot = $snap; Ref = $ref; Match = $true }
        }
    }
    return $first
}

# --------------------------------------------------------- registry manifests
# Manifest data is read with plain registry v2 HTTP requests (HEAD/GET
# /v2/<repo>/manifests/<ref>). Docker CLI's buildx front-end proved very slow
# and intermittently 524s through the mirror, while direct HTTPS requests are
# fast and expose Docker-Content-Digest. Docker pull remains the transport.

function Get-RegistryToken {
    param([string]$Challenge, [string]$MirrorName)
    $realm = $null; $service = $null; $scope = $null
    if ($Challenge -match 'realm="([^"]+)"') { $realm = $matches[1] }
    if ($Challenge -match 'service="([^"]+)"') { $service = $matches[1] }
    if ($Challenge -match 'scope="([^"]+)"') { $scope = $matches[1] }
    if (-not $realm) { return $null }
    $sep = if ($realm.Contains('?')) { '&' } else { '?' }
    $url = "$realm${sep}service=$([uri]::EscapeDataString($service))&scope=$([uri]::EscapeDataString($scope))"
    $tok = Invoke-RestMethod -Uri $url -UseBasicParsing -TimeoutSec 60
    if ($tok.token) { return $tok.token }
    if ($tok.access_token) { return $tok.access_token }
    return $null
}

function Invoke-RegistryRequest {
    param([string]$MirrorName, [string]$Path, [string]$Accept)
    $token = $null
    for ($try = 1; $try -le 3; $try++) {
        try {
            $headers = @{}
            if ($Accept) { $headers['Accept'] = $Accept }
            if ($token) { $headers['Authorization'] = "Bearer $token" }
            $resp = Invoke-WebRequest -Uri "https://$MirrorName$Path" -Headers $headers -UseBasicParsing -TimeoutSec 90
            $raw = $resp.Content
            if ($raw -is [byte[]]) {
                $text = [System.Text.Encoding]::UTF8.GetString($raw)
            } else {
                $text = [string]$raw
            }
            $digest = $null
            try {
                $dh = $resp.Headers['Docker-Content-Digest']
                if ($dh -is [array]) { $dh = $dh[0] }
                $digest = [string]$dh
            } catch { }
            return [pscustomobject]@{ StatusCode = [int]$resp.StatusCode; Text = $text; Digest = $digest; Headers = $resp.Headers }
        } catch {
            $status = 0
            $challenge = $null
            $errResp = $_.Exception.Response
            if ($errResp) {
                try { $status = [int]$errResp.StatusCode } catch { }
                try {
                    $challenge = [string]$errResp.Headers['WWW-Authenticate']
                } catch { }
            }
            if ($status -eq 401 -and -not $token -and $challenge) {
                $token = Get-RegistryToken -Challenge $challenge -MirrorName $MirrorName
                if ($token) { continue }
            }
            if ($try -lt 3 -and ($status -eq 0 -or $status -eq 429 -or $status -ge 500)) {
                Start-Sleep -Seconds (3 * $try)
                continue
            }
            throw
        }
    }
    throw "registry request failed: $MirrorName$Path"
}

function Get-ImageManifestInfo {
    param([string]$MirrorName, [string]$Image)
    if ($MirrorName -eq 'direct') { $MirrorName = 'registry-1.docker.io' }
    $m = [regex]::Match($Image, '^(?<repo>[^:]+)(?::(?<tag>[^:]+))?$')
    if (-not $m.Success) { throw "cannot parse image reference: $Image" }
    $repoName = $m.Groups['repo'].Value
    $tagName = $m.Groups['tag'].Value
    if (-not $tagName) { $tagName = 'latest' }
    $indexPath = "/v2/$repoName/manifests/$tagName"
    $indexAccept = 'application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json'
    $indexResp = Invoke-RegistryRequest -MirrorName $MirrorName -Path $indexPath -Accept $indexAccept
    $indexText = $indexResp.Text
    $idx = $indexText | ConvertFrom-Json
    $indexDigest = $indexResp.Digest
    if (-not $indexDigest) { throw 'Docker-Content-Digest missing from index response' }

    $amdDigest = $null
    $layers = @()
    if ($idx.PSObject.Properties.Name -contains 'manifests') {
        $amdEntry = @($idx.manifests | Where-Object {
            $_.platform -and $_.platform.os -eq 'linux' -and $_.platform.architecture -eq 'amd64'
        }) | Select-Object -First 1
        if (-not $amdEntry) { throw 'no linux/amd64 manifest in index' }
        $amdDigest = [string]$amdEntry.digest
    } elseif ($idx.PSObject.Properties.Name -contains 'layers') {
        $amdDigest = $indexDigest
        $layers = @($idx.layers)
    } else {
        $amdDigest = $indexDigest
    }

    $amdText = $null
    if ($layers.Count -eq 0 -and $amdDigest) {
        $manifestPath = "/v2/$repoName/manifests/$amdDigest"
        $manifestAccept = 'application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json'
        $manifestResp = Invoke-RegistryRequest -MirrorName $MirrorName -Path $manifestPath -Accept $manifestAccept
        $amdText = $manifestResp.Text
        $mj = $amdText | ConvertFrom-Json
        if ($mj.layers) { $layers = @($mj.layers) }
    }

    $compressed = [int64]0
    $layerList = @()
    foreach ($l in $layers) {
        if (-not $l) { continue }
        $size = [int64]$l.size
        $compressed += $size
        if ($l.digest) {
            $layerList += [pscustomobject]@{ digest = [string]$l.digest; size = $size }
        }
    }

    return [pscustomobject]@{
        Mirror          = $MirrorName
        IndexDigest     = $indexDigest
        Amd64Digest     = $amdDigest
        CompressedBytes = $compressed
        LayerCount      = $layerList.Count
        Layers          = $layerList
        IndexJson       = $indexText
        Amd64Json       = $amdText
    }
}

function Get-ImageManifestInfoFallback {
    param([string]$MirrorName, [string]$Image)
    $ref = if ($MirrorName -eq 'direct') { $Image } else { "$MirrorName/$Image" }
    $verboseJson = & docker manifest inspect -v $ref 2>&1
    if ($LASTEXITCODE -ne 0 -or -not $verboseJson) { throw "docker manifest inspect failed: $(($verboseJson -join ' ').Trim())" }
    $verboseText = ($verboseJson -join "`n")
    $verbose = $verboseText | ConvertFrom-Json
    if ($verbose -isnot [array]) { $verbose = @($verbose) }
    $amdEntry = $verbose | Where-Object {
        $_.Descriptor -and $_.Descriptor.platform -and
        $_.Descriptor.platform.os -eq 'linux' -and $_.Descriptor.platform.architecture -eq 'amd64'
    } | Select-Object -First 1
    if (-not $amdEntry) { $amdEntry = $verbose[0] }
    $amdDigest = if ($amdEntry -and $amdEntry.Descriptor) { [string]$amdEntry.Descriptor.digest } else { $null }
    $layers = @()
    if ($amdEntry -and $amdEntry.OCIManifest) { $layers = @($amdEntry.OCIManifest.layers) }
    $compressed = [int64]0
    $layerList = @()
    foreach ($l in $layers) {
        if (-not $l) { continue }
        $size = [int64]$l.size
        $compressed += $size
        if ($l.digest) { $layerList += [pscustomobject]@{ digest = [string]$l.digest; size = $size } }
    }
    return [pscustomobject]@{
        Mirror = $MirrorName; IndexDigest = $null; Amd64Digest = $amdDigest
        CompressedBytes = $compressed; LayerCount = $layerList.Count; Layers = $layerList
        IndexJson = $null; Amd64Json = $verboseText
    }
}

function Get-ManifestWithRetry {
    param([string]$InstanceId, [string]$Image, [string]$LogPath)
    $errors = @()
    foreach ($m in $Mirror) {
        for ($attempt = 1; $attempt -le 2; $attempt++) {
            try {
                return Get-ImageManifestInfo -MirrorName $m -Image $Image
            } catch {
                $errors += "$m#$attempt`: $($_.Exception.Message)"
                if ($attempt -lt 2) { Start-Sleep -Seconds (5 * $attempt) }
            }
        }
    }
    foreach ($m in $Mirror) {
        try {
            $fb = Get-ImageManifestInfoFallback -MirrorName $m -Image $Image
            Write-Log $LogPath "WARN manifest HTTP failed for $InstanceId; used docker CLI fallback via $m (index digest unavailable)"
            return $fb
        } catch {
            $errors += "$m(fallback): $($_.Exception.Message)"
        }
    }
    throw ($errors -join ' | ')
}

# -------------------------------------------------------------------- worker

function Invoke-PullWorker {
    $instance = $WorkerInstance
    $image = $WorkerImage
    $safe = $instance -replace '[^A-Za-z0-9.-]', '_'
    $log = $WorkerLog
    $resultPath = Join-Path $LogDir ("worker-{0}.json" -f $safe)
    Write-Log $log "=== worker start instance=$instance image=$image ==="

    function Save-WorkerResult($obj) { Write-JsonAtomic -Path $resultPath -Object $obj -Depth 10 }

    $swTotal = [System.Diagnostics.Stopwatch]::StartNew()
    $startedAt = (Get-Date).ToString('o')
    $hasExpectation = [bool]($WorkerIndexDigest -or $WorkerAmd64Digest)
    $snap = Get-ImageSnapshot $image
    if ($snap -and -not $ForcePull) {
        if (-not $hasExpectation) {
            Write-Log $log "existing image present; manifest digest unavailable, reporting as skipped-unverified: $($snap.Id)"
            Save-WorkerResult ([pscustomobject]@{
                instance = $instance; repo = $WorkerRepo; version = $WorkerVersion; image = $image
                status = 'skipped'; pull_seconds = 0; size_bytes = $snap.SizeBytes; image_id = $snap.Id
                repo_digests = $snap.RepoDigests; attempts = @(); error = 'manifest digest unavailable; image accepted as-is'
                started_at = $startedAt; finished_at = (Get-Date).ToString('o')
            })
            return
        }
        if (Test-DigestMatch $snap $WorkerIndexDigest $WorkerAmd64Digest) {
            Write-Log $log "already present and digest matches: $($snap.Id)"
            Save-WorkerResult ([pscustomobject]@{
                instance = $instance; repo = $WorkerRepo; version = $WorkerVersion; image = $image
                status = 'skipped'; pull_seconds = 0; size_bytes = $snap.SizeBytes; image_id = $snap.Id
                repo_digests = $snap.RepoDigests; attempts = @(); error = $null
                started_at = $startedAt; finished_at = (Get-Date).ToString('o')
            })
            return
        }
        Write-Log $log "existing image has unexpected digest: $($snap.Id); not overwriting"
        Save-WorkerResult ([pscustomobject]@{
            instance = $instance; repo = $WorkerRepo; version = $WorkerVersion; image = $image
            status = 'existing_mismatch'; pull_seconds = 0; size_bytes = $snap.SizeBytes; image_id = $snap.Id
            repo_digests = $snap.RepoDigests; attempts = @(); error = 'existing image digest does not match manifest'
            started_at = $startedAt; finished_at = (Get-Date).ToString('o')
        })
        return
    }

    $mirrors = @($WorkerMirrors -split ',' | Where-Object { $_ -ne '' })
    if ($mirrors.Count -eq 0) { $mirrors = @('docker.1panel.live') }
    $attempts = @()
    $pulled = $false
    $finalRef = $null
    $lastError = $null

    for ($attempt = 1; $attempt -le $MaxRetries; $attempt++) {
        $mirrorIndex = [Math]::Min([int][Math]::Floor(($attempt - 1) / 2), $mirrors.Count - 1)
        $mirrorName = $mirrors[$mirrorIndex]
        $imageName = $image
        if ($imageName -match ':[^/:]+$') { $imageName = $imageName.Substring(0, $imageName.LastIndexOf(':')) }
        $pullDigest = if ($WorkerIndexDigest) { $WorkerIndexDigest } elseif ($WorkerAmd64Digest) { $WorkerAmd64Digest } else { $null }
        if ($pullDigest) {
            $pullRef = if ($mirrorName -eq 'direct') { "$imageName@$pullDigest" } else { "$mirrorName/$imageName@$pullDigest" }
        } else {
            $pullRef = if ($mirrorName -eq 'direct') { $image } else { "$mirrorName/$image" }
        }
        $base = Join-Path $LogDir ("pull-{0}-attempt{1}" -f $safe, $attempt)
        $outFile = "$base.out.log"
        $errFile = "$base.err.log"
        Remove-Item -Force -ErrorAction SilentlyContinue $outFile, $errFile
        Write-Log $log "--- attempt $attempt/$MaxRetries mirror=$mirrorName ref=$pullRef ---"

        $attemptSw = [System.Diagnostics.Stopwatch]::StartNew()
        $timedOut = $false
        $exitCode = $null
        try {
            $proc = Start-Process -FilePath 'docker' -ArgumentList @('pull', '--platform', $Platform, $pullRef) `
                -NoNewWindow -PassThru -RedirectStandardOutput $outFile -RedirectStandardError $errFile
            $timeoutMs = [int]($AttemptTimeoutMin * 60 * 1000)
            $exited = $proc.WaitForExit($timeoutMs)
            if (-not $exited) {
                $timedOut = $true
                Write-Log $log "attempt timed out after $AttemptTimeoutMin min; killing docker pull pid $($proc.Id)"
                try { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue } catch { }
                Start-Sleep -Seconds 3
                $exitCode = -1
            } else {
                try { $exitCode = [int]$proc.ExitCode } catch { $exitCode = $null }
                if ($null -eq $exitCode) {
                    $postSnap = Get-ImageSnapshot $pullRef
                    $exitCode = if ($postSnap) { 0 } else { 1 }
                    Write-Log $log "exit code unavailable; image presence check -> $exitCode"
                }
            }
        } catch {
            $lastError = $_.Exception.Message
            $exitCode = -1
            Write-Log $log "attempt launch error: $lastError"
        }
        $attemptSw.Stop()
        $seconds = [math]::Round($attemptSw.Elapsed.TotalSeconds, 1)

        foreach ($f in @($outFile, $errFile)) {
            if (Test-Path $f) {
                $text = Get-Content -Path $f -Raw -ErrorAction SilentlyContinue
                if ($text) {
                    if ($text.Length -gt 6000) { $text = $text.Substring($text.Length - 6000) }
                    Add-Content -Path $log -Value $text -Encoding UTF8
                }
            }
        }

        $attempts += [pscustomobject]@{
            attempt = $attempt; mirror = $mirrorName; ref = $pullRef
            seconds = $seconds; exit_code = $exitCode; timed_out = $timedOut
        }

        $postPullSnap = Get-ImageSnapshot $pullRef
        if ($postPullSnap) {
            $pulled = $true
            $finalRef = $pullRef
            Write-Log $log "attempt $attempt succeeded in $seconds s (image present, exit=$exitCode)"
            break
        }

        $lastError = "attempt $attempt failed: mirror=$mirrorName exit=$exitCode timed_out=$timedOut image_absent"
        Write-Log $log $lastError
        if ($attempt -lt $MaxRetries) {
            $delay = [int]($RetryDelaySec * [Math]::Min($attempt, 5)) + (Get-Random -Minimum 0 -Maximum 10)
            Write-Log $log "backoff ${delay}s before next attempt"
            Start-Sleep -Seconds $delay
        }
    }
    $swTotal.Stop()

    if (-not $pulled) {
        Write-Log $log "FAILED after $($attempts.Count) attempts"
        Save-WorkerResult ([pscustomobject]@{
            instance = $instance; repo = $WorkerRepo; version = $WorkerVersion; image = $image
            status = 'failed'; pull_seconds = [math]::Round($swTotal.Elapsed.TotalSeconds, 1)
            size_bytes = $null; image_id = $null; repo_digests = $null
            attempts = $attempts; error = $lastError
            started_at = $startedAt; finished_at = (Get-Date).ToString('o')
        })
        return
    }

    if ($finalRef -ne $image) {
        Write-Log $log "tagging $finalRef -> $image"
        $prevEap = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        & docker tag $finalRef $image 2>&1 | ForEach-Object { Add-Content -Path $log -Value $_ -Encoding UTF8 }
        $ErrorActionPreference = $prevEap
    }
    $snap = Get-ImageSnapshot $image
    $digestOk = Test-DigestMatch $snap $WorkerIndexDigest $WorkerAmd64Digest
    if (-not $hasExpectation -and $snap) { $digestOk = $true }
    $status = if ($snap -and $digestOk) { 'pulled' } elseif ($snap) { 'verify_failed' } else { 'failed' }
    $err = if ($status -eq 'verify_failed') { 'pulled image digest does not match manifest' } else { $null }
    if (-not $hasExpectation) { $err = 'manifest digest unavailable; image accepted from registry' }
    if ($status -eq 'failed') { $err = 'pull reported success but image is not inspectable' }
    Write-Log $log "DONE status=$status size=$($snap.SizeBytes) id=$($snap.Id) digests=$($snap.RepoDigests) total=$([math]::Round($swTotal.Elapsed.TotalSeconds,1))s"

    Save-WorkerResult ([pscustomobject]@{
        instance = $instance; repo = $WorkerRepo; version = $WorkerVersion; image = $image
        status = $status; pull_ref = $finalRef
        pull_seconds = [math]::Round($swTotal.Elapsed.TotalSeconds, 1)
        size_bytes = if ($snap) { $snap.SizeBytes } else { $null }
        image_id = if ($snap) { $snap.Id } else { $null }
        repo_digests = if ($snap) { $snap.RepoDigests } else { $null }
        attempts = $attempts; error = $err
        started_at = $startedAt; finished_at = (Get-Date).ToString('o')
    })
}

# Internal worker mode must run before any orchestrator code.
if ($Worker) {
    Invoke-PullWorker
    exit 0
}

# -------------------------------------------------------------- inventory

$mainLog = Join-Path $LogDir 'pull-lite.log'
$progressLog = Join-Path $LogDir 'progress.log'
Write-Log $mainLog '=== pull_lite_all.ps1 start ==='
Write-Log $mainLog ("Dataset: " + $Dataset)
Write-Log $mainLog ("Mirrors: " + ($Mirror -join ', '))
Write-Log $mainLog ("MaxParallel=$MaxParallel MaxRetries=$MaxRetries AttemptTimeoutMin=$AttemptTimeoutMin Platform=$Platform")
Write-Log $mainLog ("Disk guards: free >= $(Format-Bytes $MinFreeBytes), docker usage <= $(Format-Bytes $MaxDockerBytes), estimate factor=$DockerEstimateFactor")

$dockerVersion = & docker version --format '{{.Server.Version}}' 2>&1
if ($LASTEXITCODE -ne 0) { Write-Log $mainLog "FATAL: docker unavailable: $dockerVersion"; exit 2 }
Write-Log $mainLog "Docker server version: $dockerVersion"

$inventoryPath = Join-Path $LogDir 'lite-instances.json'
if ($InstancesJson) {
    $inventoryPath = $InstancesJson
    if (-not [System.IO.Path]::IsPathRooted($inventoryPath)) { $inventoryPath = Join-Path $RepoRoot $inventoryPath }
} else {
    $datasetPath = $Dataset
    if (-not [System.IO.Path]::IsPathRooted($datasetPath)) { $datasetPath = Join-Path $RepoRoot $datasetPath }
    if (-not (Test-Path $datasetPath)) { Write-Log $mainLog "FATAL: dataset not found: $datasetPath"; exit 2 }

    $pyCandidates = @()
    if ($PythonExe) { $pyCandidates += $PythonExe }
    if ($env:PIR_PYTHON) { $pyCandidates += $env:PIR_PYTHON }
    $pyCandidates += 'C:\wp\empty\tools\python312\python.exe'
    $pyCandidates += 'python'
    $pyCandidates += 'python3'
    $python = $null
    foreach ($cand in $pyCandidates) {
        try {
            if ($cand -match '[\\/]' -and -not (Test-Path $cand)) { continue }
            $prevEap = $ErrorActionPreference
            $ErrorActionPreference = 'Continue'
            $check = & $cand -c 'import pyarrow' 2>&1
            $checkCode = $LASTEXITCODE
            $ErrorActionPreference = $prevEap
            if ($checkCode -eq 0) { $python = $cand; break }
        } catch { }
    }
    if (-not $python) {
        if (Test-Path $inventoryPath) {
            Write-Log $mainLog "WARN: no python+pyarrow found; reusing existing inventory $inventoryPath"
        } else {
            Write-Log $mainLog 'FATAL: no python+pyarrow found and no cached lite-instances.json'
            exit 2
        }
    } else {
        Write-Log $mainLog "inventory: $datasetPath -> $inventoryPath (python=$python)"
        $helper = Join-Path $PSScriptRoot 'lite_instances.py'
        $argsList = @($helper, '--dataset', $datasetPath, '--out', $inventoryPath)
        if ($Repo.Count -gt 0) { foreach ($r in $Repo) { $argsList += @('--repo', $r) } }
        if ($Instance.Count -gt 0) { foreach ($i in $Instance) { $argsList += @('--instance', $i) } }
        $prevEap = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        $invOut = & $python @argsList 2>&1
        $invCode = $LASTEXITCODE
        $ErrorActionPreference = $prevEap
        if ($invCode -ne 0) { Write-Log $mainLog "FATAL: inventory helper failed: $($invOut -join ' ')"; exit 2 }
        Write-Log $mainLog (($invOut -join ' ').Trim())
    }
}

$rawInventory = Get-Content -Path $inventoryPath -Raw | ConvertFrom-Json
$inventory = @()
foreach ($invItem in $rawInventory) { $inventory += $invItem }
if ($Repo.Count -gt 0) { $inventory = @($inventory | Where-Object { $Repo -contains $_.repo }) }
if ($Instance.Count -gt 0) { $inventory = @($inventory | Where-Object { $Instance -contains $_.instance_id }) }
if ($Limit -gt 0 -and $inventory.Count -gt $Limit) { $inventory = @($inventory[0..($Limit - 1)]) }
if ($inventory.Count -eq 0) { Write-Log $mainLog 'FATAL: inventory is empty'; exit 2 }
Write-Log $mainLog "inventory: $($inventory.Count) instances from $(@($inventory | Group-Object repo).Count) repos"

# Repo order: already-cached psf/requests first (its base layers exist), then
# repos by descending instance count, then any leftover alphabetically.
$preferredRepos = @(
    'psf/requests',
    'django/django',
    'sympy/sympy',
    'scikit-learn/scikit-learn',
    'matplotlib/matplotlib',
    'pytest-dev/pytest',
    'sphinx-doc/sphinx',
    'pylint-dev/pylint',
    'astropy/astropy',
    'pydata/xarray',
    'mwaskom/seaborn',
    'pallets/flask'
)
$repoGroupsAll = @($inventory | Group-Object repo)
$repoOrder = @()
foreach ($r in $preferredRepos) {
    if ($repoGroupsAll.Name -contains $r) { $repoOrder += $r }
}
$leftover = $repoGroupsAll | Where-Object { $preferredRepos -notcontains $_.Name } | Sort-Object Name
foreach ($rg in $leftover) { $repoOrder += $rg.Name }

$orderedInventory = New-Object System.Collections.ArrayList
$repoIndex = 0
foreach ($repoName in $repoOrder) {
    $repoIndex++
    $repoItems = @($inventory | Where-Object { $_.repo -eq $repoName })
    $versionGroups = @($repoItems | Group-Object version | Sort-Object {
        try { [version]$_.Name } catch { [version]'0.0' }
    })
    foreach ($vg in $versionGroups) {
        $items = @($vg.Group | Sort-Object { try { [int](($_.instance_id -split '-')[-1]) } catch { 0 } })
        foreach ($it in $items) {
            $null = $orderedInventory.Add([pscustomobject]@{
                instance_id = $it.instance_id
                repo = $it.repo
                version = $it.version
                repo_order = $repoIndex
            })
        }
    }
}
$inventory = @($orderedInventory)

# ------------------------------------------------------------- state records

$records = @{}
$indexCounter = 0
foreach ($item in $inventory) {
    $indexCounter++
    $records[$item.instance_id] = [pscustomobject]@{
        index            = $indexCounter
        instance         = $item.instance_id
        repo             = $item.repo
        version          = $item.version
        image            = ConvertTo-ImageRef $item.instance_id
        status           = 'pending'
        index_digest     = $null
        amd64_digest     = $null
        compressed_bytes = $null
        size_bytes       = $null
        image_id         = $null
        repo_digests     = $null
        pull_seconds     = $null
        attempts_count   = 0
        last_error       = $null
        updated_at       = (Get-Date).ToString('o')
        source           = $null
    }
}

$summaryPath = Join-Path $LogDir 'summary.json'
$priorRecords = @{}
if (Test-Path $summaryPath) {
    try {
        $prior = Get-Content -Path $summaryPath -Raw | ConvertFrom-Json
        foreach ($p in $prior.instances) { if ($p.instance) { $priorRecords[[string]$p.instance] = $p } }
        Write-Log $mainLog "resume: loaded $($priorRecords.Count) prior records from summary.json"
    } catch { Write-Log $mainLog "resume: summary.json unreadable, starting fresh: $($_.Exception.Message)" }
}

# --------------------------------------------------------------- disk baseline

$freeBefore = Get-CFreeBytes
$usageBefore = Get-DockerUsage
$vhdxBefore = Get-DockerVhdxBytes
$diskBaseline = [pscustomobject]@{
    recorded_at             = (Get-Date).ToString('o')
    c_free_bytes            = $freeBefore
    c_free_gib              = if ($freeBefore) { [math]::Round($freeBefore / 1GB, 2) } else { $null }
    docker_total_bytes      = $usageBefore.TotalBytes
    docker_images_bytes     = $usageBefore.ImagesBytes
    docker_rows             = $usageBefore.Rows
    docker_vhdx_path        = $vhdxBefore.Path
    docker_vhdx_bytes       = $vhdxBefore.Bytes
    min_free_bytes          = $MinFreeBytes
    max_docker_bytes        = $MaxDockerBytes
    estimate_factor         = $DockerEstimateFactor
}
Write-JsonAtomic -Path (Join-Path $LogDir 'disk-baseline.json') -Object $diskBaseline -Depth 6
Write-Log $mainLog ("disk baseline: C: free=$($diskBaseline.c_free_gib) GiB; docker total=$(Format-BytesDecimal $usageBefore.TotalBytes) images=$(Format-BytesDecimal $usageBefore.ImagesBytes); vhdx=$(Format-BytesDecimal $vhdxBefore.Bytes)")

if ($freeBefore -ne $null -and $freeBefore -lt $MinFreeBytes) {
    Write-Log $mainLog "STOP: C: free $(Format-Bytes $freeBefore) is below threshold $(Format-Bytes $MinFreeBytes); nothing pulled"
    exit 3
}
if ($usageBefore.TotalBytes -ne $null -and $usageBefore.TotalBytes -gt $MaxDockerBytes) {
    Write-Log $mainLog "STOP: Docker usage $(Format-BytesDecimal $usageBefore.TotalBytes) exceeds $(Format-BytesDecimal $MaxDockerBytes); nothing pulled"
    exit 3
}

# ------------------------------------------------------------- manifest phase

$manifestPath = Join-Path $LogDir 'manifest-cache.json'
$manifestCache = @{}
if ((Test-Path $manifestPath) -and -not $ForceManifest) {
    try {
        $cached = Get-Content -Path $manifestPath -Raw | ConvertFrom-Json
        foreach ($m in $cached) { if ($m.instance) { $manifestCache[[string]$m.instance] = $m } }
        Write-Log $mainLog "manifest cache: loaded $($manifestCache.Count) records"
    } catch { Write-Log $mainLog "manifest cache unreadable, rebuilding: $($_.Exception.Message)" }
}

# Fast path: parallel Python scanner (requests + threads). Falls back to the
# sequential PowerShell path below when Python/pyarrow is unavailable.
$pythonScanUsed = $false
if (-not $NoPythonManifestScan -and $python) {
    $scanner = Join-Path $PSScriptRoot 'scan_lite_manifests.py'
    if (Test-Path $scanner) {
        Write-Log $mainLog "manifest scan: python parallel scanner (workers=4)"
        # Manifest metadata is small: docker.1ms.run answers reliably and fast
        # (~3 s with token flow) while 1panel's HEAD/GET stalls intermittently.
        # Pulls keep the user-specified order (1panel first) below.
        $scanMirrorOrder = @($Mirror | Where-Object { $_ -ne 'direct' })
        [array]::Reverse($scanMirrorOrder)
        $scanArgs = @($scanner, '--inventory', $inventoryPath, '--out', $manifestPath,
            '--mirrors', ($scanMirrorOrder -join ','), '--workers', '4', '--timeout', '30', '--attempts', '2')
        if ($ForceManifest) { $scanArgs += '--force' }
        $prevEap = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        & $python @scanArgs 2>&1 | ForEach-Object {
            $line = [string]$_
            Write-Host $line
            Add-Content -Path $mainLog -Value $line -Encoding UTF8
        }
        $scanCode = $LASTEXITCODE
        $ErrorActionPreference = $prevEap
        if ($scanCode -ne 0) { Write-Log $mainLog "WARN: python manifest scanner exited $scanCode" }
        $manifestCache = @{}
        if (Test-Path $manifestPath) {
            try {
                $cached = Get-Content -Path $manifestPath -Raw | ConvertFrom-Json
                foreach ($m in $cached) { if ($m.instance) { $manifestCache[[string]$m.instance] = $m } }
            } catch { Write-Log $mainLog "WARN: cannot reload manifest cache: $($_.Exception.Message)" }
        }
        $pythonScanUsed = $true
        Write-Log $mainLog "manifest scan: loaded $($manifestCache.Count) records after python scan"
    }
}

$scanTotal = $inventory.Count
$scanDone = 0
$scanOk = 0
$scanFail = 0
$scanStart = Get-Date
foreach ($item in $inventory) {
    $scanDone++
    $rec = $records[$item.instance_id]
    $mr = $manifestCache[$item.instance_id]
    if ($pythonScanUsed) {
        # The parallel scanner already retried every missing record; do not
        # spend another 5-10 minutes per failure in the sequential fallback.
        $needQuery = $ForceManifest -or (-not $mr)
    } else {
        $needQuery = $ForceManifest -or (-not $mr) -or (-not $mr.index_digest) -or (-not $mr.layers) -or ($mr.layers.Count -eq 0)
    }
    if ($needQuery) {
        try {
            $info = Get-ManifestWithRetry -InstanceId $item.instance_id -Image $rec.image -LogPath $mainLog
            $mirrorsOk = @($info.Mirror)
            $mirrorsFailed = @()
            $digestConsistent = $true
            if ($CrossCheckManifests) {
                foreach ($m in ($Mirror | Where-Object { $_ -ne $info.Mirror -and $_ -ne 'direct' })) {
                    try {
                        $other = Get-ImageManifestInfo -MirrorName $m -Image $rec.image
                        $mirrorsOk += $m
                        if ($other.IndexDigest -ne $info.IndexDigest -or $other.Amd64Digest -ne $info.Amd64Digest) { $digestConsistent = $false }
                    } catch { $mirrorsFailed += "$m`: $($_.Exception.Message)" }
                }
            }
            $mr = [pscustomobject]@{
                instance         = $item.instance_id
                repo             = $item.repo
                version          = $item.version
                image            = $rec.image
                index_digest     = $info.IndexDigest
                amd64_digest     = $info.Amd64Digest
                compressed_bytes = $info.CompressedBytes
                layer_count      = $info.LayerCount
                layers           = $info.Layers
                mirrors_ok       = ($mirrorsOk -join ';')
                mirrors_failed   = ($mirrorsFailed -join '; ')
                digest_consistent = $digestConsistent
                fetched_at       = (Get-Date).ToString('o')
            }
            $manifestCache[$item.instance_id] = $mr
            $scanOk++
            Write-Log $mainLog "manifest $scanDone/$scanTotal OK $($item.instance_id) index=$($info.IndexDigest.Substring(0,16))... layers=$($info.LayerCount) compressed=$(Format-Bytes $info.CompressedBytes)"
            $safe = $item.instance_id -replace '[^A-Za-z0-9.-]', '_'
            try {
                if ($info.IndexJson) { Write-Utf8NoBom -Path (Join-Path $LogDir "manifests\$safe-index.json") -Text $info.IndexJson }
                if ($info.Amd64Json) { Write-Utf8NoBom -Path (Join-Path $LogDir "manifests\$safe-amd64.json") -Text $info.Amd64Json }
            } catch {
                Write-Log $mainLog "WARN could not write manifest files for $($item.instance_id): $($_.Exception.Message)"
            }
        } catch {
            $scanFail++
            $mr = [pscustomobject]@{
                instance = $item.instance_id; repo = $item.repo; version = $item.version; image = $rec.image
                index_digest = $null; amd64_digest = $null; compressed_bytes = $null; layer_count = 0; layers = @()
                mirrors_ok = ''; mirrors_failed = $_.Exception.Message; digest_consistent = $false
                fetched_at = (Get-Date).ToString('o')
            }
            $manifestCache[$item.instance_id] = $mr
            Write-Log $mainLog "manifest $scanDone/$scanTotal FAILED $($item.instance_id): $($_.Exception.Message)"
        }
        if ($scanDone % 10 -eq 0) { Write-JsonAtomic -Path $manifestPath -Object @($manifestCache.Values | Sort-Object index) -Depth 12 }
    } else {
        $scanOk++
        if ($scanDone % 25 -eq 0) { Write-Log $mainLog "manifest $scanDone/$scanTotal cached..." }
    }

    if ($mr) {
        $rec.index_digest = $mr.index_digest
        $rec.amd64_digest = $mr.amd64_digest
        $rec.compressed_bytes = $mr.compressed_bytes
    }
}
Write-JsonAtomic -Path $manifestPath -Object @($manifestCache.Values | Sort-Object index) -Depth 12
$scanSeconds = ((Get-Date) - $scanStart).TotalSeconds
Write-Log $mainLog "manifest phase done: $scanOk cached/ok, $scanFail failed, $([math]::Round($scanSeconds,1)) s"

# ------------------------------------------------------------- local scan

$localScanStart = Get-Date
foreach ($item in $inventory) {
    $rec = $records[$item.instance_id]
    $mr = $manifestCache[$item.instance_id]
    $found = Find-LocalImage -Image $rec.image -IndexDigest $rec.index_digest -Amd64Digest $rec.amd64_digest
    if ($found) {
        if ($rec.index_digest -and $found.Match) {
            $prior = $priorRecords[$item.instance_id]
            if ($prior -and $prior.status -eq 'pulled' -and $prior.pull_seconds) {
                $rec.status = 'pulled'
                $rec.pull_seconds = [double]$prior.pull_seconds
                $rec.attempts_count = [int]$prior.attempts_count
                $rec.source = 'prior-run'
            } else {
                $rec.status = 'skipped'
                $rec.pull_seconds = 0
                $rec.source = 'already-present'
            }
            $rec.size_bytes = $found.Snapshot.SizeBytes
            $rec.image_id = $found.Snapshot.Id
            $rec.repo_digests = $found.Snapshot.RepoDigests
            if ($found.Ref -ne $rec.image) {
                $prevEap = $ErrorActionPreference
                $ErrorActionPreference = 'Continue'
                & docker tag $found.Ref $rec.image 2>$null | Out-Null
                $ErrorActionPreference = $prevEap
                $canon = Get-ImageSnapshot $rec.image
                if ($canon) { $rec.repo_digests = $canon.RepoDigests }
                Write-Log $mainLog "tagged existing $($found.Ref) -> $($rec.image)"
            }
        } elseif (-not $rec.index_digest) {
            $rec.status = 'existing_unverified'
            $rec.size_bytes = $found.Snapshot.SizeBytes
            $rec.image_id = $found.Snapshot.Id
            $rec.repo_digests = $found.Snapshot.RepoDigests
            if ($rec.repo_digests -match '@(sha256:[0-9a-fA-F]{64})') { $rec.index_digest = $matches[1] }
            $rec.last_error = 'image present but manifest unavailable; digest taken from RepoDigests'
        } else {
            $rec.status = 'existing_mismatch'
            $rec.size_bytes = $found.Snapshot.SizeBytes
            $rec.image_id = $found.Snapshot.Id
            $rec.repo_digests = $found.Snapshot.RepoDigests
            $rec.last_error = "existing image digest $($found.Snapshot.Id) does not match manifest"
        }
        if ($rec.status -ne 'pulled') { $rec.updated_at = (Get-Date).ToString('o') }
    } else {
        $rec.status = 'pending'
    }
}
Write-Log $mainLog ("local scan done in {0:N1}s: skipped={1} pulled(prior)={2} mismatch={3} pending={4}" -f `
    ((Get-Date) - $localScanStart).TotalSeconds,
    @($records.Values | Where-Object { $_.status -eq 'skipped' }).Count,
    @($records.Values | Where-Object { $_.status -eq 'pulled' }).Count,
    @($records.Values | Where-Object { $_.status -in @('existing_mismatch', 'existing_unverified') }).Count,
    @($records.Values | Where-Object { $_.status -eq 'pending' }).Count)

# ------------------------------------------------------------- estimates

function Get-UniqueLayerBytes {
    param($Records)
    $seen = @{}
    foreach ($r in $Records) {
        $mr = $manifestCache[$r.instance]
        if (-not $mr -or -not $mr.layers) { continue }
        foreach ($l in @($mr.layers)) {
            if ($l.digest) { $seen[[string]$l.digest] = [int64]$l.size }
        }
    }
    $sum = [int64]0
    foreach ($v in $seen.Values) { $sum += [int64]$v }
    return [pscustomobject]@{ Bytes = $sum; Layers = $seen.Count }
}

$allUnique = Get-UniqueLayerBytes -Records @($records.Values)
$presentRecords = @($records.Values | Where-Object { $_.status -in @('skipped', 'pulled', 'existing_unverified') })
$remainingRecords = @($records.Values | Where-Object { $_.status -in @('pending', 'failed', 'existing_mismatch') })
$presentUnique = Get-UniqueLayerBytes -Records $presentRecords
$remainingUnique = Get-UniqueLayerBytes -Records $remainingRecords
$estimatedTotalDocker = if ($allUnique.Bytes) { [int64]($allUnique.Bytes * $DockerEstimateFactor) } else { $null }
$estimatedRemainingDocker = if ($remainingUnique.Bytes) { [int64]($remainingUnique.Bytes * $DockerEstimateFactor) } else { $null }
Write-Log $mainLog ("disk estimate: unique compressed all=$(Format-Bytes $allUnique.Bytes) ($($allUnique.Layers) layers); remaining=$(Format-Bytes $remainingUnique.Bytes); estimated docker total=$(Format-BytesDecimal $estimatedTotalDocker), remaining increment=$(Format-BytesDecimal $estimatedRemainingDocker)")

# ------------------------------------------------------------- summary helpers

function Get-Counts {
    param($Recs)
    [pscustomobject]@{
        total    = @($Recs).Count
        pulled   = @($Recs | Where-Object { $_.status -eq 'pulled' }).Count
        skipped  = @($Recs | Where-Object { $_.status -eq 'skipped' }).Count
        failed   = @($Recs | Where-Object { $_.status -eq 'failed' }).Count
        pending  = @($Recs | Where-Object { $_.status -eq 'pending' }).Count
        mismatch = @($Recs | Where-Object { $_.status -in @('existing_mismatch', 'existing_unverified', 'verify_failed') }).Count
    }
}

function Get-EtaInfo {
    param($Recs, [datetime]$RunStart)
    $counts = Get-Counts $Recs
    $done = $counts.pulled + $counts.skipped
    $remaining = $counts.total - $done - $counts.failed - $counts.mismatch
    $elapsed = ((Get-Date) - $RunStart).TotalSeconds
    $runPulled = @($Recs | Where-Object { $_.status -eq 'pulled' -and $_.source -ne 'prior-run' -and $_.pull_seconds -ne $null })
    $pulledSeconds = ($runPulled | Measure-Object -Property pull_seconds -Sum).Sum
    $pulledCompressed = ($runPulled | Where-Object { $_.compressed_bytes } | Measure-Object -Property compressed_bytes -Sum).Sum
    $ratePerHour = $null
    $etaHours = $null
    $effectiveMBps = $null
    if ($elapsed -gt 0 -and $runPulled.Count -gt 0) {
        $ratePerHour = [math]::Round($runPulled.Count / ($elapsed / 3600.0), 2)
        if ($ratePerHour -gt 0) { $etaHours = [math]::Round($remaining / $ratePerHour, 1) }
    }
    if ($pulledSeconds -gt 0 -and $pulledCompressed) {
        $effectiveMBps = [math]::Round(($pulledCompressed / 1MB) / $pulledSeconds, 3)
    }
    return [pscustomobject]@{
        remaining_instances = $remaining
        run_pulled_count = $runPulled.Count
        run_elapsed_seconds = [math]::Round($elapsed, 1)
        images_per_hour = $ratePerHour
        eta_hours = $etaHours
        effective_compressed_MBps = $effectiveMBps
    }
}

function Write-Summary {
    param([string]$Phase)
    $all = @($records.Values | Sort-Object index)
    $counts = Get-Counts $all
    $usage = Get-DockerUsage
    $free = Get-CFreeBytes
    $vhdx = Get-DockerVhdxBytes
    $eta = Get-EtaInfo -Recs $all -RunStart $script:RunStart
    $summary = [ordered]@{
        generated_at = (Get-Date).ToString('o')
        phase = $Phase
        status = $script:RunStatus
        dataset = $Dataset
        instance_count = $all.Count
        platform = $Platform
        mirrors = $Mirror
        max_parallel = $MaxParallel
        disk = [ordered]@{
            c_free_before_bytes = $freeBefore
            c_free_after_bytes = $free
            c_free_current_gib = if ($free) { [math]::Round($free / 1GB, 2) } else { $null }
            docker_total_before_bytes = $usageBefore.TotalBytes
            docker_total_bytes = $usage.TotalBytes
            docker_images_bytes = $usage.ImagesBytes
            docker_vhdx_bytes = $vhdx.Bytes
            min_free_bytes = $MinFreeBytes
            max_docker_bytes = $MaxDockerBytes
        }
        estimate = [ordered]@{
            unique_compressed_bytes_total = $allUnique.Bytes
            unique_compressed_bytes_remaining = $remainingUnique.Bytes
            docker_estimate_factor = $DockerEstimateFactor
            estimated_docker_total_bytes = $estimatedTotalDocker
            estimated_remaining_increment_bytes = $estimatedRemainingDocker
        }
        progress = $counts
        eta = $eta
        stop_reason = $script:StopReason
        instances = $all
    }
    Write-JsonAtomic -Path $summaryPath -Object $summary -Depth 12
    $all | Select-Object index, instance, repo, version, image, status, index_digest, amd64_digest,
        compressed_bytes, size_bytes, pull_seconds, attempts_count, source, last_error, updated_at |
        Export-Csv -Path (Join-Path $LogDir 'summary.csv') -NoTypeInformation -Encoding UTF8
    return $summary
}

function Write-PendingFiles {
    param($Recs)
    $pending = @($Recs | Where-Object { $_.status -in @('pending', 'failed') } | Sort-Object index)
    $lines = @()
    $currentRepo = $null
    foreach ($p in $pending) {
        if ($p.repo -ne $currentRepo) {
            if ($currentRepo) { $lines += '' }
            $lines += "[$($p.repo)]"
            $currentRepo = $p.repo
        }
        $lines += ("{0}  {1}" -f $p.instance, $p.image)
    }
    if ($pending.Count -eq 0) { $lines = @('(none)') }
    Write-Utf8NoBom -Path (Join-Path $LogDir 'pending.txt') -Text (($lines -join "`r`n") + "`r`n")
    $pending | Select-Object index, instance, repo, version, image, status, index_digest, amd64_digest, compressed_bytes, last_error |
        Export-Csv -Path (Join-Path $LogDir 'pending.csv') -NoTypeInformation -Encoding UTF8
    return $pending.Count
}

function Write-Report {
    param([string]$Phase)
    $all = @($records.Values | Sort-Object index)
    $counts = Get-Counts $all
    $eta = Get-EtaInfo -Recs $all -RunStart $script:RunStart
    $free = Get-CFreeBytes
    $usage = Get-DockerUsage
    $vhdx = Get-DockerVhdxBytes
    $lines = @()
    $lines += '# Lane Docker Lite — SWE-bench Lite 300 镜像全量拉取'
    $lines += ''
    $lines += ('生成时间：' + (Get-Date).ToString('yyyy-MM-dd HH:mm:ss') + '（Asia/Shanghai）')
    $lines += ('状态：**' + $script:RunStatus + '**' + $(if ($script:StopReason) { ' — ' + $script:StopReason } else { '' }))
    $lines += ''
    $lines += '## TL;DR'
    $lines += ''
    $lines += '| 项 | 结果 |'
    $lines += '|---|---|'
    $lines += "| 数据集 | SWE-bench Lite test.parquet（300 实例 / 12 repo） |"
    $lines += "| 实例总数 | $($counts.total) |"
    $lines += "| 已拉取 (pulled) | $($counts.pulled) |"
    $lines += "| 已存在跳过 (skipped) | $($counts.skipped) |"
    $lines += "| 失败 (failed) | $($counts.failed) |"
    $lines += "| 待拉 (pending) | $($counts.pending) |"
    $lines += "| digest 不匹配/未验证 | $($counts.mismatch) |"
    $lines += "| 唯一压缩层合计 | $(Format-Bytes $allUnique.Bytes)（$($allUnique.Layers) 层） |"
    $lines += "| 剩余唯一压缩层 | $(Format-Bytes $remainingUnique.Bytes) |"
    $lines += "| 估算 Docker 总占用（×$DockerEstimateFactor） | $(Format-BytesDecimal $estimatedTotalDocker) |"
    $lines += "| 估算剩余增量 | $(Format-BytesDecimal $estimatedRemainingDocker) |"
    $lines += "| C: 可用（开始时 → 现在） | $(Format-Bytes $freeBefore) → $(Format-Bytes $free) |"
    $lines += "| Docker 总占用（开始时 → 现在） | $(Format-BytesDecimal $usageBefore.TotalBytes) → $(Format-BytesDecimal $usage.TotalBytes) |"
    $lines += "| 实测吞吐（压缩字节/耗时，上界） | $(if ($eta.effective_compressed_MBps) { "$($eta.effective_compressed_MBps) MB/s" } else { '尚无完成样本' }) |"
    $lines += "| 完成速率 | $(if ($eta.images_per_hour) { "$($eta.images_per_hour) images/hour" } else { '尚无完成样本' }) |"
    $lines += "| ETA（按本机实测） | $(if ($eta.eta_hours -ne $null) { "$($eta.eta_hours) 小时" } else { '待实测' }) |"
    $lines += ''
    $lines += '## 分 repo 进度'
    $lines += ''
    $lines += '| repo | 总数 | pulled | skipped | failed | pending | 压缩层合计 | 唯一压缩层 | 估算 Docker 增量 |'
    $lines += '|---|---:|---:|---:|---:|---:|---:|---:|---:|'
    foreach ($repoName in $repoOrder) {
        $recs = @($all | Where-Object { $_.repo -eq $repoName })
        if ($recs.Count -eq 0) { continue }
        $c = Get-Counts $recs
        $sumCompressed = ($recs | Where-Object { $_.compressed_bytes } | Measure-Object -Property compressed_bytes -Sum).Sum
        $u = Get-UniqueLayerBytes -Records $recs
        $lines += "| $repoName | $($c.total) | $($c.pulled) | $($c.skipped) | $($c.failed) | $($c.pending) | $(Format-Bytes $sumCompressed) | $(Format-Bytes $u.Bytes) | $(Format-BytesDecimal ([int64]($u.Bytes * $DockerEstimateFactor))) |"
    }
    $lines += ''
    $lines += '## 待拉清单'
    $lines += ''
    $pending = @($all | Where-Object { $_.status -in @('pending', 'failed') } | Sort-Object index)
    if ($pending.Count -eq 0) {
        $lines += '（无）'
    } else {
        foreach ($repoName in $repoOrder) {
            $rp = @($pending | Where-Object { $_.repo -eq $repoName })
            if ($rp.Count -eq 0) { continue }
            $lines += "### $repoName（$($rp.Count)）"
            $lines += ''
            $lines += (($rp | ForEach-Object { '- `' + $_.instance + '`' }) -join "`r`n")
            $lines += ''
        }
    }
    $lines += '## 磁盘水位与保护'
    $lines += ''
    $lines += "* 开始：C: 可用 $(Format-Bytes $freeBefore)；Docker 总占用 $(Format-BytesDecimal $usageBefore.TotalBytes)（images $(Format-BytesDecimal $usageBefore.ImagesBytes)）；docker_data.vhdx $(Format-BytesDecimal $vhdxBefore.Bytes)"
    $lines += "* 现在：C: 可用 $(Format-Bytes $free)；Docker 总占用 $(Format-BytesDecimal $usage.TotalBytes)（images $(Format-BytesDecimal $usage.ImagesBytes)）；docker_data.vhdx $(Format-BytesDecimal $vhdx.Bytes)"
    $lines += "* 停止阈值：C: 可用 < $(Format-Bytes $MinFreeBytes) 或 Docker 占用 > $(Format-BytesDecimal $MaxDockerBytes)"
    $lines += '* 本脚本 **不执行 prune，不删除任何镜像/容器/卷**。'
    $lines += ''
    $lines += '## 方法与保证'
    $lines += ''
    $lines += '* 镜像名：`swebench/sweb.eval.x86_64.<instance_id __→_1776_>:latest`（Docker Hub 官方 namespace，经 mirror 传输）。'
    $lines += '* 顺序：psf/requests（已有基础层）→ django → sympy → scikit-learn → matplotlib → pytest → sphinx → pylint → astropy → xarray → seaborn → flask；repo 内按 version 分组，再按 issue 号排序，同版本共享层只下一次。'
    $lines += '* 每个镜像记录 index digest、linux/amd64 manifest digest、压缩层大小、拉取耗时、ImageId/RepoDigests 校验结果。'
    $lines += '* 已存在镜像仅在 digest 与 manifest 一致时跳过；不一致记为 `existing_mismatch` 且不覆盖、不删除。'
    $lines += '* 断点续拉：manifest-cache.json + summary.json + `docker pull` 层缓存；重启脚本会复用。'
    $lines += ''
    $lines += '## 复现命令'
    $lines += ''
    $lines += '```powershell'
    $lines += 'powershell -NoProfile -ExecutionPolicy Bypass -File scripts\docker\pull_lite_all.ps1 `'
    $lines += '  -MaxParallel 2 -MaxRetries 6 -AttemptTimeoutMin 60 -RetryDelaySec 20'
    $lines += '```'
    $lines += ''
    $lines += '* 仅盘点（不拉取）：加 `-SkipPull`。'
    $lines += '* 强制重查 manifest：`-ForceManifest`。'
    $lines += '* 只跑某个 repo：`-Repo django/django`。'
    $lines += ''
    $lines += '## 数据文件'
    $lines += ''
    $lines += '| 文件 | 说明 |'
    $lines += '|---|---|'
    $lines += '| `summary.json` / `summary.csv` | 全部实例状态、digest、大小、耗时、磁盘水位 |'
    $lines += '| `manifest-cache.json` | 300 个实例的 manifest digest 与层列表 |'
    $lines += '| `pending.txt` / `pending.csv` | 尚待拉取清单 |'
    $lines += '| `lite-instances.json` | 从 parquet 提取的实例清单 |'
    $lines += '| `disk-baseline.json` / `disk-final.json` | 磁盘基线/最终水位 |'
    $lines += '| `progress.log` | 每个 repo 完成时的进度行 |'
    $lines += '| `pull-lite.log` / `pull-<instance>.log` | 编排日志与逐实例日志 |'
    $lines += ''
    $lines += '## 备注'
    $lines += ''
    $lines += '* 本轮只做拉取，不运行 SWE-bench 评测；评测由 foreman 使用 `scripts/swebench/run_eval.ps1` 驱动。'
    $lines += '* Docker Hub 直连在本机不可用（DNS 污染/443 拒绝），镜像经 `docker.1panel.live` 主力、`docker.1ms.run` 兜底；Docker 按 digest 内容寻址校验每一层。'
    $lines += '* 估算因子 4.0 来自上一轮实测：8 个 requests 镜像唯一压缩层 2.69 GB ↔ `docker system df` 10.7 GB。'
    $lines += ''
    Write-Utf8NoBom -Path (Join-Path $LogDir 'REPORT.md') -Text (($lines -join "`r`n") + "`r`n")
}

# ------------------------------------------------------------- pull phase

$script:RunStart = Get-Date
$script:RunStatus = if ($SkipPull) { 'scan-only' } else { 'running' }
$script:StopReason = $null
$runPulledCount = 0
$runPulledSeconds = 0.0
$groupStats = @{}

Write-Summary -Phase 'scan-complete' | Out-Null
Write-PendingFiles -Recs @($records.Values) | Out-Null
Write-Report -Phase 'scan-complete'

if ($SkipPull) {
    Write-Log $mainLog 'SkipPull set; stopping after inventory/manifest/local scan.'
    $script:RunStatus = 'scan-only'
    Write-Summary -Phase 'scan-only' | Out-Null
    Write-Report -Phase 'scan-only'
    exit 0
}

$workerScript = $PSCommandPath
$psExe = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'

foreach ($repoName in $repoOrder) {
    if ($script:StopReason) { break }
    $repoRecs = @($records.Values | Where-Object { $_.repo -eq $repoName } | Sort-Object index)
    if ($repoRecs.Count -eq 0) { continue }
    $groupStart = Get-Date
    $queue = New-Object System.Collections.Queue
    foreach ($r in $repoRecs) {
        if ($r.status -eq 'pending') { $queue.Enqueue($r) }
    }
    $groupPendingAtStart = $queue.Count
    Write-Log $mainLog "=== repo group $repoName start: $($repoRecs.Count) instances, $groupPendingAtStart to pull ==="
    Write-Log $progressLog ("[{0:yyyy-MM-dd HH:mm:ss}] START {1} total={2} pending={3}" -f (Get-Date), $repoName, $repoRecs.Count, $groupPendingAtStart)

    $running = @{}
    $lastGuardCheck = Get-Date
    while ($queue.Count -gt 0 -or $running.Count -gt 0) {
        while (-not $script:StopReason -and $running.Count -lt $MaxParallel -and $queue.Count -gt 0) {
            $guard = Get-DiskGuardState
            if ($guard.Reason) {
                $script:StopReason = $guard.Reason
                Write-Log $mainLog "DISK GUARD STOP: $($guard.Reason)"
                break
            }
            $item = $queue.Dequeue()
            $safe = $item.instance -replace '[^A-Za-z0-9.-]', '_'
            $wlog = Join-Path $LogDir "pull-$safe.log"
            Remove-Item -Force -ErrorAction SilentlyContinue (Join-Path $LogDir "worker-$safe.json")
            $argString = '-NoProfile -ExecutionPolicy Bypass -File "' + $workerScript + '" -Worker' +
                ' -WorkerInstance "' + $item.instance + '"' +
                ' -WorkerRepo "' + $item.repo + '"' +
                ' -WorkerVersion "' + $item.version + '"' +
                ' -WorkerImage "' + $item.image + '"' +
                ' -WorkerIndexDigest "' + $item.index_digest + '"' +
                ' -WorkerAmd64Digest "' + $item.amd64_digest + '"' +
                ' -WorkerMirrors "' + ($Mirror -join ',') + '"' +
                ' -WorkerLog "' + $wlog + '"' +
                ' -LogDir "' + $LogDir + '"' +
                " -Platform `"$Platform`" -MaxRetries $MaxRetries -RetryDelaySec $RetryDelaySec -AttemptTimeoutMin $AttemptTimeoutMin" +
                $(if ($ForcePull) { ' -ForcePull' } else { '' })
            Write-Log $mainLog "start worker $($item.instance) mirrors=$($Mirror -join ',')"
            $proc = Start-Process -FilePath $psExe -ArgumentList $argString -PassThru -WindowStyle Hidden
            $running[$item.instance] = $proc
        }

        if ($running.Count -eq 0) { break }
        Start-Sleep -Seconds 10

        if (((Get-Date) - $lastGuardCheck).TotalSeconds -ge 60) {
            $lastGuardCheck = Get-Date
            $guard = Get-DiskGuardState
            if ($guard.Reason -and -not $script:StopReason) {
                $script:StopReason = $guard.Reason
                Write-Log $mainLog "DISK GUARD STOP (while draining): $($guard.Reason)"
            }
        }

        foreach ($inst in @($running.Keys)) {
            $proc = $running[$inst]
            $proc.Refresh()
            if (-not $proc.HasExited) { continue }
            $safe = $inst -replace '[^A-Za-z0-9.-]', '_'
            $rp = Join-Path $LogDir "worker-$safe.json"
            if (Test-Path $rp) {
                try { $res = Get-Content $rp -Raw | ConvertFrom-Json } catch { $res = $null }
            } else { $res = $null }
            $rec = $records[$inst]
            if ($res) {
                $rec.status = [string]$res.status
                if ($res.pull_seconds -ne $null) { $rec.pull_seconds = [double]$res.pull_seconds }
                if ($res.size_bytes -ne $null) { $rec.size_bytes = [int64]$res.size_bytes }
                if ($res.image_id) { $rec.image_id = [string]$res.image_id }
                if ($res.repo_digests) { $rec.repo_digests = [string]$res.repo_digests }
                if (-not $rec.index_digest -and $rec.repo_digests -match '@(sha256:[0-9a-fA-F]{64})') { $rec.index_digest = $matches[1] }
                if ($res.attempts) { $rec.attempts_count = @($res.attempts).Count }
                if ($res.error) { $rec.last_error = [string]$res.error }
                $rec.updated_at = (Get-Date).ToString('o')
                if ($rec.status -eq 'pulled') { $runPulledCount++; $runPulledSeconds += [double]$res.pull_seconds }
            } else {
                $rec.status = 'failed'
                $rec.last_error = "worker exited $($proc.ExitCode) without result file"
                $rec.updated_at = (Get-Date).ToString('o')
            }
            $running.Remove($inst)
            $doneNow = @($records.Values | Where-Object { $_.status -in @('pulled', 'skipped') }).Count
            Write-Log $mainLog "done $inst status=$($rec.status) overall=$doneNow/$($records.Count) c_free=$(Format-Bytes (Get-CFreeBytes))"
            Write-Summary -Phase "pulling:$repoName" | Out-Null
        }
    }

    $elapsed = ((Get-Date) - $groupStart).TotalSeconds
    $c = Get-Counts $repoRecs
    $usageNow = Get-DockerUsage
    $freeNow = Get-CFreeBytes
    $eta = Get-EtaInfo -Recs @($records.Values) -RunStart $script:RunStart
    $progressLine = "DONE ${repoName}: total=$($c.total) pulled=$($c.pulled) skipped=$($c.skipped) failed=$($c.failed) pending=$($c.pending) elapsed=$([math]::Round($elapsed/60,1))min free=$(Format-Bytes $freeNow) docker=$(Format-BytesDecimal $usageNow.TotalBytes) overall_done=$((Get-Counts @($records.Values)).pulled + (Get-Counts @($records.Values)).skipped)/$($records.Count) eta=$($eta.eta_hours)h"
    Write-Log $mainLog "[progress] $progressLine"
    Write-Log $progressLog ("[{0:yyyy-MM-dd HH:mm:ss}] {1}" -f (Get-Date), $progressLine)
    $groupStats[$repoName] = [pscustomobject]@{ elapsed_seconds = [math]::Round($elapsed, 1); pulled = $c.pulled; skipped = $c.skipped; failed = $c.failed; pending = $c.pending }
    Write-Summary -Phase "group-done:$repoName" | Out-Null
    Write-PendingFiles -Recs @($records.Values) | Out-Null
    Write-Report -Phase "group-done:$repoName"
}

# ------------------------------------------------------------- finalize

$finalStatus = 'completed'
if ($script:StopReason) { $finalStatus = 'stopped-disk-guard' }
$counts = Get-Counts @($records.Values)
if ($counts.failed -gt 0 -and -not $script:StopReason) { $finalStatus = 'completed-with-failures' }
$script:RunStatus = $finalStatus

$freeAfter = Get-CFreeBytes
$usageAfter = Get-DockerUsage
$vhdxAfter = Get-DockerVhdxBytes
Write-JsonAtomic -Path (Join-Path $LogDir 'disk-final.json') -Object ([pscustomobject]@{
    recorded_at = (Get-Date).ToString('o')
    c_free_bytes = $freeAfter
    c_free_gib = if ($freeAfter) { [math]::Round($freeAfter / 1GB, 2) } else { $null }
    docker_total_bytes = $usageAfter.TotalBytes
    docker_images_bytes = $usageAfter.ImagesBytes
    docker_rows = $usageAfter.Rows
    docker_vhdx_path = $vhdxAfter.Path
    docker_vhdx_bytes = $vhdxAfter.Bytes
}) -Depth 6

Write-Summary -Phase 'final' | Out-Null
$pendingCount = Write-PendingFiles -Recs @($records.Values)
Write-Report -Phase 'final'
Write-Log $mainLog "FINAL status=$finalStatus pulled=$($counts.pulled) skipped=$($counts.skipped) failed=$($counts.failed) pending=$($counts.pending) mismatch=$($counts.mismatch) c_free=$(Format-Bytes $freeAfter) docker=$(Format-BytesDecimal $usageAfter.TotalBytes)"
Write-Log $mainLog '=== pull_lite_all.ps1 done ==='

if ($script:StopReason) { exit 3 }
if ($counts.failed -gt 0 -or $counts.mismatch -gt 0) { exit 1 }
exit 0
