$ErrorActionPreference = "Stop"

$repo = Split-Path -Parent $PSScriptRoot
# Toolchains live in .tools by default; checkouts that unpacked them next to the
# repo (tools/) must keep working too, so probe both before giving up.
$tools = Join-Path $repo ".tools"
if (-not (Test-Path -LiteralPath (Join-Path $tools "cargo"))) {
  $fallbackTools = Join-Path $repo "tools"
  if (Test-Path -LiteralPath (Join-Path $fallbackTools "cargo")) { $tools = $fallbackTools }
}
# A checkout whose own path contains non-ASCII characters cannot link with the
# GNU toolchain: MSYS2 gcc resolves the encoded rlib paths through the ANSI code
# page, so "软件" reaches ld as mojibake and the libcore rlib looks missing.
# subst/junction do not help (rustc resolves its sysroot back to the real long
# path), so the only working fix is a real copy of the toolchain on a pure-ASCII
# path. Resolve-RustupHome below prepares that copy on demand.
$isAsciiPath = -not ($repo -match '[^\x00-\x7F]')

function Test-RustupHome([string] $path) {
  return ($path -and (Test-Path -LiteralPath (Join-Path $path "toolchains\stable-x86_64-pc-windows-gnu\bin\rustc.exe")))
}

function Resolve-RustupHome {
  if ($env:RUSTUP_HOME -and (Test-RustupHome $env:RUSTUP_HOME)) { return $env:RUSTUP_HOME }
  $local = Join-Path $tools "rustup"
  if ($isAsciiPath -and (Test-RustupHome $local)) { return $local }

  # Pick a writable ASCII location outside the checkout for the mirrored toolchain.
  $roots = @()
  if ($env:DDO_TOOLCHAIN_ROOT) { $roots += $env:DDO_TOOLCHAIN_ROOT }
  # Prefer a non-system drive: the toolchain is ~1.1 GB and C: is usually tight.
  $shortRoots = @()
  foreach ($drive in @("D", "E", "F", "C")) {
    if (Test-Path -LiteralPath ($drive + ":\")) {
      $shortRoots += ($drive + ":\ddrust")
      $roots += ($drive + ":\DeepDark_Oracle-toolchain\rustup")
    }
  }
  if ($env:LOCALAPPDATA) { $roots += (Join-Path $env:LOCALAPPDATA "DeepDark_Oracle-toolchain\rustup") }
  # Reuse an already mirrored toolchain before copying another gigabyte.
  foreach ($candidate in $shortRoots + $roots) {
    if (Test-RustupHome $candidate) { return $candidate }
  }

  # Nothing mirrored yet: copy the toolchain once. This is the only step that
  # touches the disk in bulk, and it is skipped on every later build.
  $source = $local
  if (-not (Test-Path -LiteralPath (Join-Path $source "toolchains"))) { return $local }
  foreach ($root in $roots) {
    try {
      $null = New-Item -ItemType Directory -Force -Path $root -ErrorAction Stop
      $candidate = Join-Path $root "rustup"
      Write-Host "Mirroring Rust toolchain to an ASCII path (one time, ~1 GB): $candidate"
      & robocopy $source $candidate /E /NFL /NDL /NJH /NJS /NP /R:1 /W:1 | Out-Null
      if (Test-RustupHome $candidate) { return $candidate }
    } catch {
      Write-Host "Could not mirror the toolchain to $root ($($_.Exception.Message)); trying the next location."
    }
  }
  return $local
}

# A cargo registry home only needs to be readable, so a junction/short path is
# fine there; keep an externally chosen value when it exists.
$cargoHome = if ($env:CARGO_HOME -and (Test-Path -LiteralPath $env:CARGO_HOME)) { $env:CARGO_HOME } else { Join-Path $tools "cargo" }
$rustupHome = Resolve-RustupHome
if (-not $isAsciiPath -and -not (Test-Path -LiteralPath (Join-Path $rustupHome "toolchains\stable-x86_64-pc-windows-gnu\bin"))) {
  Write-Warning "The checkout path contains non-ASCII characters and no ASCII toolchain mirror was found; the GNU linker will probably fail. Set DDO_TOOLCHAIN_ROOT to a writable ASCII directory."
}
$nodeHome = Join-Path $tools "node\node-v22.14.0-win-x64"
# Some checkouts keep a plain tools\node folder instead of the pinned versioned one.
if (-not (Test-Path (Join-Path $nodeHome "node.exe"))) { $nodeHome = Join-Path $tools "node" }
$cmakeHome = Join-Path $tools "cmake"
$ninjaHome = Join-Path $tools "ninja"
# The MSYS2 UCRT64 toolchain ships cmake/ninja/gcc and is what the GNU target
# links with. It is normally unpacked inside the checkout, but hosts that already
# have MSYS2 elsewhere must keep working, so probe the usual locations.
$ucrt64Bin = $null
foreach ($msys in @(
    (Join-Path $tools "msys64"),
    (Join-Path $repo "msys64"),
    $env:MSYS2_ROOT,
    $env:DDO_MSYS2_ROOT,
    "D:\dsh-tools\msys64",
    "C:\msys64",
    "D:\msys64",
    "C:\tools\msys64"
  )) {
  if (-not $msys) { continue }
  $probe = Join-Path $msys "ucrt64\bin"
  if (Test-Path -LiteralPath (Join-Path $probe "gcc.exe")) { $ucrt64Bin = $probe; break }
}
if (-not $ucrt64Bin) { $ucrt64Bin = Join-Path $tools "msys64\ucrt64\bin" }
$npmCache = Join-Path $tools "npm-cache"

$env:DEEPAISEE_REPO = $repo
$env:CARGO_HOME = $cargoHome
$env:RUSTUP_HOME = $rustupHome
# The build directory has to be on a pure-ASCII path too: even with an ASCII
# sysroot, gcc receives the crate rlib paths and mangles the Chinese characters
# through the ANSI code page, so ld reports "cannot find ...\target\debug\deps\
# libserde-....rlib". Pointing CARGO_TARGET_DIR at a short ASCII directory keeps
# every rlib path ASCII and is the only workaround that survives a fresh clone.
function Resolve-AsciiDir([string] $label) {
  if ($env:USE_BUILD_IN_WORKSPACE -eq "1") { return $null }
  if ($env:DDO_BUILD_ROOT) {
    $explicit = Join-Path $env:DDO_BUILD_ROOT $label
    $null = New-Item -ItemType Directory -Force -Path $explicit -ErrorAction SilentlyContinue
    return $explicit
  }
  $candidates = @()
  foreach ($drive in @("D", "E", "F", "C")) {
    if (Test-Path -LiteralPath ($drive + ":\")) { $candidates += ($drive + ":\DeepDark_Oracle-build") }
  }
  if ($env:LOCALAPPDATA) { $candidates += (Join-Path $env:LOCALAPPDATA "DeepDark_Oracle-build") }
  foreach ($root in $candidates) {
    try {
      $null = New-Item -ItemType Directory -Force -Path $root -ErrorAction Stop
      return (Join-Path $root $label)
    } catch { }
  }
  return $null
}

# Keep an externally chosen target dir (CI / limited-disk hosts may redirect it),
# but normalize relative values. Cargo is often invoked from `src-tauri`; a
# relative `src-tauri/target` would otherwise become `src-tauri/src-tauri/target`
# and produce very long CMake paths on Windows.
if ($env:CARGO_TARGET_DIR) {
  if (-not [System.IO.Path]::IsPathRooted($env:CARGO_TARGET_DIR)) {
    $env:CARGO_TARGET_DIR = [System.IO.Path]::GetFullPath((Join-Path $repo $env:CARGO_TARGET_DIR))
  }
} elseif ($isAsciiPath) {
  $env:CARGO_TARGET_DIR = Join-Path $repo "src-tauri\target"
} else {
  $asciiTarget = Resolve-AsciiDir "target"
  if ($asciiTarget) {
    $env:CARGO_TARGET_DIR = $asciiTarget
    Write-Host "Non-ASCII checkout: building into $asciiTarget (needed to keep the GNU linker happy)"
  } else {
    $env:CARGO_TARGET_DIR = Join-Path $repo "src-tauri\target"
    Write-Warning "Non-ASCII checkout without an ASCII build directory; linking will probably fail. Set DDO_BUILD_ROOT."
  }
}
$env:npm_config_cache = $npmCache
$env:npm_config_prefix = Join-Path $tools "npm-global"

# Order matters. The MSYS2 UCRT64 bin directory must come first: it supplies gcc,
# ld, dlltool, cmake and ninja. Putting the Rust toolchain's bin (which also
# carries a different libgcc_s_seh-1.dll / libwinpthread-1.dll) ahead of it makes
# gcc load a mismatched MinGW runtime and fail silently, which shows up as
# "libsqlite3-sys: Compiler family detection failed" with an empty stderr.
$localPaths = @(
  $ucrt64Bin,
  (Join-Path $rustupHome "toolchains\stable-x86_64-pc-windows-gnu\bin"),
  $nodeHome,
  (Join-Path $cmakeHome "bin"),
  $ninjaHome
)

# Prefer a project-local CUDA Toolkit when one has been unpacked or installed.
$localCuda = Join-Path $tools "cuda"
$cudaRoot = if (Test-Path (Join-Path $localCuda "bin\nvcc.exe")) {
  $localCuda
} else {
  Get-ChildItem "C:\Program Files\NVIDIA GPU Computing Toolkit\CUDA\v*" -Directory -ErrorAction SilentlyContinue |
    Sort-Object Name -Descending | Select-Object -First 1 -ExpandProperty FullName
}

# Vulkan SDK：GPU 推理在 Windows/GNU 工具链上唯一的加速途径（CUDA 只支持 MSVC）。
# 三种布局都要认：官方 Khronos SDK（Include/Bin）、MSYS2 的 ucrt64（include/bin）、
# 项目内解包的 .tools\vulkan。
function Test-VulkanRoot([string] $root) {
  if (-not $root) { return $false }
  $header = (Test-Path (Join-Path $root "Include\vulkan\vulkan.h")) -or
            (Test-Path (Join-Path $root "include\vulkan\vulkan.h"))
  $glslc = (Test-Path (Join-Path $root "Bin\glslc.exe")) -or
           (Test-Path (Join-Path $root "bin\glslc.exe"))
  return ($header -and $glslc)
}

$vulkanRoot = $null
foreach ($candidate in @(
  (Join-Path $tools "vulkan"),
  (Join-Path $tools "vulkan-sdk"),
  $env:VULKAN_SDK,
  $env:DEEPAISEE_VULKAN_ROOT,
  # MSYS2 的 vulkan-devel 直接装在 ucrt64 下；本机工具链就在那里。
  $ucrt64Bin,
  "D:\dsh-tools\msys64\ucrt64",
  "C:\msys64\ucrt64",
  "D:\msys64\ucrt64"
)) {
  $dir = $candidate
  if ($dir -and $dir.EndsWith("\bin")) { $dir = Split-Path $dir -Parent }
  if (Test-VulkanRoot $dir) { $vulkanRoot = (Resolve-Path -LiteralPath $dir).Path; break }
}
if ($vulkanRoot) {
  $env:VULKAN_SDK = $vulkanRoot
  $env:DEEPAISEE_VULKAN_ROOT = $vulkanRoot
  $vulkanBin = if (Test-Path (Join-Path $vulkanRoot "Bin\glslc.exe")) { Join-Path $vulkanRoot "Bin" } else { Join-Path $vulkanRoot "bin" }
  if ($localPaths -notcontains $vulkanBin) { $localPaths += $vulkanBin }
  Write-Host "Vulkan SDK: $vulkanRoot"
}
if ($cudaRoot -and (Test-Path (Join-Path $cudaRoot "bin\nvcc.exe"))) {
  $env:CUDA_PATH = $cudaRoot
  $env:CUDA_HOME = $cudaRoot
  $env:DEEPAISEE_CUDA_ROOT = $cudaRoot
  $cudaVersion = Split-Path $cudaRoot -Leaf
  Set-Item -Path ("Env:CUDA_PATH_" + $cudaVersion.Replace('.', '_').ToUpperInvariant()) -Value $cudaRoot
  $localPaths += Join-Path $cudaRoot "bin"
  if (Test-Path (Join-Path $cudaRoot "bin\x64")) { $localPaths += Join-Path $cudaRoot "bin\x64" }
}

$env:Path = (($localPaths + ($env:Path -split ';')) | Where-Object { $_ -and (Test-Path $_) } | Select-Object -Unique) -join ';'

# CUDA's nvcc uses the MSVC host compiler for both Candle PTX and llama.cpp.
# A normal PowerShell session does not have the INCLUDE/LIB variables that
# VsDevCmd sets, so load the installed x64 toolchain here when available.
function Import-VisualStudioEnvironment {
  $vswhere = @(
    (Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'),
    (Join-Path $env:ProgramFiles 'Microsoft Visual Studio\Installer\vswhere.exe')
  ) | Where-Object { $_ -and (Test-Path -LiteralPath $_) } | Select-Object -First 1
  if (-not $vswhere) { return }

  $installation = (& $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath 2>$null | Select-Object -First 1)
  if (-not $installation) { return }
  $vsdevcmd = Join-Path $installation 'Common7\Tools\VsDevCmd.bat'
  if (-not (Test-Path -LiteralPath $vsdevcmd)) { return }

  $cmd = '"{0}" -arch=x64 -host_arch=x64 && set' -f $vsdevcmd
  foreach ($line in (& cmd.exe /d /s /c $cmd)) {
    if ($line -match '^([^=]+)=(.*)$') {
      [Environment]::SetEnvironmentVariable($Matches[1], $Matches[2], 'Process')
    }
  }
  $env:NVCC_CCBIN = 'cl.exe'
  Write-Host "Visual Studio x64 toolchain active: $installation"
}

Import-VisualStudioEnvironment
Write-Host "Project-local environment active: $repo"
Write-Host "Cargo home: $env:CARGO_HOME"
Write-Host "Cargo target: $env:CARGO_TARGET_DIR"
