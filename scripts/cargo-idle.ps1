(Get-Process -Id $PID).PriorityClass = 'Idle'
# Machine guard (2026-10-10): Matthew's training jobs share this machine's RAM, and a -j 4 workspace
# test build once took ~20 GB of commit and crashed them. So every cargo run here is one at a time
# machine-wide, starts only with commit headroom to spare, and sizes its jobs and test threads to
# the RAM that is free at that moment (2026-10-11). Any -j/--jobs argument is ignored. Set MARLOWE_CARGO_UNGUARDED=1 to bypass when the machine is free.
$pass = @(); $skip = $false; $afterDashes = $false
foreach ($a in $args) {
    if ($afterDashes) { $pass += $a; continue }
    if ($a -eq '--') { $afterDashes = $true; $pass += $a; continue }
    if ($skip) { $skip = $false; continue }
    if ($a -eq '-j' -or $a -eq '--jobs') { $skip = $true; continue }
    if ($a -match '^(-j\d+|--jobs=\d+)$') { continue }
    $pass += $a
}
if ($env:MARLOWE_CARGO_UNGUARDED -eq '1') { & cargo @args; exit $LASTEXITCODE }
# A hold file pauses every build and test outright (Matthew's training needs the whole machine).
$hold = 'C:\Users\matth\Projects\MarloweCowork\target\cargo-hold'
if (Test-Path $hold) {
    Write-Host "[cargo-idle] HOLD: all builds and tests are paused until the training session says go ($(Get-Content $hold -Raw)). Do not run cargo another way; stop and report."
    exit 75
}
$minFreeGb = 12

# Disk cap (2026-10-14): cargo never deletes stale artifacts; target\ reached 300 GB and one
# worktree's build folders 590 GB. At most every 30 minutes per build folder the guard measures the
# folder this build writes to (and the main one); over the cap it drops the incremental caches and
# every artifact hash but the newest two per crate in each deps folder, and as a last resort the
# debug profile folder itself. Worktree build folders (MarloweCowork-*\target*) not built in two
# days are deleted outright. Cargo rebuilds anything missing, so this only ever costs build time.
$main = 'C:\Users\matth\Projects\MarloweCowork\target'
$capGb = if ($env:MARLOWE_TARGET_CAP_GB) { [double]$env:MARLOWE_TARGET_CAP_GB } else { 60 }
function Get-BuildTarget {
    for ($i = 0; $i -lt $pass.Count; $i++) {
        if ($pass[$i] -eq '--target-dir' -and $i + 1 -lt $pass.Count) { return [IO.Path]::GetFullPath($pass[$i + 1]) }
        if ($pass[$i] -match '^--target-dir=(.+)$') { return [IO.Path]::GetFullPath($Matches[1]) }
    }
    if ($env:CARGO_TARGET_DIR) { return [IO.Path]::GetFullPath($env:CARGO_TARGET_DIR) }
    $d = (Get-Location).Path
    while ($d) {
        if (Test-Path (Join-Path $d 'Cargo.lock')) { return Join-Path $d 'target' }
        $d = Split-Path $d -Parent
    }
    return $main
}
function Remove-StaleWorktreeTargets {
    foreach ($w in Get-ChildItem 'C:\Users\matth\Projects' -Directory -Filter 'MarloweCowork-*' -ErrorAction SilentlyContinue) {
        foreach ($t in Get-ChildItem $w.FullName -Directory -Filter 'target*' -ErrorAction SilentlyContinue) {
            if (-not (Test-Path (Join-Path $t.FullName 'CACHEDIR.TAG'))) { continue }
            $last = (@($t) + @(Get-ChildItem $t.FullName -Directory -ErrorAction SilentlyContinue) + @(Get-ChildItem $t.FullName -Directory -Recurse -Depth 1 -Filter '.fingerprint' -Force -ErrorAction SilentlyContinue) | Measure-Object LastWriteTime -Maximum).Maximum
            if (((Get-Date) - $last).TotalDays -gt 2) {
                Write-Host ("[cargo-idle] removing {0}: not built since {1:yyyy-MM-dd}" -f $t.FullName, $last)
                cmd /c "rmdir /s /q `"$($t.FullName)`""
            }
        }
    }
}
function Get-TreeGb([string]$path) {
    if (-not (Test-Path $path)) { return 0 }
    $line = robocopy $path NUL /L /S /NJH /BYTES /NFL /NDL /NC /R:0 /W:0 /XJ | Where-Object { $_ -match '^\s*Bytes\s*:' } | Select-Object -First 1
    if (-not $line) { return 0 }
    return [double](($line -replace '^\s*Bytes\s*:\s*', '').Trim().Split(' ')[0]) / 1GB
}
function Limit-Target([string]$target) {
    if (-not (Test-Path $target)) { return }
    $stamp = Join-Path $target 'cargo-size-check.txt'
    if ((Test-Path $stamp) -and ((Get-Date) - (Get-Item $stamp).LastWriteTime).TotalMinutes -lt 30) { return }
    $gb = Get-TreeGb $target
    try { Set-Content -Path $stamp -Value ("{0} {1:N1} GB (cap {2} GB)" -f (Get-Date -Format 's'), $gb, $capGb) -ErrorAction Stop } catch { }
    if ($gb -le $capGb) { return }
    Write-Host ("[cargo-idle] target is {0:N1} GB, over its {1} GB cap: pruning stale artifacts" -f $gb, $capGb)
    $profiles = Get-ChildItem $target -Recurse -Depth 2 -Directory -Filter deps -ErrorAction SilentlyContinue | ForEach-Object { $_.Parent.FullName }
    foreach ($p in $profiles) {
        $inc = Join-Path $p 'incremental'
        if (Test-Path $inc) { cmd /c "rmdir /s /q `"$inc`"" }
        $files = Get-ChildItem (Join-Path $p 'deps') -File -ErrorAction SilentlyContinue | Where-Object { $_.Name -match '^(?<stem>.+)-(?<hash>[0-9a-f]{16})(\..*)?$' } | ForEach-Object {
            [pscustomobject]@{ File = $_; Stem = $Matches.stem; Hash = $Matches.hash }
        }
        foreach ($crate in ($files | Group-Object Stem)) {
            $keep = $crate.Group | Group-Object Hash | Sort-Object { ($_.Group.File | Measure-Object LastWriteTime -Maximum).Maximum } -Descending | Select-Object -First 2 | ForEach-Object { $_.Name }
            $crate.Group | Where-Object { $keep -notcontains $_.Hash } | ForEach-Object { Remove-Item -LiteralPath $_.File.FullName -Force -ErrorAction SilentlyContinue }
        }
    }
    $after = Get-TreeGb $target
    if ($after -gt $capGb -and (Test-Path (Join-Path $target 'debug'))) {
        Write-Host ("[cargo-idle] still {0:N1} GB after pruning: removing the debug profile folder (the next build is a full one)" -f $after)
        $dbg = Join-Path $target 'debug'
        if (Test-Path $dbg) { cmd /c "rmdir /s /q `"$dbg`"" }
        $after = Get-TreeGb $target
    }
    Write-Host ("[cargo-idle] target pruned: {0:N1} GB -> {1:N1} GB" -f $gb, $after)
    try { Set-Content -Path $stamp -Value ("{0} pruned {1:N1} -> {2:N1} GB (cap {3} GB)" -f (Get-Date -Format 's'), $gb, $after, $capGb) -ErrorAction Stop } catch { }
}

$lock = New-Object System.Threading.Mutex($false, 'Global\MarloweCoworkCargo')
$waited = $false
try { $null = $lock.WaitOne() } catch [System.Threading.AbandonedMutexException] { }
try {
    while ($true) {
        $os = Get-CimInstance Win32_OperatingSystem
        $free = $os.FreeVirtualMemory / 1MB
        $ram = $os.FreePhysicalMemory / 1MB
        # A quiet file (another session's CPU benchmarks need an idle machine) makes builds wait
        # rather than refuse: the build starts by itself once the file is gone.
        $quiet = Test-Path 'C:\Users\matth\Projects\MarloweCowork\target\cargo-quiet'
        # Enough commit, or (the page file can grow again now that the disk has room) plenty of free RAM.
        if (-not $quiet -and ($free -ge $minFreeGb -or ($ram -ge 8 -and $free -ge 4))) { break }
        if ($quiet -and -not $waited) {
            Write-Host "[cargo-idle] waiting: the machine is reserved for a quiet benchmark window (target\cargo-quiet); this is expected, keep waiting"
            $waited = $true
        }
        if (-not $waited) {
            # A marker the lead watches so Matthew gets a push when builds are stuck waiting on his machine.
            try { Set-Content -Path 'C:\Users\matth\Projects\MarloweCowork\target\cargo-waiting.txt' -Value ("{0} free commit {1:N1} GB, free RAM {2:N1} GB" -f (Get-Date -Format 'HH:mm'), $free, $ram) -ErrorAction Stop } catch { }
            Write-Host ("[cargo-idle] waiting: {0:N1} GB of commit free, need {1} GB (Matthew's training has priority); this is expected, keep waiting rather than bypassing" -f $free, $minFreeGb)
            $waited = $true
        }
        Start-Sleep -Seconds 20
    }
    if ($waited) { Remove-Item 'C:\Users\matth\Projects\MarloweCowork\target\cargo-waiting.txt' -ErrorAction SilentlyContinue }
    # Under the build lock, so no guarded build is writing while artifacts are pruned.
    $buildTarget = Get-BuildTarget
    $mainStamp = Join-Path $main 'cargo-size-check.txt'
    if (-not (Test-Path $mainStamp) -or ((Get-Date) - (Get-Item $mainStamp).LastWriteTime).TotalMinutes -ge 30) { Remove-StaleWorktreeTargets }
    Limit-Target $main
    if ($buildTarget -ne $main) { Limit-Target $buildTarget }
    # Jobs scale with free RAM (about 3 GB per rustc/link at peak), 1 to 6; one build at a time still.
    $ramGb = (Get-CimInstance Win32_OperatingSystem).FreePhysicalMemory / 1MB
    $jobs = [Math]::Max(1, [Math]::Min(6, [Math]::Floor($ramGb / 3)))
    $env:CARGO_BUILD_JOBS = "$jobs"
    $env:RUST_TEST_THREADS = "$jobs"
    & cargo @pass
    $code = $LASTEXITCODE
} finally {
    $lock.ReleaseMutex()
}
exit $code
