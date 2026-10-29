#requires -Version 5.1
<#
.SYNOPSIS
  Pull all SWE-bench Verified psf/requests evaluation images from Docker Hub
  mirrors, with resume-friendly retries, mirror rotation, parallelism and a
  machine-readable summary.

.DESCRIPTION
  Official image naming (SWE-bench v3.0.17 harness, test_spec.py):
    swebench/sweb.eval.<arch>.<instance_id with "__" -> "_1776_">:<tag>
  e.g. psf__requests-1142 -> swebench/sweb.eval.x86_64.psf_1776_requests-1142:latest

  Docker Hub is unreachable directly from this host (DNS pollution / blocked
  443). The script therefore prepends a registry mirror host at pull time.
  Mirrors are transports only: Docker verifies every layer against its digest,
  and the manifest digests returned by the mirrors are recorded and compared.

  Pulls are resumable: `docker pull` reuses already-downloaded layers, so a
  timed-out or failed attempt can continue where it stopped, even through a
  different mirror (layers are shared in the content store by digest).

  The orchestrator collects OCI index / linux-amd64 manifest digests and
  compressed layer sizes for every instance, skips images already present,
  runs up to MaxParallel worker processes, rotates mirrors + exponential
  backoff on failure, and writes summary.json / summary.csv plus per-instance
  pull logs under -LogDir.

.PARAMETER Instance
  Instance ids (psf__requests-1142) or full image references. Default: all 8
  SWE-bench Verified psf/requests instances.

.PARAMETER Mirror
  Registry mirrors in fallback order. Default: docker.1panel.live, docker.1ms.run.
  Use "direct" as an entry to try Docker Hub directly.

.PARAMETER MaxParallel
  Number of concurrent `docker pull` workers (1-4 recommended; each pulls up to
  3 layers concurrently, so 4 workers can open ~12 connections).

.PARAMETER MaxRetries
  Attempts per instance across the whole mirror rotation.

.PARAMETER RetryDelaySec
  Base backoff delay; attempt N waits RetryDelaySec * N seconds (+ jitter).

.PARAMETER AttemptTimeoutMin
  Kill and retry a single `docker pull` attempt after this many minutes.
  Downloaded layers are kept, so the next attempt resumes.

.PARAMETER Platform
  Pull platform. Default linux/amd64 (SWE-bench official images are x86_64).

.PARAMETER LogDir
  Directory for logs/summary, relative to the repo root unless absolute.

.PARAMETER ForcePull
  Pull even when the image already exists locally.

.PARAMETER SkipPull
  Only collect/compare manifests and report existing images (no pulls).

.PARAMETER ReuseManifests
  Reuse the existing manifest-compare.json instead of re-querying every mirror
  (useful when resuming a long batch).

.PARAMETER DirectFallback
  After exhausting the -Mirror list, try Docker Hub directly as an extra attempt.
#>
[CmdletBinding()]
param(
    [string[]]$Instance = @(
        'psf__requests-1142',
        'psf__requests-1724',
        'psf__requests-1766',
        'psf__requests-1921',
        'psf__requests-2317',
        'psf__requests-2931',
        'psf__requests-5414',
        'psf__requests-6028'
    ),
    [string[]]$Mirror = @('docker.1panel.live', 'docker.1ms.run'),
    [int]$MaxParallel = 3,
    [int]$MaxRetries = 6,
    [int]$RetryDelaySec = 20,
    [int]$AttemptTimeoutMin = 50,
    [string]$Platform = 'linux/amd64',
    [string]$LogDir = 'logs/lane-docker-all',
    [switch]$ForcePull,
    [switch]$SkipPull,
    [switch]$DirectFallback,
    [switch]$ReuseManifests,
    [Parameter(DontShow = $true)][switch]$Worker,
    [Parameter(DontShow = $true)][string]$WorkerInstance,
    [Parameter(DontShow = $true)][string]$WorkerMirrors,
    [Parameter(DontShow = $true)][string]$WorkerLog
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
if (-not [System.IO.Path]::IsPathRooted($LogDir)) { $LogDir = Join-Path $RepoRoot $LogDir }
New-Item -ItemType Directory -Force -Path $LogDir | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $LogDir 'manifests') | Out-Null

# ---------------------------------------------------------------- utilities

function Write-Utf8NoBom {
    param([string]$Path, [string]$Text)
    $enc = New-Object System.Text.UTF8Encoding($false)
    [System.IO.File]::WriteAllText($Path, $Text, $enc)
}

function ConvertTo-ImageRef {
    param([string]$Value)
    if ($Value -match '/' -or $Value -match ':') { return $Value }
    $norm = $Value -replace '__', '_1776_'
    return "swebench/sweb.eval.x86_64.${norm}:latest"
}

function Get-ImageSnapshot {
    param([string]$Ref)
    $prevEap = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $out = & docker image inspect $Ref 2>$null
        $code = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $prevEap
    }
    if ($code -ne 0 -or -not $out) { return $null }
    try {
        $obj = ($out -join "`n") | ConvertFrom-Json
        # docker image inspect returns an array; take the first entry.
        if ($obj -is [array]) { $obj = $obj[0] }
        return [pscustomobject]@{
            Ref         = $Ref
            Id          = $obj.Id
            SizeBytes   = [int64]$obj.Size
            Created     = $obj.Created
            RepoDigests = (@($obj.RepoDigests) -join ';')
        }
    } catch { return $null }
}

function Get-CFreeBytes {
    try {
        $d = Get-CimInstance Win32_LogicalDisk -Filter "DeviceID='C:'"
        return [int64]$d.FreeSpace
    } catch { return $null }
}

function Write-Log {
    param([string]$Path, [string]$Message)
    $line = '[{0:yyyy-MM-dd HH:mm:ss}] {1}' -f (Get-Date), $Message
    Write-Host $line
    Add-Content -Path $Path -Value $line -Encoding UTF8
}

# --------------------------------------------------------- registry manifests
# Manifest data is read through the Docker CLI (buildx/manifest commands), i.e.
# the same registry client path used by `docker pull`. Direct HTTP requests to
# some mirrors are intermittently rejected with 403 while Docker's requests
# succeed, so HTTP is deliberately not used here.

function Get-ImageManifestInfo {
    param([string]$Mirror, [string]$Image)
    $ref = if ($Mirror -eq 'direct') { $Image } else { "$Mirror/$Image" }

    $idxJson = & docker buildx imagetools inspect --format '{{json .Manifest}}' $ref 2>&1
    if ($LASTEXITCODE -ne 0 -or -not $idxJson) { throw "imagetools inspect failed: $($idxJson -join ' ')" }
    $idxText = ($idxJson -join "`n")
    $idx = $idxText | ConvertFrom-Json
    $indexDigest = [string]$idx.digest
    if (-not $indexDigest) { throw "index digest missing from buildx output" }

    $amdDigest = $null
    if ($idx.PSObject.Properties.Name -contains 'manifests') {
        $amd = $idx.manifests | Where-Object {
            $_.platform.os -eq 'linux' -and $_.platform.architecture -eq 'amd64'
        } | Select-Object -First 1
        if (-not $amd) { throw "no linux/amd64 manifest in index" }
        $amdDigest = [string]$amd.digest
    } else {
        $amdDigest = $indexDigest
    }

    $verboseJson = & docker manifest inspect -v $ref 2>&1
    $verboseText = ($verboseJson -join "`n")
    $layers = @()
    if ($LASTEXITCODE -eq 0 -and $verboseJson) {
        $verbose = $verboseText | ConvertFrom-Json
        if ($verbose -isnot [array]) { $verbose = @($verbose) }
        $amdEntry = $verbose | Where-Object {
            $_.Descriptor -and $_.Descriptor.platform -and
            $_.Descriptor.platform.os -eq 'linux' -and $_.Descriptor.platform.architecture -eq 'amd64'
        } | Select-Object -First 1
        if (-not $amdEntry) { $amdEntry = $verbose[0] }
        if ($amdEntry) {
            $layers = @($amdEntry.OCIManifest.layers)
            if ($amdEntry.Descriptor.digest) { $amdDigest = [string]$amdEntry.Descriptor.digest }
        }
    }

    $compressed = [int64]0
    foreach ($l in $layers) { $compressed += [int64]$l.size }

    return [pscustomobject]@{
        Mirror          = $Mirror
        IndexDigest     = $indexDigest
        Amd64Digest     = $amdDigest
        CompressedBytes = $compressed
        LayerCount      = $layers.Count
        IndexJson       = $idxText
        Amd64Json       = $verboseText
    }
}

# -------------------------------------------------------------------- worker

function Invoke-PullWorker {
    $log = $WorkerLog
    Write-Log $log "=== worker start instance=$WorkerInstance ==="
    $image = ConvertTo-ImageRef $WorkerInstance
    $mirrors = @($WorkerMirrors -split ',' | Where-Object { $_ -ne '' })
    if ($mirrors.Count -eq 0) { $mirrors = @('docker.1panel.live') }
    $resultPath = Join-Path $LogDir ("worker-{0}.json" -f ($WorkerInstance -replace '[^A-Za-z0-9.-]', '_'))

    function Save-WorkerResult($obj) {
        Write-Utf8NoBom -Path $resultPath -Text ($obj | ConvertTo-Json -Depth 10)
    }

    $snap = Get-ImageSnapshot $image
    if ($snap -and -not $ForcePull) {
        Write-Log $log "image already present: $($snap.Id) size=$($snap.SizeBytes)"
        Save-WorkerResult ([pscustomobject]@{
            instance = $WorkerInstance; image = $image; status = 'existing'
            pull_seconds = 0; size_bytes = $snap.SizeBytes; image_id = $snap.Id
            repo_digests = $snap.RepoDigests; attempts = @(); error = $null
        })
        return
    }

    $attempts = @()
    $pulled = $false
    $finalRef = $null
    $swTotal = [System.Diagnostics.Stopwatch]::StartNew()
    $lastError = $null

    for ($attempt = 1; $attempt -le $MaxRetries; $attempt++) {
        # Two attempts per mirror before rotating: the primary mirror rejects
        # requests intermittently (HTTP 403) and usually succeeds on retry.
        $mirrorIndex = [Math]::Min([int][Math]::Floor(($attempt - 1) / 2), $mirrors.Count - 1)
        $mirror = $mirrors[$mirrorIndex]
        if ($mirror -eq 'direct') { $pullRef = $image } else { $pullRef = "$mirror/$image" }
        $base = Join-Path $LogDir ("pull-{0}-attempt{1}" -f ($WorkerInstance -replace '[^A-Za-z0-9.-]', '_'), $attempt)
        $outFile = "$base.out.log"
        $errFile = "$base.err.log"
        Remove-Item -Force -ErrorAction SilentlyContinue $outFile, $errFile
        Write-Log $log "--- attempt $attempt/$MaxRetries mirror=$mirror ref=$pullRef ---"

        $attemptSw = [System.Diagnostics.Stopwatch]::StartNew()
        $timedOut = $false
        $exitCode = -1
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
            } else {
                # Start-Process with redirected streams does not populate
                # ExitCode; fall back to checking whether the tag now exists.
                $exitCode = $null
                try { $exitCode = $proc.ExitCode } catch { }
                if ($null -eq $exitCode) {
                    $postSnap = Get-ImageSnapshot $pullRef
                    $exitCode = if ($postSnap) { 0 } else { 1 }
                    Write-Log $log "exit code unavailable; image presence check -> $exitCode"
                }
            }
        } catch {
            $lastError = $_.Exception.Message
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
            attempt = $attempt; mirror = $mirror; ref = $pullRef
            seconds = $seconds; exit_code = $exitCode; timed_out = $timedOut
        }

        if ($exitCode -eq 0 -and -not $timedOut) {
            $pulled = $true
            $finalRef = $pullRef
            Write-Log $log "attempt $attempt succeeded in $seconds s"
            break
        }

        $lastError = "attempt $attempt failed: mirror=$mirror exit=$exitCode timed_out=$timedOut"
        Write-Log $log $lastError
        if ($attempt -lt $MaxRetries) {
            $delay = [int]($RetryDelaySec * [Math]::Min($attempt, 5)) + (Get-Random -Minimum 0 -Maximum 10)
            Write-Log $log "backoff ${delay}s before next attempt"
            Start-Sleep -Seconds $delay
        }
    }
    $swTotal.Stop()

    if (-not $pulled -and $DirectFallback -and ($mirrors -notcontains 'direct')) {
        Write-Log $log "all mirrors exhausted; final direct Docker Hub attempt"
        $pullRef = $image
        $base = Join-Path $LogDir ("pull-{0}-direct" -f ($WorkerInstance -replace '[^A-Za-z0-9.-]', '_'))
        $attemptSw = [System.Diagnostics.Stopwatch]::StartNew()
        $proc = Start-Process -FilePath 'docker' -ArgumentList @('pull', '--platform', $Platform, $pullRef) `
            -NoNewWindow -PassThru -RedirectStandardOutput "$base.out.log" -RedirectStandardError "$base.err.log"
        $exited = $proc.WaitForExit([int]($AttemptTimeoutMin * 60 * 1000))
        if (-not $exited) { try { Stop-Process -Id $proc.Id -Force } catch { } }
        $attemptSw.Stop()
        $exitCode = -1
        if ($exited) {
            $exitCode = $null
            try { $exitCode = $proc.ExitCode } catch { }
            if ($null -eq $exitCode) {
                $postSnap = Get-ImageSnapshot $pullRef
                $exitCode = if ($postSnap) { 0 } else { 1 }
            }
        }
        $attempts += [pscustomobject]@{
            attempt = $attempts.Count + 1; mirror = 'direct'; ref = $pullRef
            seconds = [math]::Round($attemptSw.Elapsed.TotalSeconds, 1)
            exit_code = $exitCode; timed_out = (-not $exited)
        }
        if ($exitCode -eq 0) { $pulled = $true; $finalRef = $pullRef }
    }

    if (-not $pulled) {
        Write-Log $log "FAILED after $($attempts.Count) attempts"
        Save-WorkerResult ([pscustomobject]@{
            instance = $WorkerInstance; image = $image; status = 'failed'
            pull_seconds = [math]::Round($swTotal.Elapsed.TotalSeconds, 1)
            size_bytes = $null; image_id = $null; repo_digests = $null
            attempts = $attempts; error = $lastError
        })
        return
    }

    # Canonical tag so later tooling can use the official reference.
    if ($finalRef -ne $image) {
        Write-Log $log "tagging $finalRef -> $image"
        & docker tag $finalRef $image 2>&1 | ForEach-Object { Add-Content -Path $log -Value $_ -Encoding UTF8 }
    }
    $snap = Get-ImageSnapshot $image
    $repoDigests = if ($snap) { $snap.RepoDigests } else { $null }
    $sizeBytes = if ($snap) { $snap.SizeBytes } else { $null }
    $imageId = if ($snap) { $snap.Id } else { $null }
    Write-Log $log "DONE size=$sizeBytes id=$imageId digests=$repoDigests total=$([math]::Round($swTotal.Elapsed.TotalSeconds,1))s"

    Save-WorkerResult ([pscustomobject]@{
        instance = $WorkerInstance; image = $image; status = 'pulled'
        pull_ref = $finalRef
        pull_seconds = [math]::Round($swTotal.Elapsed.TotalSeconds, 1)
        size_bytes = $sizeBytes; image_id = $imageId; repo_digests = $repoDigests
        attempts = $attempts; error = $null
    })
}

# -------------------------------------------------------------- orchestration

# Internal worker mode: pull exactly one image, write worker-<instance>.json.
# Must be dispatched before any orchestrator code runs.
if ($Worker) {
    Invoke-PullWorker
    exit 0
}

$mainLog = Join-Path $LogDir 'pull-all.log'
Write-Log $mainLog '=== pull_all_requests.ps1 start ==='
Write-Log $mainLog ("Instances: " + ($Instance -join ', '))
Write-Log $mainLog ("Mirrors: " + ($Mirror -join ', '))
Write-Log $mainLog ("MaxParallel=$MaxParallel MaxRetries=$MaxRetries AttemptTimeoutMin=$AttemptTimeoutMin Platform=$Platform")

$dockerVersion = & docker version --format '{{.Server.Version}}' 2>&1
if ($LASTEXITCODE -ne 0) { Write-Log $mainLog "FATAL: docker unavailable: $dockerVersion"; exit 2 }
Write-Log $mainLog "Docker server version: $dockerVersion"

Write-Log $mainLog '--- docker system df (before) ---'
& docker system df 2>&1 | ForEach-Object { Add-Content -Path $mainLog -Value $_ -Encoding UTF8 }
Write-Log $mainLog '--- docker images (before) ---'
& docker images --format 'table {{.Repository}}:{{.Tag}} {{.ID}} {{.Size}}' 2>&1 | ForEach-Object { Add-Content -Path $mainLog -Value $_ -Encoding UTF8 }
$freeBefore = Get-CFreeBytes
Write-Log $mainLog "C: free before: $freeBefore bytes ($([math]::Round($freeBefore/1GB,2)) GiB)"

# 1) manifests: index + amd64 digest + compressed size, from every mirror
$manifestCache = Join-Path $LogDir 'manifest-compare.json'
if ($ReuseManifests -and (Test-Path $manifestCache)) {
    $cachedRecords = Get-Content -Path $manifestCache -Raw | ConvertFrom-Json
    if ($cachedRecords -isnot [array]) { $cachedRecords = @($cachedRecords) }
    $manifestRecords = @($cachedRecords | Where-Object { $Instance -contains $_.instance })
    Write-Log $mainLog "manifest records reused from cache: $($manifestRecords.Count)"
} else {
    $manifestRecords = @()
    foreach ($inst in $Instance) {
        $image = ConvertTo-ImageRef $inst
        $key = $inst
        $ok = @()
        $errors = @()
        $savedIndex = $null
        foreach ($m in ($Mirror | Where-Object { $_ -ne 'direct' })) {
            try {
                Write-Log $mainLog "manifest: $inst @ $m"
                $info = Get-ImageManifestInfo -Mirror $m -Image $image
                $ok += [pscustomobject]@{ Mirror = $m; IndexDigest = $info.IndexDigest; Amd64Digest = $info.Amd64Digest; CompressedBytes = $info.CompressedBytes }
                $safe = $inst -replace '[^A-Za-z0-9.-]', '_'
                Write-Utf8NoBom -Path (Join-Path $LogDir "manifests\$safe-$m-index.json") -Text $info.IndexJson
                Write-Utf8NoBom -Path (Join-Path $LogDir "manifests\$safe-$m-amd64.json") -Text $info.Amd64Json
                if (-not $savedIndex) { $savedIndex = $info }
            } catch {
                $errors += [pscustomobject]@{ Mirror = $m; Error = $_.Exception.Message }
                Write-Log $mainLog "manifest FAILED $inst @ ${m}: $($_.Exception.Message)"
            }
        }
        $indexSet = @($ok | ForEach-Object { $_.IndexDigest } | Sort-Object -Unique)
        $amdSet = @($ok | ForEach-Object { $_.Amd64Digest } | Sort-Object -Unique)
        $consistent = ($indexSet.Count -le 1 -and $amdSet.Count -le 1)
        $manifestRecords += [pscustomobject]@{
            instance        = $inst
            image           = $image
            index_digest    = ($indexSet -join ';')
            amd64_digest    = ($amdSet -join ';')
            compressed_bytes = if ($savedIndex) { $savedIndex.CompressedBytes } else { $null }
            mirrors_ok      = ($ok | ForEach-Object { $_.Mirror }) -join ';'
            mirrors_failed  = ($errors | ForEach-Object { "$($_.Mirror): $($_.Error)" }) -join '; '
            digest_consistent = $consistent
        }
    }
}

$manifestRecords | ConvertTo-Json -Depth 8 | ForEach-Object { Write-Utf8NoBom -Path (Join-Path $LogDir 'manifest-compare.json') -Text $_ }
$manifestCsv = $manifestRecords | Select-Object instance, image, index_digest, amd64_digest, compressed_bytes,
    @{n = 'compressed_gib'; e = { if ($_.compressed_bytes) { [math]::Round($_.compressed_bytes / 1GB, 3) } else { $null } } },
    mirrors_ok, digest_consistent, mirrors_failed
$manifestCsv | Export-Csv -Path (Join-Path $LogDir 'manifest-compare.csv') -NoTypeInformation -Encoding UTF8
Write-Log $mainLog "manifest records: $($manifestRecords.Count); all consistent: $(@($manifestRecords | Where-Object { -not $_.digest_consistent }).Count -eq 0)"

# 2) existing images
$state = @()
foreach ($inst in $Instance) {
    $image = ConvertTo-ImageRef $inst
    $snap = Get-ImageSnapshot $image
    if (-not $snap) {
        $foundRef = $null
        foreach ($m in ($Mirror | Where-Object { $_ -ne 'direct' })) {
            $cand = "$m/$image"
            $snap = Get-ImageSnapshot $cand
            if ($snap) { $foundRef = $cand; break }
        }
        if ($snap -and $foundRef) {
            # Pulled earlier through a mirror: add the canonical tag too.
            $prevEap = $ErrorActionPreference
            $ErrorActionPreference = 'Continue'
            & docker tag $foundRef $image 2>$null | Out-Null
            $ErrorActionPreference = $prevEap
            $canonical = Get-ImageSnapshot $image
            if ($canonical) { $snap = $canonical }
            Write-Log $mainLog "tagged existing $foundRef -> $image"
        }
    }
    $state += [pscustomobject]@{
        instance = $inst; image = $image
        status = if ($snap) { 'existing' } else { 'pending' }
        size_bytes = if ($snap) { $snap.SizeBytes } else { $null }
        image_id = if ($snap) { $snap.Id } else { $null }
        repo_digests = if ($snap) { $snap.RepoDigests } else { $null }
    }
}
$existingCount = @($state | Where-Object { $_.status -eq 'existing' }).Count
Write-Log $mainLog "existing images: $existingCount / $($Instance.Count)"

if ($SkipPull) {
    $state | ConvertTo-Json -Depth 8 | ForEach-Object { Write-Utf8NoBom -Path (Join-Path $LogDir 'state.json') -Text $_ }
    Write-Log $mainLog 'SkipPull set; stopping after manifest/existing scan.'
    exit 0
}

# 3) parallel workers
$pending = @($state | Where-Object { $_.status -eq 'pending' })
Write-Log $mainLog "pending pulls: $($pending.Count)"

$results = @()
$running = @{}
$queue = New-Object System.Collections.Queue
foreach ($p in $pending) { $queue.Enqueue($p) }

$workerScript = $PSCommandPath
$psExe = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
$priority = 0
while ($queue.Count -gt 0 -or $running.Count -gt 0) {
    while ($running.Count -lt $MaxParallel -and $queue.Count -gt 0) {
        $item = $queue.Dequeue()
        $safe = $item.instance -replace '[^A-Za-z0-9.-]', '_'
        $wlog = Join-Path $LogDir "pull-$safe.log"
        Remove-Item -Force -ErrorAction SilentlyContinue (Join-Path $LogDir "worker-$safe.json")
        # Keep the caller's priority order: every worker starts with the fastest
        # mirror and only falls back to slower mirrors on failure.
        $order = @($Mirror)
        $priority++
        $argString = '-NoProfile -ExecutionPolicy Bypass -File "' + $workerScript + '" -Worker' +
            ' -WorkerInstance "' + $item.instance + '"' +
            ' -WorkerMirrors "' + ($order -join ',') + '"' +
            ' -WorkerLog "' + $wlog + '"' +
            ' -LogDir "' + $LogDir + '"' +
            " -Platform `"$Platform`" -MaxRetries $MaxRetries -RetryDelaySec $RetryDelaySec -AttemptTimeoutMin $AttemptTimeoutMin" +
            $(if ($ForcePull) { ' -ForcePull' } else { '' }) +
            $(if ($DirectFallback) { ' -DirectFallback' } else { '' })
        Write-Log $mainLog "start worker $($item.instance) mirrors=$($order -join ',')"
        $proc = Start-Process -FilePath $psExe -ArgumentList $argString -PassThru -WindowStyle Hidden
        $running[$item.instance] = $proc
    }

    Start-Sleep -Seconds 15
    foreach ($inst in @($running.Keys)) {
        $proc = $running[$inst]
        if ($proc.HasExited) {
            $safe = $inst -replace '[^A-Za-z0-9.-]', '_'
            $rp = Join-Path $LogDir "worker-$safe.json"
            if (Test-Path $rp) {
                $res = Get-Content $rp -Raw | ConvertFrom-Json
            } else {
                $res = [pscustomobject]@{ instance = $inst; status = 'failed'; error = "worker exited $($proc.ExitCode) without result"; attempts = @() }
            }
            $results += $res
            Write-Log $mainLog "worker finished $inst status=$($res.status) exit=$($proc.ExitCode)"
            $running.Remove($inst)
        }
    }
    if ($queue.Count -eq 0 -and $running.Count -gt 0) {
        Write-Log $mainLog "waiting: $($running.Keys -join ', ')"
    }
}

# 4) summary
$summaryRows = @()
foreach ($inst in $Instance) {
    $st = $state | Where-Object { $_.instance -eq $inst } | Select-Object -First 1
    $mr = $manifestRecords | Where-Object { $_.instance -eq $inst } | Select-Object -First 1
    $res = $results | Where-Object { $_.instance -eq $inst } | Select-Object -First 1
    $status = if ($st.status -eq 'existing') { 'existing' } elseif ($res) { $res.status } else { 'pending' }
    $summaryRows += [pscustomobject]@{
        instance         = $inst
        image            = $st.image
        status           = $status
        index_digest     = $mr.index_digest
        amd64_digest     = $mr.amd64_digest
        compressed_bytes = $mr.compressed_bytes
        compressed_gib   = if ($mr.compressed_bytes) { [math]::Round($mr.compressed_bytes / 1GB, 3) } else { $null }
        pull_seconds     = if ($res) { $res.pull_seconds } elseif ($st.status -eq 'existing') { 0 } else { $null }
        size_bytes       = if ($res -and $res.size_bytes) { $res.size_bytes } elseif ($st.size_bytes) { $st.size_bytes } else { $null }
        image_id         = if ($res -and $res.image_id) { $res.image_id } else { $st.image_id }
        repo_digests     = if ($res -and $res.repo_digests) { $res.repo_digests } else { $st.repo_digests }
        attempts         = if ($res) { $res.attempts } else { @() }
        error            = if ($res) { $res.error } else { $null }
        digest_consistent = $mr.digest_consistent
    }
}

$freeAfter = Get-CFreeBytes
$summary = [pscustomobject]@{
    generated_at   = (Get-Date).ToString('o')
    platform       = $Platform
    mirrors        = $Mirror
    max_parallel   = $MaxParallel
    instances      = $summaryRows
    ok_count       = @($summaryRows | Where-Object { $_.status -in @('pulled', 'existing') }).Count
    failed_count   = @($summaryRows | Where-Object { $_.status -eq 'failed' }).Count
    pulled_count   = @($summaryRows | Where-Object { $_.status -eq 'pulled' }).Count
    existing_count = @($summaryRows | Where-Object { $_.status -eq 'existing' }).Count
    total_compressed_bytes = ($summaryRows | Where-Object { $_.compressed_bytes } | Measure-Object -Property compressed_bytes -Sum).Sum
    total_image_bytes = ($summaryRows | Where-Object { $_.size_bytes } | Measure-Object -Property size_bytes -Sum).Sum
    c_free_before_bytes = $freeBefore
    c_free_after_bytes  = $freeAfter
}
$summary | ConvertTo-Json -Depth 12 | ForEach-Object { Write-Utf8NoBom -Path (Join-Path $LogDir 'summary.json') -Text $_ }
$summaryRows | Select-Object instance, image, status, index_digest, amd64_digest, compressed_bytes, compressed_gib,
    pull_seconds, size_bytes, image_id, digest_consistent, error |
    Export-Csv -Path (Join-Path $LogDir 'summary.csv') -NoTypeInformation -Encoding UTF8

Write-Log $mainLog '--- docker system df (after) ---'
& docker system df 2>&1 | ForEach-Object { Add-Content -Path $mainLog -Value $_ -Encoding UTF8 }
Write-Log $mainLog "C: free after: $freeAfter bytes ($([math]::Round($freeAfter/1GB,2)) GiB)"
Write-Log $mainLog "SUMMARY ok=$($summary.ok_count) pulled=$($summary.pulled_count) existing=$($summary.existing_count) failed=$($summary.failed_count) total_image=$([math]::Round($summary.total_image_bytes/1GB,2)) GiB"
Write-Log $mainLog '=== pull_all_requests.ps1 done ==='

if ($summary.failed_count -gt 0) { exit 1 } else { exit 0 }
