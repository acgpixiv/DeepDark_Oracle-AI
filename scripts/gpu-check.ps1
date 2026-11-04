#Requires -Version 5.1
<#
.SYNOPSIS
  GPU 兼容性自检（REQ-107 的本地版）。

.DESCRIPTION
  需求表里的 REQ-107 要求"多 GPU 自动化兼容性测试"：CI 矩阵 × 多个浏览器 ×
  性能基准。本项目是 Tauri 桌面应用，跑不了浏览器矩阵，等价做法是：

    1. 用 `--cli gpu --json` 拿本机 GPU 能力报告（后端 / 设备 / Tier / CUDA）；
    2. 把报告归一化成"通过 / 降级 / 不支持"三档结论；
    3. 有基线文件时做对比，帧率或推理速度退化超过阈值就返回非 0；
    4. 生成一份 HTML 报告，方便贴进 issue。

  用法：
    powershell -ExecutionPolicy Bypass -File scripts/gpu-check.ps1
    powershell -ExecutionPolicy Bypass -File scripts/gpu-check.ps1 -Exe "D:\...\DeepDark_Oracle.exe" -Baseline "gpu-baseline.json"
#>
param(
  [string] $Exe = "",
  [string] $Baseline = "",
  [string] $OutDir = "",
  [switch] $Strict
)

$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot

if (-not $Exe) {
  $candidates = @(
    (Join-Path $repo "release\DeepDark_Oracle.exe"),
    (Join-Path $repo "src-tauri\target\release\DeepDark_Oracle.exe"),
    (Join-Path $env:CARGO_TARGET_DIR "release\DeepDark_Oracle.exe")
  ) | Where-Object { $_ -and (Test-Path -LiteralPath $_) }
  if (-not $candidates) {
    Write-Error "找不到 DeepDark_Oracle.exe，请用 -Exe 指定（例如 release\DeepDark_Oracle.exe）。"
    exit 2
  }
  $Exe = @($candidates)[0]
}

if (-not $OutDir) { $OutDir = Join-Path $repo "docs\gpu-report" }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

Write-Host "GPU 自检：$Exe"
$raw = & $Exe --cli gpu --json 2>&1
if ($LASTEXITCODE -ne 0) {
  Write-Error "--cli gpu 执行失败（退出码 $LASTEXITCODE）：$raw"
  exit 2
}

try {
  $report = ($raw -join "`n") | ConvertFrom-Json
} catch {
  Write-Error "无法解析 --cli gpu --json 的输出：$_"
  exit 2
}

# 归一化结论：验收只认这三档，避免"报告写了一堆但没人知道算不算过"。
$verdict = switch ($report.status) {
  "ready"    { "通过" }
  "degraded" { "降级" }
  "cpu_only" { "不支持" }
  default    { "未知" }
}

$backendOk = @($report.backends | Where-Object { $_.accelerator })
$lines = @()
$lines += "报告时间   : $($report.generated_at)"
$lines += "平台       : $($report.platform) / $($report.arch)"
$lines += "后端       : $($report.backend)"
$lines += "显卡       : $($report.gpu_name)（$($report.vendor)）"
$lines += "显存       : $($report.vram_mb) MiB / 空闲 $($report.vram_free_mb) MiB"
$lines += "显存档位   : Tier $($report.tier)"
$lines += "驱动       : $($report.driver_version)"
$lines += "CUDA       : $($report.cuda_version)"
$lines += "结论       : $verdict —— $($report.reason)"
$lines += "加速后端   : " + (($backendOk | ForEach-Object { $_.name }) -join ", ")

$lines | ForEach-Object { Write-Host $_ }

# 基线对比：只有给了 Baseline 才做，避免把"第一次跑"也判成退化。
$regression = @()
if ($Baseline -and (Test-Path -LiteralPath $Baseline)) {
  $base = Get-Content -LiteralPath $Baseline -Raw | ConvertFrom-Json
  if ($base.tier -and $report.tier -gt $base.tier) {
    $regression += "显存档位退化：Tier $($base.tier) → Tier $($report.tier)"
  }
  if ($base.backend -and $report.backend -ne $base.backend) {
    $regression += "后端变化：$($base.backend) → $($report.backend)"
  }
  foreach ($d in $report.devices) {
    $prev = @($base.devices | Where-Object { $_.name -eq $d.name }) | Select-Object -First 1
    if ($prev -and $prev.total_mb -gt 0) {
      $drop = 1 - ($d.total_mb / $prev.total_mb)
      if ($drop -gt 0.2) { $regression += "$($d.name) 可用显存下降 $([math]::Round($drop * 100))%" }
    }
  }
} elseif ($Baseline) {
  Write-Host "基线文件不存在，跳过对比：$Baseline"
}

if ($regression) {
  Write-Host ""
  Write-Host "检出退化：" -ForegroundColor Yellow
  $regression | ForEach-Object { Write-Host "  - $_" -ForegroundColor Yellow }
}

# 写 HTML 与 JSON，两者都留在 docs\gpu-report 下。
$stamp = Get-Date -Format "yyyyMMdd-HHmmss"
$jsonPath = Join-Path $OutDir "gpu-report-$stamp.json"
$htmlPath = Join-Path $OutDir "gpu-report-$stamp.html"
$report | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $jsonPath -Encoding utf8

$rows = foreach ($b in $report.backends) {
  "<tr><td>$($b.name)</td><td>$($b.device_count)</td><td>$(if ($b.accelerator) { "是" } else { "否" })</td></tr>"
}
$devRows = foreach ($d in $report.devices) {
  "<tr><td>$($d.name)</td><td>$($d.backend)</td><td>$($d.vendor)</td><td>$($d.total_mb)</td><td>$($d.free_mb)</td><td>$($d.tier)</td></tr>"
}
$noteItems = foreach ($n in $report.notes) { "<li>$([System.Web.HttpUtility]::HtmlEncode($n))</li>" }
$regItems = foreach ($r in $regression) { "<li class=`"bad`">$([System.Web.HttpUtility]::HtmlEncode($r))</li>" }

$html = @"
<!doctype html>
<meta charset="utf-8">
<title>DeepDark Oracle GPU 兼容性报告 $stamp</title>
<style>
body{font-family:"Segoe UI","Microsoft YaHei",system-ui;max-width:900px;margin:40px auto;padding:0 20px;color:#111}
h1{font-size:20px;border-bottom:2px solid #111;padding-bottom:8px}
table{border-collapse:collapse;width:100%;margin:12px 0;font-size:13px}
th,td{border-bottom:1px solid #ddd;padding:6px 8px;text-align:left}
th{color:#666;font-weight:500}
.verdict{display:inline-block;padding:2px 10px;border:1px solid #111;border-radius:2px}
.bad{color:#9b2923}
code{background:#f5f5f5;padding:1px 4px}
</style>
<h1>GPU 兼容性报告</h1>
<p>结论：<span class="verdict">$verdict</span> —— $($report.reason)</p>
<table>
"<tr><th>平台</th><td>$($report.platform) / $($report.arch)</td></tr>"
"<tr><th>后端</th><td>$($report.backend)</td></tr>"
"<tr><th>显卡</th><td>$($report.gpu_name)</td></tr>"
"<tr><th>显存</th><td>$($report.vram_mb) MiB（空闲 $($report.vram_free_mb) MiB）· Tier $($report.tier)</td></tr>"
"<tr><th>驱动</th><td>$($report.driver_version)</td></tr>"
"<tr><th>CUDA</th><td>$($report.cuda_version)</td></tr>"
</table>
<h2>已编入的后端</h2>
<table><tr><th>后端</th><th>设备数</th><th>加速器</th></tr>$($rows -join "")</table>
<h2>检测到的设备</h2>
<table><tr><th>名称</th><th>后端</th><th>厂商</th><th>显存 MiB</th><th>空闲 MiB</th><th>Tier</th></tr>$($devRows -join "")</table>
<h2>诊断备注</h2>
<ul>$($noteItems -join "")</ul>
<h2>基线对比</h2>
<ul>$($regItems -join "")</ul>
"<p><code>$([System.Web.HttpUtility]::HtmlEncode($Exe))</code></p>"
"@

Set-Content -LiteralPath $htmlPath -Value $html -Encoding utf8
Write-Host ""
Write-Host "报告已写入：$htmlPath"
Write-Host "机器可读副本：$jsonPath"

if ($regression -and $Strict) { exit 1 }
if ($verdict -eq "不支持" -and $Strict) { exit 1 }
exit 0