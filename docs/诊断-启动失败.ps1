# CP16+ 启动失败诊断脚本（0xc000007b 等）
#
# 用法：放进安装目录（默认 C:\Program Files\CP16Plus），右键"使用 PowerShell 运行"，
#      或： powershell -NoProfile -ExecutionPolicy Bypass -File .\诊断-启动失败.ps1
# 它只读文件信息，不修改任何东西。

$ErrorActionPreference = 'Continue'
$dir = if ($PSScriptRoot) { $PSScriptRoot } else { (Get-Location).Path }

function Show-Header($t) {
    Write-Host ''
    Write-Host "== $t ==" -ForegroundColor Cyan
}

function Get-PeMachine($path) {
    try {
        $fs = [IO.File]::OpenRead($path)
        $br = New-Object IO.BinaryReader($fs)
        $fs.Position = 0x3C
        $peOff = $br.ReadInt32()
        $fs.Position = $peOff + 4
        $machine = $br.ReadUInt16()
        $br.Close(); $fs.Close()
        switch ($machine) {
            0x014c { return 'x86 (32 位)' }
            0x8664 { return 'x64 (64 位)' }
            0xAA64 { return 'ARM64' }
            default { return ('0x{0:X4}' -f $machine) }
        }
    } catch {
        return '无法读取'
    }
}

Write-Host "CP16+ 启动诊断  $(Get-Date -Format 'yyyy-MM-dd HH:mm:ss')"
Write-Host "安装目录: $dir"

Show-Header '系统'
$os = Get-CimInstance Win32_OperatingSystem -ErrorAction SilentlyContinue
if ($os) {
    Write-Host ("Windows: {0}  版本 {1}  架构 {2}" -f $os.Caption, $os.Version, $os.OSArchitecture)
    Write-Host ("进程是否 64 位: {0}" -f [Environment]::Is64BitProcess)
} else {
    Write-Host ("$env:PROCESSOR_ARCHITECTURE")
}
if ($env:PROCESSOR_ARCHITECTURE -ne 'AMD64' -and $env:PROCESSOR_ARCHITECTURE -ne 'ARM64') {
    Write-Host '注意：本程序只有 64 位版本，32 位系统无法运行。' -ForegroundColor Yellow
}

Show-Header '安装目录里的程序与 DLL'
$need = @(
    'cp16plus.exe',
    'libhamlib-4.dll',
    'libusb-1.0.dll',
    'libwinpthread-1.dll'
)
foreach ($n in $need) {
    $p = Join-Path $dir $n
    if (Test-Path $p) {
        $size = (Get-Item $p).Length
        $arch = Get-PeMachine $p
        $flag = if ($size -le 0) { '  ← 文件是空的！' } elseif ($arch -like '*32*') { '  ← 32 位，和 64 位主程序不匹配！' } else { '' }
        Write-Host ("{0,-24} {1,10:N0} 字节  {2}{3}" -f $n, $size, $arch, $flag)
    } else {
        Write-Host ("{0,-24} 缺失" -f $n) -ForegroundColor Yellow
    }
}

$extra = Get-ChildItem $dir -Filter '*.exe' -ErrorAction SilentlyContinue
if ($extra) {
    Write-Host ''
    Write-Host '目录内其它 exe：'
    foreach ($f in $extra) {
        Write-Host ("  {0,-28} {1}" -f $f.Name, (Get-PeMachine $f.FullName))
    }
}

Show-Header '系统里同名 DLL 是否被别的程序占用/抢先'
foreach ($n in @('libusb-1.0.dll', 'libwinpthread-1.dll', 'libhamlib-4.dll', 'zlib1.dll')) {
    $w = where.exe $n 2>$null
    if ($w) {
        Write-Host "$n 在搜索路径里找到：" -ForegroundColor Yellow
        foreach ($line in $w) {
            $arch = if (Test-Path $line) { Get-PeMachine $line } else { '?' }
            Write-Host ("   {0}   [{1}]" -f $line, $arch)
        }
    } else {
        Write-Host "$n ：搜索路径里没有"
    }
}

Show-Header '文件属性（是否被"来自其他计算机"阻止）'
$blocked = Get-ChildItem $dir -File -ErrorAction SilentlyContinue | Where-Object { $_.Length -gt 0 }
$anyZone = $false
foreach ($f in $blocked) {
    $zone = Get-Item -Path $f.FullName -Stream Zone.Identifier -ErrorAction SilentlyContinue
    if ($zone) {
        $anyZone = $true
        Write-Host ("被标记：{0}" -f $f.Name) -ForegroundColor Yellow
    }
}
if (-not $anyZone) { Write-Host '没有 MOTW 标记（正常）' }

Show-Header '结论提示'
Write-Host '· 如果 libusb-1.0.dll / libwinpthread-1.dll 缺失 → 安装包不完整（B0.1.1 及更早确实缺），装 B0.1.2+'
Write-Host '· 如果它们是 32 位 → 系统里有旧的 32 位副本被抢先加载，见文档"故障-0xc000007b.md"第 A 节'
Write-Host '· 如果文件 0 字节 → 被杀软截断，恢复并加排除项后重装'
Write-Host '· 如果全部正常仍报错 → 把本窗口内容整段贴出来'
Write-Host ''
Read-Host '按回车关闭'
