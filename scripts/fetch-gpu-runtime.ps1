# Fetches the pinned GPU runtime that a `--features cuda` build loads at run time, and verifies
# every archive against the sha256 pinned here before extracting anything. Nothing is installed
# system-wide: files land in -Dest (default <repo>\gpu-runtime, git-ignored), which the memory
# engine finds through an explicit --gpu-runtime flag (eval), <home>\gpu-runtime (pass -Dest),
# $MARLOWE_GPU_RUNTIME, or the dev tree.
#
#   x64   CUDA 13 runtime, cuBLAS, cuFFT and cuDNN 9 from NVIDIA's redistributable wheels on PyPI,
#         for the pyke ORT 1.28 build's CUDA EP (onnxruntime_providers_cuda.dll is copied next to
#         the executable by cargo). Needs an NVIDIA driver with CUDA 13 support (UMD >= 13.4).
#   arm64 The official onnxruntime-win-arm64 1.31.0 build plus Microsoft's CUDA plugin EP v0.3.0
#         (no prebuilt ORT CUDA build exists for aarch64-pc-windows-msvc). The plugin needs the
#         CUDA 13.4.1 runtime and driver 616.41+; cuDNN 9.26+ is optional. -WithCudaRuntime also
#         fetches the arm64 CUDA runtime wheels; otherwise the system CUDA install (%CUDA_PATH%)
#         is used.
#
#   powershell -NoProfile -File scripts\fetch-gpu-runtime.ps1                  # this machine's arch
#   powershell -NoProfile -File scripts\fetch-gpu-runtime.ps1 -Arch arm64 -WithCudaRuntime
param(
    [ValidateSet("auto", "x64", "arm64")] [string]$Arch = "auto",
    [string]$Dest = (Join-Path $PSScriptRoot "..\gpu-runtime"),
    [switch]$WithCudaRuntime,
    [switch]$Force
)
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"
Add-Type -AssemblyName System.IO.Compression.FileSystem

if ($Arch -eq "auto") {
    $Arch = if ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") { "arm64" } else { "x64" }
}

$pypi = "https://files.pythonhosted.org/packages"
$gh = "https://github.com/microsoft/onnxruntime/releases/download"

# Kind "wheel": every nvidia/**/bin/**/*.dll is extracted flat into Sub.
# Kind "zip":   every *.dll under Prefix is extracted flat into Sub.
$x64 = @(
    @{ Name = "nvidia_cuda_runtime-13.4.92-py3-none-win_amd64.whl"; Kind = "wheel"; Sub = "cuda"
       Sha = "08dca5e4aba480c2fd5b55075c0fa71b84ef9dcf0521f2d58baa14a803a7311c"
       Url = "$pypi/86/00/d5436004268f049214193659ebc36550b5ef3925c3d13b4cc980e13be6f5/nvidia_cuda_runtime-13.4.92-py3-none-win_amd64.whl" },
    @{ Name = "nvidia_cublas-13.4.1.3-py3-none-win_amd64.whl"; Kind = "wheel"; Sub = "cuda"
       Sha = "7e7f780caf07bde7b36b0d25bdad29e8ccbacfc4b75066907bdb601af1e6d4aa"
       Url = "$pypi/bc/0f/8982a363ab246d22876f3eb8baccce81f0da6304d39f86d63b67389c5457/nvidia_cublas-13.4.1.3-py3-none-win_amd64.whl" },
    @{ Name = "nvidia_cufft-12.4.0.43-py3-none-win_amd64.whl"; Kind = "wheel"; Sub = "cuda"
       Sha = "4ff7075f2d0b5f69291f70938d37a86ec632cbe5747184c74ba1f50f17accacc"
       Url = "$pypi/7b/cc/be7fe31058127336a66c88414a2ecc6beecf680baf85994a43eaf292b202/nvidia_cufft-12.4.0.43-py3-none-win_amd64.whl" },
    @{ Name = "nvidia_cudnn_cu13-9.27.0.42-py3-none-win_amd64.whl"; Kind = "wheel"; Sub = "cuda"
       Sha = "7d96f634adafd55c72231eb0500ca77ab109ec8ebff7b33000b76e081bc4558e"
       Url = "$pypi/87/6a/e55ff0ac26a5c6e2b21f41c9d04ad096b4ed6da593fba7e25845c61b0532/nvidia_cudnn_cu13-9.27.0.42-py3-none-win_amd64.whl" }
)

$arm64 = @(
    @{ Name = "onnxruntime-win-arm64-1.31.0.zip"; Kind = "zip"; Prefix = "onnxruntime-win-arm64-1.31.0/lib/"; Sub = "onnxruntime"
       Sha = "ef6206fa8d6975878d93a97035d280240ce5764c126a7430e5427e18923316d5"
       Url = "$gh/v1.31.0/onnxruntime-win-arm64-1.31.0.zip" },
    @{ Name = "cuda_ep_cuda13_0.3.0_win-arm64.zip"; Kind = "zip"; Prefix = ""; Sub = "cuda-plugin"
       Sha = "cf8d00c19ab08f70b628cf8326cc9be948877a31a2d3000a51d0560f1d93a62b"
       Url = "$gh/plugin-ep-cuda/v0.3.0/cuda_ep_cuda13_0.3.0_win-arm64.zip" }
)
# Unverified on hardware: the earliest cuBLAS wheel published for win_arm64 is 13.7.0.27.
$arm64Cuda = @(
    @{ Name = "nvidia_cuda_runtime-13.4.92-py3-none-win_arm64.whl"; Kind = "wheel"; Sub = "cuda"
       Sha = "43972819798ca06ad6354f6cbdb82a0a113f88b94e9ea90256d660027b0b3b60"
       Url = "$pypi/b5/bc/9a141b73d65b9f09e548fcb9b7f4415da22f35e0fe78c60e3c0a9b458efd/nvidia_cuda_runtime-13.4.92-py3-none-win_arm64.whl" },
    @{ Name = "nvidia_cufft-12.4.0.43-py3-none-win_arm64.whl"; Kind = "wheel"; Sub = "cuda"
       Sha = "4e8d551542bd661aef431422acddc5f06f793ac9b1dc7af53de63cbfde5781f9"
       Url = "$pypi/0b/ee/9217a02eae5b0b183b62b39b65f1e64ee8ffb6d6da67a79fd7a2f295ed3a/nvidia_cufft-12.4.0.43-py3-none-win_arm64.whl" }
)

$items = if ($Arch -eq "x64") { $x64 } else { $arm64 + $(if ($WithCudaRuntime) { $arm64Cuda } else { @() }) }
$root = Join-Path $Dest $Arch
$cache = Join-Path $Dest "_downloads"
New-Item -ItemType Directory -Force $root, $cache | Out-Null

function Get-Sha256([string]$Path) { (Get-FileHash $Path -Algorithm SHA256).Hash.ToLower() }

$extracted = @()
foreach ($it in $items) {
    $archive = Join-Path $cache $it.Name
    $ok = (Test-Path $archive) -and ((Get-Sha256 $archive) -eq $it.Sha)
    if (-not $ok -or $Force) {
        Write-Host "get $($it.Name)"
        Invoke-WebRequest -Uri $it.Url -OutFile $archive -UseBasicParsing -UserAgent "MarloweCowork fetch-gpu-runtime"
    }
    $got = Get-Sha256 $archive
    if ($got -ne $it.Sha) {
        Remove-Item $archive -Force
        throw "$($it.Name): sha256 $got does not match the pinned $($it.Sha). Nothing was extracted."
    }
    $out = Join-Path $root $it.Sub
    New-Item -ItemType Directory -Force $out | Out-Null
    $zip = [System.IO.Compression.ZipFile]::OpenRead($archive)
    try {
        foreach ($e in $zip.Entries) {
            if (-not $e.Name.ToLower().EndsWith(".dll")) { continue }
            $take = if ($it.Kind -eq "wheel") { $e.FullName -like "nvidia/*/bin/*" } else { $e.FullName.StartsWith($it.Prefix) }
            if (-not $take) { continue }
            $target = Join-Path $out $e.Name
            [System.IO.Compression.ZipFileExtensions]::ExtractToFile($e, $target, $true)
            $extracted += [pscustomobject]@{ file = "$($it.Sub)/$($e.Name)"; sha256 = (Get-Sha256 $target); from = $it.Name }
        }
    } finally { $zip.Dispose() }
    Write-Host ("ok  {0}  sha256 {1}" -f $it.Name, $it.Sha)
}

$manifest = [ordered]@{
    arch = $Arch
    fetched = (Get-Date).ToString("s")
    archives = @($items | ForEach-Object { [ordered]@{ name = $_.Name; sha256 = $_.Sha; url = $_.Url } })
    files = $extracted
}
$manifest | ConvertTo-Json -Depth 4 | Set-Content -Encoding utf8 (Join-Path $root "manifest.json")
Write-Host ("{0} files under {1}" -f $extracted.Count, $root)
