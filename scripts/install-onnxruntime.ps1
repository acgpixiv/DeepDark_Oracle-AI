param([switch]$Update)

$ErrorActionPreference = "Stop"

# Keep the native runtime under the repository. Nothing is removed by this
# script; an existing runtime is left in place and a downloaded archive is
# only expanded into a new project-local directory.
$repo = Split-Path -Parent $PSScriptRoot
$version = if ($env:DEEPAISEE_ORT_VERSION) { $env:DEEPAISEE_ORT_VERSION } else { "1.20.1" }
$destination = Join-Path $repo ".tools\onnxruntime"
$native = Join-Path $destination "onnxruntime.dll"
$archive = Join-Path $repo ".tools\downloads\onnxruntime-$version.zip"
$versionFile = "$native.version"

New-Item -ItemType Directory -Force -Path (Split-Path -Parent $archive) | Out-Null
New-Item -ItemType Directory -Force -Path $destination | Out-Null

# Reuse the checked-in/portable DLL when it is already present. This avoids a
# second 13 MB copy and still makes the installation explicit and verifiable.
$portable = Join-Path $repo "onnxruntime.dll"
if ((Test-Path $portable) -and -not (Test-Path $native)) {
  Copy-Item -LiteralPath $portable -Destination $native
}

if ($Update) {
  [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
  $indexUri = "https://api.nuget.org/v3-flatcontainer/microsoft.ml.onnxruntime/index.json"
  $release = Invoke-RestMethod -Uri $indexUri -TimeoutSec 20
  $stableVersions = @($release.versions | Where-Object { $_ -match '^1\.[0-9]+\.[0-9]+$' })
  $latest = $stableVersions | ForEach-Object {
    try { [pscustomobject]@{ Text = $_; Parsed = [version]$_ } } catch { $null }
  } | Where-Object { $_ } | Sort-Object Parsed -Descending | Select-Object -First 1
  if (-not $latest) { throw "NuGet 没有返回可用的稳定 ONNX Runtime 版本" }

  $current = ""
  if (Test-Path -LiteralPath $versionFile) {
    $current = (Get-Content -LiteralPath $versionFile -Raw).Trim()
  } elseif (Test-Path -LiteralPath $native) {
    $current = [regex]::Match((Get-Item -LiteralPath $native).VersionInfo.ProductVersion, '^\d+\.\d+\.\d+').Value
  }
  $currentVersion = $null
  try { if ($current) { $currentVersion = [version]$current } } catch { $currentVersion = $null }
  if ($currentVersion -and $currentVersion -ge $latest.Parsed) {
    Set-Content -LiteralPath $versionFile -Value $current -Encoding Ascii
    Write-Host "ONNX Runtime $current is up to date (latest $($latest.Text))"
    exit 0
  }
  $version = $latest.Text
  $archive = Join-Path $repo ".tools\downloads\onnxruntime-$version.zip"
}

$extract = Join-Path $destination "package-$version"
$candidate = $null
if (-not (Test-Path $archive)) {
  $uri = "https://www.nuget.org/api/v2/package/Microsoft.ML.OnnxRuntime/$version"
  Write-Host "Downloading ONNX Runtime $version to $archive"
  Invoke-WebRequest -Uri $uri -OutFile $archive -TimeoutSec 60
}
New-Item -ItemType Directory -Force -Path $extract | Out-Null
Expand-Archive -LiteralPath $archive -DestinationPath $extract -Force
$candidate = Get-ChildItem -LiteralPath $extract -Filter "onnxruntime.dll" -File -Recurse | Select-Object -First 1
if (-not $candidate) {
  throw "NuGet 包中没有找到 onnxruntime.dll"
}

$length = $candidate.Length
if ($length -lt 1000000) {
  throw "onnxruntime.dll 文件异常（$length bytes）：$($candidate.FullName)"
}
$staged = Join-Path $destination "onnxruntime.dll.pending"
Copy-Item -LiteralPath $candidate.FullName -Destination $staged -Force
if (Test-Path -LiteralPath $native) {
  $backup = "$native.backup-$(Get-Date -Format 'yyyyMMddHHmmss')"
  Move-Item -LiteralPath $native -Destination $backup
}
try {
  Move-Item -LiteralPath $staged -Destination $native
} catch {
  if (Test-Path -LiteralPath $backup) { Move-Item -LiteralPath $backup -Destination $native }
  throw
}

Set-Content -LiteralPath $versionFile -Value $version -Encoding Ascii
Write-Host "ONNX Runtime $version installed in project: $native ($length bytes)"
Write-Host "Set DEEPAISEE_ORT_DLL to override this file for a portable build."
