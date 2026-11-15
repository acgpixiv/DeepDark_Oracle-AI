param([string]$InstallRoot = "")

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

# Portable Python is optional. It lives inside the project and does not change
# PATH, registry entries, or the system Python installation.
$repo = if ($InstallRoot) { (Resolve-Path -LiteralPath $InstallRoot).Path } else { Split-Path -Parent $PSScriptRoot }
$version = if ($env:DEEPAISEE_PYTHON_VERSION) { $env:DEEPAISEE_PYTHON_VERSION } else { "3.12.10" }
$tools = Join-Path $repo ".tools"
$destination = Join-Path $tools "python"
$downloads = Join-Path $tools "downloads"
$temp = Join-Path $tools "temp"
$sitePackages = Join-Path $destination "Lib\site-packages"
$archive = Join-Path $downloads "python-$version-embed-amd64.zip"
$python = Join-Path $destination "python.exe"

New-Item -ItemType Directory -Force -Path $downloads | Out-Null
New-Item -ItemType Directory -Force -Path $temp | Out-Null
New-Item -ItemType Directory -Force -Path $destination | Out-Null
if (-not (Test-Path $python)) {
  if (-not (Test-Path $archive)) {
    $uri = "https://www.python.org/ftp/python/$version/python-$version-embed-amd64.zip"
    Write-Host "Downloading portable Python $version to $archive"
    Invoke-WebRequest -Uri $uri -OutFile $archive
  }
  Expand-Archive -LiteralPath $archive -DestinationPath $destination -Force
}

if (-not (Test-Path $python)) {
  throw "Python sidecar installation failed: python.exe was not found"
}
New-Item -ItemType Directory -Force -Path $sitePackages | Out-Null
$pth = Get-ChildItem -LiteralPath $destination -Filter "python*._pth" -File -ErrorAction SilentlyContinue | Select-Object -First 1
if ($pth) {
  $lines = @(Get-Content -LiteralPath $pth.FullName)
  if (-not ($lines | Where-Object { $_.Trim() -eq "Lib\site-packages" })) { $lines += "Lib\site-packages" }
  $lines = @($lines | ForEach-Object { if ($_.Trim() -eq "#import site") { "import site" } else { $_ } })
  if (-not ($lines | Where-Object { $_.Trim() -eq "import site" })) { $lines += "import site" }
  Set-Content -LiteralPath $pth.FullName -Value $lines -Encoding Ascii
}

$env:TEMP = $temp
$env:TMP = $temp
$env:PIP_CACHE_DIR = Join-Path $tools "pip-cache"
$getPip = Join-Path $downloads "get-pip.py"
# Missing pip is expected in an embedded Python installation. Catch the import
# inside Python so Windows PowerShell does not turn stderr into a terminating
# NativeCommandError before we can bootstrap it.
$pipProbe = @'
try:
    import pip
except Exception:
    raise SystemExit(1)
'@
& $python -c $pipProbe
if ($LASTEXITCODE -ne 0) {
  if (-not (Test-Path -LiteralPath $getPip)) {
    Invoke-WebRequest -Uri "https://bootstrap.pypa.io/get-pip.py" -OutFile $getPip
  }
  & $python $getPip --disable-pip-version-check --no-warn-script-location
  if ($LASTEXITCODE -ne 0) { throw "pip bootstrap failed with exit code $LASTEXITCODE" }
}
& $python -m pip --version
if ($LASTEXITCODE -ne 0) { throw "pip bootstrap finished but pip is still unavailable" }
Write-Host "Python sidecar installed in project: $python"
Write-Host "Python site-packages: $sitePackages"
Write-Host "pip is ready for optional local adapters."
