# 给 exe 写入 VERSIONINFO（公司名用呼号 BD8DQS）
#
# 为什么自己拼二进制：这套工具链的 windres 缺 DLL 跑不起来，装 Windows SDK 又太重。
# VERSIONINFO 的结构不算复杂，直接按 VS_VERSIONINFO 规范拼好，用 kernel32 的
# UpdateResource 写进 RT_VERSION 即可。写完立刻回读校验。

param(
    [Parameter(Mandatory = $true)][string[]]$Exe,
    [string]$Version = '0.1.2.0',
    [string]$Company = 'BD8DQS',
    [string]$Product = 'CP16+ 频谱书写',
    [string]$Copyright = 'Copyright (C) 2026 BD8DQS. MIT License.',
    [string]$LangId = '040904B0'
)

$ErrorActionPreference = 'Stop'

Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class PeVer {
    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    public static extern IntPtr BeginUpdateResource(string fileName, bool deleteExisting);
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern bool UpdateResource(IntPtr h, IntPtr type, IntPtr name, ushort lang, byte[] data, uint size);
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern bool EndUpdateResource(IntPtr h, bool discard);
}
"@

$RT_VERSION = [IntPtr]16
$LANG = [uint16]0x0409

function Pad4([System.IO.MemoryStream]$ms) {
    while (($ms.Length % 4) -ne 0) { $ms.WriteByte(0) }
}

# 按 VS_VERSIONINFO 规则拼一个节点
function New-Node {
    param(
        [string]$Key,
        [int]$Type,          # 0 = binary, 1 = text
        [byte[]]$Value,
        [byte[]]$Children
    )
    $ms = New-Object System.IO.MemoryStream
    $bw = New-Object System.IO.BinaryWriter($ms)
    $bw.Write([uint16]0)
    $bw.Write([uint16]0)
    $bw.Write([uint16]$Type)
    $bw.Write([System.Text.Encoding]::Unicode.GetBytes($Key + [char]0))
    Pad4 $ms
    if ($Value -and $Value.Length -gt 0) {
        $ms.Write($Value, 0, $Value.Length)
        Pad4 $ms
    }
    if ($Children -and $Children.Length -gt 0) {
        $ms.Write($Children, 0, $Children.Length)
        Pad4 $ms
    }
    $bw.Flush()
    $bytes = $ms.ToArray()
    $total = [uint16]$bytes.Length
    # wValueLength：文本节点按"字数"，二进制节点按字节数
    $vlen = if ($Type -eq 1) { [uint16]($(if ($Value) { $Value.Length / 2 } else { 0 })) } else { [uint16]$(if ($Value) { $Value.Length } else { 0 }) }
    [BitConverter]::GetBytes($total).CopyTo($bytes, 0)
    [BitConverter]::GetBytes($vlen).CopyTo($bytes, 2)
    return $bytes
}

function New-String([string]$Name, [string]$Text) {
    $val = [System.Text.Encoding]::Unicode.GetBytes($Text + [char]0)
    return New-Node -Key $Name -Type 1 -Value $val -Children $null
}

# --- 版本四段 ---
$parts = @($Version.Split('.') + @('0', '0', '0', '0')) | Select-Object -First 4
$msv = ([uint32]$parts[0] -shl 16) -bor [uint32]$parts[1]
$lsv = ([uint32]$parts[2] -shl 16) -bor [uint32]$parts[3]

# VS_FIXEDFILEINFO（13 个 DWORD）
$ffi = New-Object System.IO.MemoryStream
$fw = New-Object System.IO.BinaryWriter($ffi)
$fw.Write([convert]::ToUInt32('FEEF04BD', 16))   # signature
$fw.Write([uint32]0x00010000)   # struc version
$fw.Write([uint32]$msv)         # file version MS
$fw.Write([uint32]$lsv)         # file version LS
$fw.Write([uint32]$msv)         # product version MS
$fw.Write([uint32]$lsv)         # product version LS
$fw.Write([uint32]0x3F)         # flags mask
$fw.Write([uint32]0x00)         # flags
$fw.Write([uint32]0x00040004)   # VOS_NT_WINDOWS32
$fw.Write([uint32]0x00000001)   # VFT_APP
$fw.Write([uint32]0x00000000)   # subtype
$fw.Write([uint32]0x00000000)   # date MS
$fw.Write([uint32]0x00000000)   # date LS
$fw.Flush()
$ffiBytes = $ffi.ToArray()
$fw.Dispose()

foreach ($path in ($Exe | ForEach-Object { $_ -split ',' } | Where-Object { $_ -ne '' })) {
    $full = (Resolve-Path $path).Path
    $file = [System.IO.Path]::GetFileName($full)

    $desc = switch -Regex ($file) {
        '^cp16plus\.exe$' { 'CP16+ 引擎（采集 / 编解码 / 发射 / 本地服务）' }
        '^cp16plus-setup' { 'CP16+ 安装程序' }
        '^卸载' { 'CP16+ 卸载程序' }
        default { 'CP16+ 频谱书写' }
    }

    $strings = @(
        (New-String 'CompanyName' $Company),
        (New-String 'FileDescription' $desc),
        (New-String 'FileVersion' $Version),
        (New-String 'InternalName' ([System.IO.Path]::GetFileNameWithoutExtension($file))),
        (New-String 'LegalCopyright' $Copyright),
        (New-String 'OriginalFilename' $file),
        (New-String 'ProductName' $Product),
        (New-String 'ProductVersion' $Version)
    )
    $strBlob = [byte[]]::new(0)
    $msJoin = New-Object System.IO.MemoryStream
    foreach ($sb in $strings) { $msJoin.Write($sb, 0, $sb.Length) }
    $strBlob = $msJoin.ToArray()

    $table = New-Node -Key $LangId -Type 1 -Value $null -Children $strBlob
    $sfi = New-Node -Key 'StringFileInfo' -Type 1 -Value $null -Children $table
    $trans = [BitConverter]::GetBytes([uint32]0x04B00409)   # codepage 0x04B0, lang 0x0409
    $transNode = New-Node -Key 'Translation' -Type 0 -Value $trans -Children $null
    $vfi = New-Node -Key 'VarFileInfo' -Type 1 -Value $null -Children $transNode

    $all = New-Object System.IO.MemoryStream
    $all.Write($sfi, 0, $sfi.Length)
    $all.Write($vfi, 0, $vfi.Length)
    $root = New-Node -Key 'VS_VERSION_INFO' -Type 0 -Value $ffiBytes -Children $all.ToArray()

    $h = [PeVer]::BeginUpdateResource($full, $false)
    if ($h -eq [IntPtr]::Zero) {
        Write-Warning "打不开 $file（错误 $([Runtime.InteropServices.Marshal]::GetLastWin32Error())）"
        continue
    }
    $ok = $false
    try {
        $ok = [PeVer]::UpdateResource($h, $RT_VERSION, [IntPtr]1, $LANG, $root, [uint32]$root.Length)
        if (-not $ok) { throw "UpdateResource 失败（$([Runtime.InteropServices.Marshal]::GetLastWin32Error())）" }
    } finally {
        [void][PeVer]::EndUpdateResource($h, (-not $ok))
    }

    # 回读校验
    $vi = (Get-Item $full).VersionInfo
    $bad = @()
    if ($vi.CompanyName -ne $Company) { $bad += "CompanyName=$($vi.CompanyName)" }
    if ($vi.ProductName -ne $Product) { $bad += "ProductName=$($vi.ProductName)" }
    if ($vi.FileVersion -ne $Version) { $bad += "FileVersion=$($vi.FileVersion)" }
    if ($bad.Count -gt 0) {
        Write-Warning ("{0}: 回读不一致 -> {1}" -f $file, ($bad -join '; '))
    } else {
        Write-Host ("   VERSIONINFO -> {0}  ({1} / {2} / {3})" -f $file, $Company, $Product, $Version)
    }
}
