$ErrorActionPreference = "Stop"

$repo = Split-Path -Parent $PSScriptRoot
$llama = Join-Path $repo "vendor\llama.cpp"
$arch = Join-Path $llama "src\llama-arch.h"
$modelHeader = Join-Path $llama "src\models\models.h"
$modelSource = Join-Path $llama "src\models\gemma-embedding2.cpp"
$basePatch = Join-Path $repo "patches\llama-embeddinggemma2.patch"
$runtimePatch = Join-Path $repo "patches\llama-embeddinggemma2-runtime.patch"
$sourceTemplate = Join-Path $repo "patches\llama\gemma-embedding2.cpp"

if (-not (Test-Path -LiteralPath $arch)) {
  throw "llama.cpp was not found at $llama. Run scripts\bootstrap-windows.ps1 first."
}

$source = [System.IO.File]::ReadAllText($arch)
$headerSource = if (Test-Path -LiteralPath $modelHeader) { [System.IO.File]::ReadAllText($modelHeader) } else { "" }
$modelText = if (Test-Path -LiteralPath $modelSource) { [System.IO.File]::ReadAllText($modelSource) } else { "" }
# Upstream changed build_inp_embd() to take an attention scale argument
# (build_inp_embd(model.tok_embd, sqrtf(n_embd))), so match the call prefix only.
if ($source.Contains("LLM_ARCH_GEMMA_EMBEDDING2") -and
    $headerSource.Contains("struct llama_model_gemma_embedding2") -and
    $modelText.Contains("build_inp_embd(model.tok_embd")) {
  Write-Host "llama.cpp already supports gemma-embedding2"
  exit 0
}

Push-Location $repo
try {
  if (-not $source.Contains("LLM_ARCH_GEMMA_EMBEDDING2")) {
    & git apply --check $basePatch
    if ($LASTEXITCODE -ne 0) {
      throw "The base EmbeddingGemma 2 patch does not match this llama.cpp source. Check the llama.cpp revision before building."
    }
    & git apply $basePatch
    if ($LASTEXITCODE -ne 0) {
      throw "Failed to apply the base EmbeddingGemma 2 patch."
    }
  }

  if (-not $headerSource.Contains("struct llama_model_gemma_embedding2")) {
    & git apply --check $runtimePatch
    if ($LASTEXITCODE -ne 0) {
      throw "The Gemma2 runtime backport does not match this llama.cpp source. Check the llama.cpp revision before building."
    }
    & git apply $runtimePatch
    if ($LASTEXITCODE -ne 0) {
      throw "Failed to apply the Gemma2 runtime backport."
    }
  }

  if (-not (Test-Path -LiteralPath $modelSource)) {
    Copy-Item -LiteralPath $sourceTemplate -Destination $modelSource
  }

  if ($LASTEXITCODE -ne 0) {
    throw "Failed to install the EmbeddingGemma 2 runtime implementation."
  }
  Write-Host "Applied full gemma-embedding2 support for this llama.cpp source"
} finally {
  Pop-Location
}
