$ErrorActionPreference = "Continue"
Set-Location (Join-Path $PSScriptRoot "..")
. .\scripts\activate-local.ps1 *> $null
$tmp = Join-Path $PSScriptRoot "..\build-tmp"
New-Item -ItemType Directory -Force -Path $tmp | Out-Null
$env:TMP = $tmp; $env:TEMP = $tmp
$log = Join-Path $tmp "web.log"
cmd /c "npm run build > $log 2>&1"
Write-Output "EXIT=$LASTEXITCODE"