param(
  [string]$InstallRoot = "",
  [string]$TorchIndex = ""
)

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"
$repo = if ($InstallRoot) { (Resolve-Path -LiteralPath $InstallRoot).Path } else { Split-Path -Parent $PSScriptRoot }
$tools = Join-Path $repo ".tools"
$python = Join-Path $tools "python\python.exe"
$temp = Join-Path $tools "temp"
$cache = Join-Path $tools "pip-cache"

New-Item -ItemType Directory -Force -Path $temp, $cache | Out-Null
# An existing python.exe may still lack pip or the embedded site-packages path.
# The idempotent sidecar installer repairs both before installing PyTorch.
$pythonScript = Join-Path $PSScriptRoot "install-python-sidecar.ps1"
if (-not (Test-Path -LiteralPath $pythonScript)) { throw "Python sidecar installer not found: $pythonScript" }
& powershell.exe -NoProfile -ExecutionPolicy Bypass -File $pythonScript -InstallRoot $repo
if ($LASTEXITCODE -ne 0) { throw "Python sidecar installation failed with exit code $LASTEXITCODE" }
if (-not (Test-Path -LiteralPath $python)) { throw "Project-local Python was not installed: $python" }

$env:TEMP = $temp
$env:TMP = $temp
$env:PIP_CACHE_DIR = $cache
$env:PIP_DISABLE_PIP_VERSION_CHECK = "1"
& $python -m pip --version
if ($LASTEXITCODE -ne 0) { throw "pip is unavailable in the project Python sidecar" }

if (-not $TorchIndex) { $TorchIndex = $env:DEEPAISEE_TORCH_INDEX }
if (-not $TorchIndex) {
  $nvidia = Get-Command "nvidia-smi.exe" -ErrorAction SilentlyContinue
  if (-not $nvidia) { $nvidia = Get-Command "nvidia-smi" -ErrorAction SilentlyContinue }
  $hasNvidia = $false
  if ($nvidia) {
    & $nvidia.Source -L *> $null
    $hasNvidia = ($LASTEXITCODE -eq 0)
  }
  $TorchIndex = if ($hasNvidia) { "https://download.pytorch.org/whl/cu128" } else { "https://download.pytorch.org/whl/cpu" }
}

Write-Host "Installing local OCR runtime into $tools"
Write-Host "PyTorch wheel index: $TorchIndex"
& $python -m pip install --upgrade --no-warn-script-location --index-url $TorchIndex torch torchvision
if ($LASTEXITCODE -ne 0) { throw "PyTorch installation failed with exit code $LASTEXITCODE" }
& $python -m pip install --upgrade --no-warn-script-location --index-url "https://pypi.org/simple" transformers accelerate safetensors pillow pymupdf
if ($LASTEXITCODE -ne 0) { throw "PyTorch/OCR dependency installation failed with exit code $LASTEXITCODE" }

$probe = "import torch, transformers, PIL, fitz; print('torch=' + torch.__version__); print('cuda=' + str(torch.cuda.is_available())); print('transformers=' + transformers.__version__)"
& $python -c $probe
if ($LASTEXITCODE -ne 0) { throw "Installed PyTorch OCR runtime failed its import check" }
Write-Host "PyTorch and OCR runtime installed successfully."
