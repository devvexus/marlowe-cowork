(Get-Process -Id $PID).PriorityClass = 'Idle'
# Machine guard (2026-10-10): Matthew's training jobs share this machine's RAM, and a -j 4 workspace
# test build once took ~20 GB of commit and crashed them. So every cargo run here is one at a time
# machine-wide, single-job, single test thread, and starts only with commit headroom to spare.
# Any -j/--jobs argument is ignored. Set MARLOWE_CARGO_UNGUARDED=1 to bypass when the machine is free.
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
$env:CARGO_BUILD_JOBS = '1'
$env:RUST_TEST_THREADS = '1'
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
    & cargo @pass
    $code = $LASTEXITCODE
} finally {
    $lock.ReleaseMutex()
}
exit $code
