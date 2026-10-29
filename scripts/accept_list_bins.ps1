$ErrorActionPreference = 'Continue'
Set-Location 'C:\Users\XPENG_USER\Desktop\github\pir'
$lines = Get-Content 'test_bins.json'
$rows = @()
foreach ($l in $lines) {
    try { $o = $l | ConvertFrom-Json } catch { continue }
    if ($null -ne $o -and $o.executable -and $o.profile.test) { $rows += $o }
}
$rows = $rows | Sort-Object executable -Unique
$out = @()
foreach ($r in $rows) {
    $n = [IO.Path]::GetFileName($r.executable) -replace '-[0-9a-f]{16}\.exe$', ''
    $out += "$($r.executable)|$n"
}
$out | Set-Content 'test_bins.txt' -Encoding ascii
Write-Host "count=$($out.Count)"
