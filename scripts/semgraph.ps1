$ErrorActionPreference = "Continue"
Set-Location (Join-Path $PSScriptRoot "..")
$exe = "D:\DeepDark_Oracle-build\target\release\DeepDark_Oracle.exe"
$log = Join-Path $env:TEMP "ddo_final.log"
cmd /c "`"$exe`" --cli semgraph --nodes 260 > `"$log`" 2>&1"
Write-Output "EXIT=$LASTEXITCODE"
Get-Content $log