# 把 .ico 写进已经链接好的 exe（RT_ICON + RT_GROUP_ICON）。
#
# 为什么不用 windres：这套 MinGW 的 windres 依赖 libintl-8/zlib1/libzstd 三个 DLL，
# 工具链里没有，跑起来直接 0xC0000135。改用 kernel32 的 UpdateResource，
# 不依赖任何外部工具，还能保留 exe 里已有的其它资源。

param(
    [Parameter(Mandatory = $true)][string]$Ico,
    [Parameter(Mandatory = $true)][string[]]$Exe
)

Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class PeRes {
    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    public static extern IntPtr BeginUpdateResource(string fileName, bool deleteExisting);
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern bool UpdateResource(IntPtr h, IntPtr type, IntPtr name, ushort lang, byte[] data, uint size);
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern bool EndUpdateResource(IntPtr h, bool discard);
}
"@

$RT_ICON = [IntPtr]3
$RT_GROUP_ICON = [IntPtr]14
$LANG = [uint16]2052   # 简体中文；1033 也行，资源查找会回退

$bytes = [IO.File]::ReadAllBytes((Resolve-Path $Ico).Path)
$count = [BitConverter]::ToUInt16($bytes, 4)
if ($count -lt 1) { throw "ico 里没有图像" }

$entries = @()
for ($i = 0; $i -lt $count; $i++) {
    $o = 6 + 16 * $i
    $w = $bytes[$o]
    $h = $bytes[$o + 1]
    $size = [BitConverter]::ToUInt32($bytes, $o + 8)
    $off = [BitConverter]::ToUInt32($bytes, $o + 12)
    $data = New-Object byte[] $size
    [Array]::Copy($bytes, $off, $data, 0, $size)
    $entries += [pscustomobject]@{ W = $w; H = $h; Size = $size; Data = $data }
}

# GRPICONDIR + GRPICONDIRENTRY[]（entry 里的偏移字段换成图标 id）
$grp = New-Object System.IO.MemoryStream
$bw = New-Object System.IO.BinaryWriter($grp)
$bw.Write([uint16]0)
$bw.Write([uint16]1)
$bw.Write([uint16]$count)
for ($i = 0; $i -lt $count; $i++) {
    $e = $entries[$i]
    $bw.Write([byte]$e.W)
    $bw.Write([byte]$e.H)
    $bw.Write([byte]0)          # 调色板数
    $bw.Write([byte]0)          # 保留
    $bw.Write([uint16]1)        # 平面
    $bw.Write([uint16]32)       # 位深
    $bw.Write([uint32]$e.Size)
    $bw.Write([uint16]($i + 1)) # 图标 id
}
$grpBytes = $grp.ToArray()
$bw.Dispose()

# 允许 -Exe a,b,c 或 -Exe a -Exe b 两种写法
$Exe = $Exe | ForEach-Object { $_ -split ',' } | Where-Object { $_ -ne '' }

foreach ($path in $Exe) {
    $full = (Resolve-Path $path).Path
    $h = [PeRes]::BeginUpdateResource($full, $false)
    if ($h -eq [IntPtr]::Zero) {
        Write-Warning "打不开 $full（错误 $([Runtime.InteropServices.Marshal]::GetLastWin32Error())）"
        continue
    }
    try {
        for ($i = 0; $i -lt $count; $i++) {
            $e = $entries[$i]
            $ok = [PeRes]::UpdateResource($h, $RT_ICON, [IntPtr]($i + 1), $LANG, $e.Data, [uint32]$e.Size)
            if (-not $ok) { throw "写 RT_ICON #$($i+1) 失败（$([Runtime.InteropServices.Marshal]::GetLastWin32Error())）" }
        }
        $ok = [PeRes]::UpdateResource($h, $RT_GROUP_ICON, [IntPtr]1, $LANG, $grpBytes, [uint32]$grpBytes.Length)
        if (-not $ok) { throw "写 RT_GROUP_ICON 失败（$([Runtime.InteropServices.Marshal]::GetLastWin32Error())）" }
    }
    finally {
        $done = [PeRes]::EndUpdateResource($h, $false)
        if (-not $done) { Write-Warning "提交资源失败：$full" }
    }
    Write-Host ("   图标 -> {0}（{1} 个尺寸）" -f (Split-Path $full -Leaf), $count)
}
