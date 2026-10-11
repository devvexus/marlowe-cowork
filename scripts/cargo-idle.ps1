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
$lock = New-Object System.Threading.Mutex($false, 'Global\MarloweCoworkCargo')
$waited = $false
try { $null = $lock.WaitOne() } catch [System.Threading.AbandonedMutexException] { }
try {
    while ($true) {
        $free = (Get-CimInstance Win32_OperatingSystem).FreeVirtualMemory / 1MB
        if ($free -ge $minFreeGb) { break }
        if (-not $waited) {
            Write-Host ("[cargo-idle] waiting: {0:N1} GB of commit free, need {1} GB (Matthew's training has priority); this is expected, keep waiting rather than bypassing" -f $free, $minFreeGb)
            $waited = $true
        }
        Start-Sleep -Seconds 20
    }
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
