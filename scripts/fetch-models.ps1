# Puts the two pinned retrieval models into models/ and verifies every file against the sha256 the
# code pins (crates/marlowe-memory/src/cue/dense/embedder.rs and rerank.rs). A mismatch is a hard
# failure: the memory engine refuses to load an unpinned graph anyway, so failing here is earlier
# and clearer.
#
# Sources, in order:
#   1. A local Marlowe_Harness checkout (the fine-tuned cross-encoder exists only there — it was
#      trained in-house on the Session J recipe and is not published).
#   2. Hugging Face, for the jina embedder only.
param(
    [string]$Harness = "C:\Users\matth\Projects\Marlowe_Harness",
    [string]$Dest = (Join-Path $PSScriptRoot "..\models")
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
