$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

# Build the Tauri release and the NSIS installer in one shot.
# CUDA is only a build-time switch for Candle kernels; the app picks the GPU at run time.
$repo = Split-Path -Parent $PSScriptRoot
. (Join-Path $PSScriptRoot "activate-local.ps1")
# Vulkan 由 build.rs 自动探测：不在这里给 DEEPAISEE_VULKAN 设默认值。
# 早期版本在这写了一行 "= 0"，结果把自动探测永远关掉，装好的 MSYS2 Vulkan SDK
# 白放着不用，GPU 推理只能退回 CPU。需要强制关闭时由用户显式设置
# DEEPAISEE_VULKAN=0 即可（activate-local.ps1 与 build.rs 都尊重这个值）。

function Find-CudaRoot {
  $candidates = @()
  if ($env:DEEPAISEE_CUDA_ROOT) { $candidates += $env:DEEPAISEE_CUDA_ROOT }
  if ($env:CUDA_PATH) { $candidates += $env:CUDA_PATH }
  if ($env:CUDA_HOME) { $candidates += $env:CUDA_HOME }
  $candidates += (Join-Path $repo ".tools\cuda")
  $candidates += (Join-Path $repo ".tools\cuda-toolkit")
  $candidates += (Get-ChildItem "C:\Program Files\NVIDIA GPU Computing Toolkit\CUDA\v*" -Directory -ErrorAction SilentlyContinue |
    Sort-Object Name -Descending | Select-Object -ExpandProperty FullName)
  foreach ($candidate in $candidates) {
    if ($candidate -and (Test-Path (Join-Path $candidate "bin\nvcc.exe"))) {
      return (Resolve-Path -LiteralPath $candidate).Path
    }
  }
  return $null
}

function Stage-CudaRuntime([string]$cudaRoot) {
  $runtimeSource = Join-Path $cudaRoot "bin\x64"
  $runtimeTarget = Join-Path $repo ".tools\cuda-runtime"
  New-Item -ItemType Directory -Force -Path $runtimeTarget | Out-Null

  $patterns = @("cublas64_*.dll", "cublasLt64_*.dll", "cudart64_*.dll", "curand64_*.dll", "nvrtc64_*.dll", "nvrtc-builtins64_*.dll", "nvJitLink_*.dll")
  $files = foreach ($pattern in $patterns) {
    Get-ChildItem -LiteralPath $runtimeSource -Filter $pattern -File -ErrorAction SilentlyContinue
  }
  foreach ($required in @("cublas64_*.dll", "cublasLt64_*.dll", "cudart64_*.dll", "curand64_*.dll")) {
    if (-not ($files | Where-Object { $_.Name -like $required })) {
      throw "CUDA Toolkit is missing the required runtime library: $required"
    }
  }
  foreach ($file in $files | Sort-Object Name -Unique) {
    Copy-Item -LiteralPath $file.FullName -Destination (Join-Path $runtimeTarget $file.Name) -Force
  }

  $openMpRuntime = Join-Path $env:VCToolsRedistDir "x64\Microsoft.VC143.OpenMP\vcomp140.dll"
  if (-not (Test-Path -LiteralPath $openMpRuntime)) {
    throw "Visual Studio x64 OpenMP runtime not found: $openMpRuntime"
  }
  Copy-Item -LiteralPath $openMpRuntime -Destination (Join-Path $runtimeTarget "vcomp140.dll") -Force
  Write-Host "Staged $($files.Count + 1) CUDA/MSVC runtime DLLs in $runtimeTarget"
}

# Everything the installer ships must exist before tauri-bundler starts packaging.
function Assert-ReleasePrerequisites {
  $required = @(
    "tools\bin\ffmpeg.exe",
    "src-tauri\nsis\installer.nsi",
    "src-tauri\nsis\installer-hooks.nsh",
    "scripts\install-onnxruntime.ps1",
    "scripts\install-python-sidecar.ps1"
  )
  $missing = @()
  foreach ($relative in $required) {
    if (-not (Test-Path -LiteralPath (Join-Path $repo $relative))) { $missing += $relative }
  }
  if ($missing.Count -gt 0) {
    $list = ($missing -join ([Environment]::NewLine + "  "))
    throw ("Missing installer resources:" + [Environment]::NewLine + "  " + $list + [Environment]::NewLine + "Restore the missing installer resources before packaging.")
  }
  foreach ($pattern in @("models\clip\*", "models\clap\*")) {
    $hits = Get-ChildItem -Path (Join-Path $repo $pattern) -File -ErrorAction SilentlyContinue
    if (-not $hits) { Write-Warning "Optional cross-modal models not found: $pattern" }
  }
  # tauri-bundler treats every [[bin]] under src-tauri/src/bin as an external
  # binary and copies it into the installer. Dev probes must live in
  # src-tauri/examples so they never ship.
  $strayBins = Get-ChildItem -Path (Join-Path $repo "src-tauri\src\bin") -Filter "*.rs" -File -ErrorAction SilentlyContinue
  if ($strayBins) {
    throw ("src-tauri\src\bin contains probe binaries that would be bundled into the installer: " + (($strayBins | ForEach-Object { $_.Name }) -join ", ") + ". Move them to src-tauri\examples.")
  }
  Write-Host "Installer prerequisites OK"
}

function Resolve-TargetRoot {
  if ($env:CARGO_TARGET_DIR -and (Test-Path -LiteralPath $env:CARGO_TARGET_DIR)) { return (Resolve-Path -LiteralPath $env:CARGO_TARGET_DIR).Path }
  return (Join-Path $repo "src-tauri\target")
}

# esbuild 会往 %TEMP% 写临时文件再把它删掉。本机的用户 Temp 目录
# （C:\Users\alin\AppData\Local\Temp）上这个删除会被拒，vite 的
# esbuild-transpile 随即报 "remove ...: Access is denied"，整个
# `npm run tauri:release` 在 beforeBuildCommand 阶段就挂掉，以前只能在外层
# 手动覆写 TEMP 才跑得动。这里把临时目录钉在仓库内的 .tmpbuild，
# 让 VSCode 终端里直接敲 `npm run tauri:release` 就能出包。
# 注意必须放在仓库内：放到构建盘的 D:\DeepDark_Oracle-build\tmp 实测仍然被拒。
function Set-BuildTemp {
  $tempRoot = Join-Path $repo ".tmpbuild"
  New-Item -ItemType Directory -Force -Path $tempRoot | Out-Null
  $env:TEMP = $tempRoot
  $env:TMP = $tempRoot
  Write-Host "Temp dir     : $tempRoot"
}

Push-Location $repo
try {
  Set-BuildTemp
  & (Join-Path $PSScriptRoot "patch-llama-embeddinggemma2.ps1")
  Assert-ReleasePrerequisites
  $runtimeDir = Join-Path $repo ".tools\cuda-runtime"
  New-Item -ItemType Directory -Force -Path $runtimeDir | Out-Null

  $cudaRoot = Find-CudaRoot
  $features = "llama,candle,custom-protocol"
  if ($cudaRoot) {
    $env:CUDA_PATH = $cudaRoot
    $env:CUDA_HOME = $cudaRoot
    $env:DEEPAISEE_CUDA = "1"
    $env:Path = "$(Join-Path $cudaRoot 'bin');$env:Path"
    $features = "llama,candle,candle-cuda,custom-protocol"
    Stage-CudaRuntime $cudaRoot
    Write-Host "Building integrated CUDA release with toolkit: $cudaRoot"
  } else {
    $env:DEEPAISEE_CUDA = "0"
    Write-Host "CUDA Toolkit not found; building the native Candle/llama.cpp release without CUDA kernels."
  }

  $tauriCli = Join-Path $repo "node_modules\.bin\tauri.cmd"
  & $tauriCli build --features $features @args
  if ($LASTEXITCODE -ne 0) { throw "Tauri release build failed with exit code $LASTEXITCODE" }

  $targetRoot = Resolve-TargetRoot
  $releaseDir = Join-Path $targetRoot "release"
  if ($cudaRoot) { Copy-Item -Path (Join-Path $runtimeDir "*.dll") -Destination $releaseDir -Force }

  $installerDir = Join-Path $releaseDir "bundle\nsis"
  $installers = @(Get-ChildItem -Path $installerDir -Filter "*-setup.exe" -File -ErrorAction SilentlyContinue | Sort-Object LastWriteTime -Descending)
  Write-Host ""
  Write-Host "Release binary : $(Join-Path $releaseDir 'DeepDark_Oracle.exe')"
  if ($installers.Count -gt 0) {
    $installer = $installers[0]
    $sizeMb = [math]::Round($installer.Length / 1MB, 1)
    Write-Host "NSIS installer : $($installer.FullName) ($sizeMb MB)"
  } else {
    Write-Warning "No NSIS installer was produced under $installerDir"
  }
} finally {
  Pop-Location
}
