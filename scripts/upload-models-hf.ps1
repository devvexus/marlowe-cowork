# Publishes the fine-tuned rerankers to the public Hugging Face repo that `marlowe models fetch`, the
# first-Wake offer and scripts/fetch-models.ps1 download from. Run by hand, once, by the owner of the
# repo; nothing else uploads anything. The jina embedder is NOT uploaded: it is public upstream.
#
#   pip install -U "huggingface_hub[cli]"
#   hf auth login          # a token with write access to the repo (older CLIs: huggingface-cli login)
#   powershell -NoProfile -File scripts/upload-models-hf.ps1            # check, then upload
#   powershell -NoProfile -File scripts/upload-models-hf.ps1 -CheckOnly # check the hashes only
#
# The repo mirrors models/: <dir>/model.onnx and <dir>/tokenizer.json, which is the URL layout
# https://huggingface.co/<repo>/resolve/main/<dir>/<file>. It has to be public (the app downloads
# without a token). Every file is checked against the sha256 the code pins (embedder.rs, rerank.rs,
# cascade.rs) first, so a wrong file never goes up; the app checks again after downloading.
param(
    [string]$Repo = "devvexus/marlowe-memory",
    [string]$Models = (Join-Path $PSScriptRoot "..\models"),
    [switch]$CheckOnly
)
$ErrorActionPreference = "Stop"

# The shipped cross-encoder, then the five other cascade graphs. All carry the same tokenizer.json.
$tokenizer = "0d3aef594edd5f9b53e7f814277a9171dc70ff93eb66bda6e01f7aa53997d963"
$graphs = [ordered]@{
    "ms-marco-MiniLM-L-2-v2-ft-session-j" = "9c222dac4315cfd2f33f2e865bb651a7a16bf532c11e55b7fb1a43bb041880c0"
    "ms-marco-MiniLM-L-6-v2-ft-session-j" = "78dcc7c1834b2e0cfc67d58a2735b9bc27900f46d5b5ccada99e8ee610c724f6"
    "ms-marco-MiniLM-L-2-v2-ft-w1"        = "cc4df68c30d319cab3d74c166f4de18c4ade7e8c1fccd1fbc1512bdffee58b96"
    "ms-marco-MiniLM-L-4-v2-ft-w1"        = "f6324276380fbc6a6a34864c077a94d5227d9719d9d5dfbc80bcb8695078fddb"
    "ms-marco-MiniLM-L-6-v2-ft-w1"        = "bbfb8a0831db882397c2b83f1c108e233320e1c257c923216357d2c48e25e050"
    "ms-marco-MiniLM-L-12-v2-ft-w1"       = "b1a374f3cd2752af2143d4f01853282a9a1d4e7ee8137e82c5827291b26ccdd6"
}

foreach ($dir in $graphs.Keys) {
    foreach ($f in @(@("model.onnx", $graphs[$dir]), @("tokenizer.json", $tokenizer))) {
        $path = Join-Path $Models "$dir\$($f[0])"
        if (-not (Test-Path $path)) { throw "$dir/$($f[0]): not found under $Models (run scripts/fetch-models.ps1 -Cascade first)." }
        $got = (Get-FileHash $path -Algorithm SHA256).Hash.ToLower()
        if ($got -ne $f[1]) { throw "$dir/$($f[0]): sha256 $got does not match the pinned $($f[1]); nothing was uploaded." }
        Write-Host ("ok  {0}/{1}" -f $dir, $f[0])
    }
}
if ($CheckOnly) { return }

if (-not (Get-Command hf -ErrorAction SilentlyContinue)) { throw "the 'hf' CLI is not installed: pip install -U ""huggingface_hub[cli]""" }
foreach ($dir in $graphs.Keys) {
    hf upload $Repo (Join-Path $Models $dir) $dir --include "model.onnx" --include "tokenizer.json" --commit-message "Add $dir"
    if ($LASTEXITCODE -ne 0) { throw "hf upload failed for $dir (exit $LASTEXITCODE)" }
}
Write-Host "done: https://huggingface.co/$Repo (make sure the repo is public)"
