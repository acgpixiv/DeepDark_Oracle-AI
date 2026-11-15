param(
  [Parameter(ValueFromRemainingArguments = $true)]
  [string[]] $CliArgs
)

$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot

# Make the normal `npm run tauri dev` command include the native Candle
# backend when the local CUDA toolkit is available. Explicit --features wins.
. (Join-Path $PSScriptRoot "activate-local.ps1")
# build.rs/activate-local.ps1 detect a project-local or system Vulkan SDK.
# Respect an explicit DEEPAISEE_VULKAN=0/1 override, otherwise let that
# detection decide so integrated GPUs and AMD adapters are compiled in when
# the local toolchain supports them.

$tauri = Join-Path $repo "node_modules\.bin\tauri.cmd"
if (-not (Test-Path -LiteralPath $tauri)) {
  throw "Tauri CLI is missing. Run npm ci first."
}

$hasFeatures = $false
foreach ($arg in $CliArgs) {
  if ($arg -eq "--features" -or $arg -like "--features=*") {
    $hasFeatures = $true
    break
  }
}

$forward = @($CliArgs)
if (-not $hasFeatures -and $CliArgs.Count -gt 0 -and $CliArgs[0] -in @("dev", "build")) {
  $cuda = $env:DEEPAISEE_CUDA_ROOT
  if (-not $cuda) { $cuda = $env:CUDA_PATH }
  $cudaReady = $cuda -and (Test-Path -LiteralPath (Join-Path $cuda "bin\nvcc.exe"))
  $features = if ($cudaReady) { "llama,candle,candle-cuda" } else { "llama,candle" }
  $forward += @("--features", $features)
  Write-Host "Tauri features: $features"
}

& $tauri @forward
exit $LASTEXITCODE
