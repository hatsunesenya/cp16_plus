# 给 CP16+ 的 exe 做 Authenticode 签名
#
# 用法（需要先有代码签名证书）：
#   .\tools\sign.ps1 -Thumbprint 1234567890ABCDEF... -Files "dist\cp16plus-B0.1.2-261108\*.exe"
#   .\tools\sign.ps1 -Pfx .\cert.pfx -Password "***" -Files ...
#
# 说明：签名是解决杀软误报最有效的一步（尤其是 SmartScreen 与国产杀软的"未知发布者"判定）。

param(
    [string]$Thumbprint,
    [string]$Pfx,
    [string]$Password,
    [string]$Timestamp = 'http://timestamp.digicert.com',
    [string[]]$Files = @()
)

$ErrorActionPreference = 'Stop'

function Find-SignTool {
    $cands = @(
        (Get-Command signtool.exe -ErrorAction SilentlyContinue | Select-Object -First 1 -ExpandProperty Source),
        "${env:ProgramFiles(x86)}\Windows Kits\10\bin\x64\signtool.exe"
    )
    foreach ($c in $cands) {
        if ($c -and (Test-Path $c)) { return $c }
    }
    $kit = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin" -Directory -ErrorAction SilentlyContinue |
        Sort-Object Name -Descending | Select-Object -First 1
    if ($kit) {
        $p = Join-Path $kit.FullName 'x64\signtool.exe'
        if (Test-Path $p) { return $p }
    }
    throw '没找到 signtool.exe。安装 Windows SDK（只勾选 "Windows SDK Signing Tools"）后再试。'
}

if (-not $Thumbprint -and -not $Pfx) {
    throw '请用 -Thumbprint（证书在个人存储里）或 -Pfx + -Password 指定证书。'
}

$signtool = Find-SignTool
$files = @()
foreach ($pattern in $Files) {
    $files += Get-ChildItem $pattern -File -ErrorAction SilentlyContinue
}
if (-not $files) { throw '没有匹配到要签名的 exe（-Files 支持通配符，记得加引号）。' }

foreach ($f in $files) {
    $args = @('sign', '/fd', 'sha256', '/td', 'sha256', '/tr', $Timestamp, '/v')
    if ($Thumbprint) { $args += @('/sha1', $Thumbprint) } else { $args += @('/f', $Pfx, '/p', $Password) }
    $args += $f.FullName
    Write-Host "签名 $($f.Name) ..."
    & $signtool @args
    if ($LASTEXITCODE -ne 0) { throw "签名失败：$($f.Name)" }
    & $signtool verify /pa /v $f.FullName | Out-Null
    if ($LASTEXITCODE -ne 0) { Write-Warning "验签未通过：$($f.Name)" }
}

Write-Host ''
Write-Host '全部完成。签名后请重新计算哈希并附在发布页上：' -ForegroundColor Green
foreach ($f in $files) {
    $h = (Get-FileHash $f.FullName -Algorithm SHA256).Hash
    Write-Host ("  {0}  {1}" -f $f.Name, $h)
}
