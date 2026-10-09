# Runs cargo at IDLE priority so builds never compete with foreground work (or with another job
# that owns the machine). Windows passes an IDLE priority class from a parent to every child it
# creates, so lowering this process first makes cargo and every rustc it spawns run idle, while
# output still flows through the normal pipes.
#
#   powershell -NoProfile -File scripts/cargo-idle.ps1 check --workspace --all-targets -j 2
param([Parameter(ValueFromRemainingArguments = $true)][string[]]$CargoArgs)
(Get-Process -Id $PID).PriorityClass = 'Idle'
& cargo @CargoArgs
exit $LASTEXITCODE
