# Puts the two pinned retrieval models into models/ and verifies every file against the sha256 the
# code pins (crates/marlowe-memory/src/cue/dense/embedder.rs and rerank.rs). A mismatch is a hard
# failure: the memory engine refuses to load an unpinned graph anyway, so failing here is earlier
# and clearer.
#
# Sources, in order:
#   1. A local Marlowe_Harness checkout (the fine-tuned cross-encoder exists only there — it was
#      trained in-house on the Session J recipe and is not published).
#   2. Hugging Face, for the jina embedder only.
#
# -Cascade also copies the five extra fine-tuned graphs the GPU rerank cascade fuses (`--features
# cuda`; pinned in crates/marlowe-memory/src/cascade.rs). They share the shipped tokenizer.json.
param(
    [string]$Harness = "C:\Users\matth\Projects\Marlowe_Harness",
    [string]$Dest = (Join-Path $PSScriptRoot "..\models"),
    [switch]$Cascade
)
$ErrorActionPreference = "Stop"

$files = @(
    @{ Dir = "jina-embeddings-v2-small-en"; Name = "model.onnx";
       Sha = "974fdefe71fc9889258f569132b35acae6278874c8d09dbdf7806d23ad0b4497";
       Url = "https://huggingface.co/jinaai/jina-embeddings-v2-small-en/resolve/44e7d1d6caec8c883c2d4b207588504d519788d0/model.onnx" },
    @{ Dir = "jina-embeddings-v2-small-en"; Name = "vocab.txt";
       Sha = "109753d618dbb576a35112f9c20ef35cf3517d46106175bcf010c986a4bef1df";
       Url = "https://huggingface.co/jinaai/jina-embeddings-v2-small-en/resolve/44e7d1d6caec8c883c2d4b207588504d519788d0/vocab.txt" },
    @{ Dir = "ms-marco-MiniLM-L-2-v2-ft-session-j"; Name = "model.onnx";
       Sha = "9c222dac4315cfd2f33f2e865bb651a7a16bf532c11e55b7fb1a43bb041880c0"; Url = $null },
    @{ Dir = "ms-marco-MiniLM-L-2-v2-ft-session-j"; Name = "tokenizer.json";
       Sha = "0d3aef594edd5f9b53e7f814277a9171dc70ff93eb66bda6e01f7aa53997d963"; Url = $null }
)
if ($Cascade) {
    $graphs = @(
        @("ms-marco-MiniLM-L-6-v2-ft-session-j", "78dcc7c1834b2e0cfc67d58a2735b9bc27900f46d5b5ccada99e8ee610c724f6"),
        @("ms-marco-MiniLM-L-2-v2-ft-w1", "cc4df68c30d319cab3d74c166f4de18c4ade7e8c1fccd1fbc1512bdffee58b96"),
        @("ms-marco-MiniLM-L-4-v2-ft-w1", "f6324276380fbc6a6a34864c077a94d5227d9719d9d5dfbc80bcb8695078fddb"),
        @("ms-marco-MiniLM-L-6-v2-ft-w1", "bbfb8a0831db882397c2b83f1c108e233320e1c257c923216357d2c48e25e050"),
        @("ms-marco-MiniLM-L-12-v2-ft-w1", "b1a374f3cd2752af2143d4f01853282a9a1d4e7ee8137e82c5827291b26ccdd6")
    )
    foreach ($g in $graphs) {
        $files += @{ Dir = $g[0]; Name = "model.onnx"; Sha = $g[1]; Url = $null }
        $files += @{ Dir = $g[0]; Name = "tokenizer.json"; Url = $null
                     Sha = "0d3aef594edd5f9b53e7f814277a9171dc70ff93eb66bda6e01f7aa53997d963" }
    }
}

foreach ($f in $files) {
    $dir = Join-Path $Dest $f.Dir
    New-Item -ItemType Directory -Force $dir | Out-Null
    $target = Join-Path $dir $f.Name

    $ok = (Test-Path $target) -and ((Get-FileHash $target -Algorithm SHA256).Hash.ToLower() -eq $f.Sha)
    if (-not $ok) {
        $local = Join-Path $Harness "models\$($f.Dir)\$($f.Name)"
        if (Test-Path $local) {
            Copy-Item $local $target -Force
        } elseif ($f.Url) {
            Invoke-WebRequest -Uri $f.Url -OutFile $target -UseBasicParsing
        } else {
            throw "$($f.Dir)/$($f.Name): not found in $Harness and has no public source."
        }
    }

    $got = (Get-FileHash $target -Algorithm SHA256).Hash.ToLower()
    if ($got -ne $f.Sha) {
        Remove-Item $target -Force
        throw "$($f.Dir)/$($f.Name): sha256 $got does not match the pinned $($f.Sha)."
    }
    Write-Host ("ok  {0}/{1}" -f $f.Dir, $f.Name)
}
