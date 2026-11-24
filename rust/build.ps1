# CP16+ Rust 端构建脚本
#
# 环境约定（都能换，但换了要改这里）：
#   · 工具链在 .toolchain\rustup（rustup 装好的 stable-x86_64-pc-windows-gnu）
#   · CARGO_HOME / TMP 都指到工程内（不往用户目录里写东西）
#   · 只用 rust-lld 链接（不依赖 gcc / binutils）
#
# 关于盘符：早期版本因为工程路径含中文，用 `subst X:` 把工程映射成纯英文路径再构建。
# 那个映射是脚本自己建的，**已经彻底取消**——现在就地构建（rust-lld 能处理中文路径），
# 脚本不会再创建任何虚拟盘符。若哪天某台机器上 rust-lld 处理中文路径失败，
# 请手动处理并在 docs/porting.md 记录，不要把它写回脚本默认行为。

param(
    [switch]$Release,
    [switch]$SkipGolden,
    [switch]$Audio,
    [switch]$Pack
)

$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot          # CP16Plus-Protocol
$work = $root

# 工具链：优先用工程内的 .toolchain，其次用系统安装的
# stable-x86_64-pc-windows-gnu（GitHub 上 clone 下来的人多半是后者）。
$tc = "$work\.toolchain\rustup\toolchains\stable-x86_64-pc-windows-gnu"
$local_tc = Test-Path "$tc\bin\rustc.exe"
if (-not $local_tc) {
    $sys = $null
    if (Get-Command rustup -ErrorAction SilentlyContinue) {
        try { $sys = (& rustup run stable-x86_64-pc-windows-gnu rustc --print sysroot).Trim() } catch {}
    }
    if ($sys -and (Test-Path "$sys\bin\rustc.exe")) { $tc = $sys }
}
if (-not (Test-Path "$tc\bin\rustc.exe")) {
    Write-Host '未找到 Rust 工具链。安装 x86_64-pc-windows-gnu 目标：'
    Write-Host '  rustup toolchain install stable-x86_64-pc-windows-gnu'
    throw '缺少工具链'
}

$lld = "$tc\lib\rustlib\x86_64-pc-windows-gnu\bin\rust-lld.exe"
if ($local_tc) {
    $env:CARGO_HOME = "$work\tmp\cargo-home"
    $env:RUSTUP_HOME = "$work\.toolchain\rustup"
}
$env:RUSTC = "$tc\bin\rustc.exe"
$env:RUSTFLAGS = "-C linker=$lld -C link-self-contained=yes"
$env:TMP = "$work\tmp"; $env:TEMP = "$work\tmp"
$env:CARGO_TARGET_DIR = "$work\rust\target"
if (-not $Audio) { $env:CARGO_NET_OFFLINE = 'true' }
$env:PATH = "$tc\lib\rustlib\x86_64-pc-windows-gnu\bin;$tc\bin;$env:PATH"
New-Item -ItemType Directory -Force -Path "$work\tmp\cargo-home" | Out-Null

Push-Location "$work\rust"
try {
    $cfg = @('build')
    if ($Release) { $cfg += '--release' }
    if ($Audio) { $cfg += @('--features', 'audio') }
    Write-Host '== cargo build =='
    & "$tc\bin\cargo.exe" @cfg
    if ($LASTEXITCODE -ne 0) { throw 'build failed' }

    Write-Host '== cargo test =='
    & "$tc\bin\cargo.exe" test -q
    if ($LASTEXITCODE -ne 0) { throw 'test failed' }

    # 界面脚本语法自检：一句 SyntaxError 就会让整页按钮全死（真踩过），
    # 所以构建时把内嵌脚本抠出来过一遍 node --check。没有 node 就跳过。
    Write-Host '== 界面脚本语法检查 =='
    $uiPath = "$work\rust\crates\cli\src\ui.html"
    $node = $null
    # CP16P_NODE 可以显式指定（有些环境 node 不在 PATH 也不在默认目录）
    if ($env:CP16P_NODE -and (Test-Path $env:CP16P_NODE)) { $node = $env:CP16P_NODE }
    foreach ($cand in @("$env:ProgramFiles\nodejs\node.exe", "$env:LOCALAPPDATA\Programs\nodejs\node.exe")) {
        if ($node) { break }
        if (Test-Path $cand) { $node = $cand; break }
    }
    if (-not $node) { $node = (Get-Command node -ErrorAction SilentlyContinue).Source }
    if ($node -and (Test-Path $uiPath)) {
        $html = Get-Content $uiPath -Raw
        $i = $html.IndexOf('<script>'); $j = $html.LastIndexOf('</script>')
        if ($i -ge 0 -and $j -gt $i) {
            $js = $html.Substring($i + 8, $j - $i - 8)
            $jsFile = "$work\tmp\ui-check.js"
            New-Item -ItemType Directory -Force -Path "$work\tmp" | Out-Null
            [System.IO.File]::WriteAllText($jsFile, $js, (New-Object System.Text.UTF8Encoding($false)))
            & $node --check $jsFile
            if ($LASTEXITCODE -ne 0) { throw '界面脚本有语法错误（node --check 失败）' }
            Write-Host '   ok'
        }
    } else {
        Write-Host '   （没找到 node，跳过）'
    }

    # 图标：链接完成后用 UpdateResource 把 .ico 写进三个 exe
    Write-Host '== 写入图标 =='
    $stamp = "$work\tools\stamp_icon.ps1"
    if (Test-Path $stamp) {
        & powershell -NoProfile -ExecutionPolicy Bypass -File $stamp `
            -Ico "$work\rust\assets\cp16plus.ico" `
            -Exe "$work\rust\target\release\cp16plus.exe,$work\rust\target\release\cp16plus-app.exe,$work\rust\target\release\cp16plus-setup.exe"
        if ($LASTEXITCODE -ne 0) { throw '写图标失败' }
    } else {
        Write-Host '   （没有 stamp_icon.ps1，跳过）'
    }

    # VERSIONINFO：公司名用呼号 BD8DQS，版本取 Cargo.toml 的 workspace 版本
    Write-Host '== 写入版本信息 =='
    $verScript = "$work\tools\stamp_version.ps1"
    if (Test-Path $verScript) {
        $cargoVer = '0.1.2'
        $cargoToml = Get-Content "$work\rust\Cargo.toml" -Raw
        if ($cargoToml -match 'version\s*=\s*"([0-9]+\.[0-9]+\.[0-9]+)"') { $cargoVer = $matches[1] }
        $vi = "$cargoVer.0"
        & powershell -NoProfile -ExecutionPolicy Bypass -File $verScript `
            -Exe "$work\rust\target\release\cp16plus.exe,$work\rust\target\release\cp16plus-app.exe,$work\rust\target\release\cp16plus-setup.exe" `
            -Version $vi -Company 'BD8DQS'
        if ($LASTEXITCODE -ne 0) { throw '写版本信息失败' }
    } else {
        Write-Host '   （没有 stamp_version.ps1，跳过）'
    }

    if (-not $SkipGolden) {
        Write-Host '== golden 对拍 =='
        $exe = if ($Release) { "$work\rust\target\release\cp16plus.exe" } else { "$work\rust\target\debug\cp16plus.exe" }
        & $exe golden "$work\tests\golden"
        if ($LASTEXITCODE -ne 0) { throw 'golden failed' }
    }

    if ($Pack) {
        # 发行物 = 一个目录、两个 exe（界面是引擎内嵌的一页，浏览器应用窗口显示）
        $rel = if ($Release) { 'release' } else { 'debug' }
        $src = "$work\rust\target\$rel"
        # 命名规范：cp16plus-<版本号>-<YYMMDD>
        $ver = 'B0.1.5'
        $tag = "cp16plus-$ver-" + (Get-Date -Format 'yyMMdd')
        $out = "$work\dist\$tag"
        Write-Host "== 打包 -> $out =="
        # 先清空输出目录：以前从 dist 里跑过引擎，会在里面留下 settings.json / qso.json 之类的运行垃圾
        if (Test-Path $out) { Remove-Item $out -Recurse -Force }
        New-Item -ItemType Directory -Force -Path "$out\examples" | Out-Null
        # 再清掉上一次发行的 exe，避免旧文件名留在目录里（曾出现过 cp16plus-cli.exe 混在里面）
        Remove-Item "$out\*.exe" -Force -ErrorAction SilentlyContinue
        Copy-Item "$src\cp16plus-app.exe" "$out\CP16+ $ver.exe" -Force
        Copy-Item "$src\cp16plus.exe"     "$out\cp16plus.exe" -Force
        # 电台 CAT 用的 Hamlib（LGPL，动态链接；独立文件，用户可自行替换）
        Copy-Item "$work\rust\third_party\hamlib\libhamlib-4.dll" "$out\libhamlib-4.dll" -Force
        # Hamlib 自己的依赖：不一起发的话，没装 JTDX 的机器会报"找不到 DLL"或 0xc000007b
        Copy-Item "$work\rust\third_party\hamlib\libusb-1.0.dll"     "$out\libusb-1.0.dll" -Force
        Copy-Item "$work\rust\third_party\hamlib\libwinpthread-1.dll" "$out\libwinpthread-1.dll" -Force
        Copy-Item "$work\rust\third_party\hamlib\COPYING.LIB"     "$out\COPYING.LIB-hamlib.txt" -Force
        Copy-Item "$work\rust\assets\cp16plus.ico"                 "$out\cp16plus.ico" -Force
        if (Test-Path "$work\docs\诊断-启动失败.ps1") {
            Copy-Item "$work\docs\诊断-启动失败.ps1" "$out\诊断-启动失败.ps1" -Force
        }
        if (Test-Path "$work\docs\使用说明.md") { Copy-Item "$work\docs\使用说明.md" "$out\使用说明.md" -Force }
        if (Test-Path "$work\dist\examples")      { Copy-Item "$work\dist\examples\*" "$out\examples\" -Force }
        $zip = "$work\dist\$tag.zip"
        if (Test-Path $zip) { Remove-Item $zip -Force }
        Compress-Archive -Path "$out\*" -DestinationPath $zip -Force
        $mb = [math]::Round((Get-Item $zip).Length / 1MB, 2)
        Write-Host "   $zip（$mb MB）"

        # ---- 单文件安装包：setup.exe 尾部追加载荷记录 ----
        # 记录格式：u32 名字长度 | 名字(UTF-8) | u64 内容长度 | 内容
        # 尾部：u64 载荷长度 | "CP16PAY1"
        Write-Host '== 安装包 =='
        $payloadItems = @(
            @{ name = "CP16+ $ver.exe"; path = "$out\CP16+ $ver.exe" },
            @{ name = 'cp16plus.exe';     path = "$out\cp16plus.exe" },
            @{ name = '使用说明.md';       path = "$out\使用说明.md" },
            @{ name = 'libhamlib-4.dll';  path = "$out\libhamlib-4.dll" },
            @{ name = 'libusb-1.0.dll';   path = "$out\libusb-1.0.dll" },
            @{ name = 'libwinpthread-1.dll'; path = "$out\libwinpthread-1.dll" },
            @{ name = 'cp16plus.ico';      path = "$out\cp16plus.ico" },
            @{ name = '诊断-启动失败.ps1';   path = "$out\诊断-启动失败.ps1" },
            @{ name = 'COPYING.LIB-hamlib.txt'; path = "$out\COPYING.LIB-hamlib.txt" },
            @{ name = 'examples/示例-带同步帧8字.wav'; path = "$out\examples\示例-带同步帧8字.wav" },
            @{ name = 'examples/示例-长报文25字.wav';  path = "$out\examples\示例-长报文25字.wav" }
        ) | Where-Object { Test-Path $_.path }

        $ms = New-Object System.IO.MemoryStream
        $bw = New-Object System.IO.BinaryWriter($ms)
        foreach ($it in $payloadItems) {
            $nameBytes = [System.Text.Encoding]::UTF8.GetBytes($it.name)
            $data = [System.IO.File]::ReadAllBytes($it.path)
            $bw.Write([uint32]$nameBytes.Length)
            $bw.Write($nameBytes)
            $bw.Write([uint64]$data.Length)
            $bw.Write($data)
        }
        $bw.Flush()
        $payload = $ms.ToArray()
        $bw.Dispose()

        $setupExe = "$work\dist\$tag-安装包.exe"
        if (Test-Path $setupExe) { Remove-Item $setupExe -Force }
        Copy-Item "$src\cp16plus-setup.exe" $setupExe -Force
        $fs = [System.IO.File]::Open($setupExe, 'Append')
        try {
            $fs.Write($payload, 0, $payload.Length)
            $tailMs = New-Object System.IO.MemoryStream
            $tw = New-Object System.IO.BinaryWriter($tailMs)
            $tw.Write([uint64]$payload.Length)
            $tw.Write([System.Text.Encoding]::ASCII.GetBytes('CP16PAY1'))
            $tw.Flush()
            $tail = $tailMs.ToArray()
            $tw.Dispose()
            $fs.Write($tail, 0, $tail.Length)
        } finally {
            $fs.Close()
        }
        # 免自解压的"文件夹安装"包：安装程序不带载荷，从同目录复制文件安装。
        # 有些杀软对"自解压 -> 释放 -> 执行"特别敏感，这个包没有那一步。
        $stage = "$work\tmp\setup-folder"
        if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
        New-Item -ItemType Directory -Force $stage | Out-Null
        Copy-Item "$out\*" $stage -Recurse -Force
        Copy-Item "$src\cp16plus-setup.exe" "$stage\安装程序.exe" -Force
        $folderZip = "$work\dist\$tag-安装包-免自解压.zip"
        Remove-Item $folderZip -Force -ErrorAction SilentlyContinue
        Compress-Archive -Path "$stage\*" -DestinationPath $folderZip -CompressionLevel Optimal -Force
        Write-Host "   免自解压安装包 -> $folderZip"
        Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue

        $smb = [math]::Round((Get-Item $setupExe).Length / 1MB, 2)
        Write-Host "   $setupExe（$smb MB，$($payloadItems.Count) 个文件）"
    }
} finally {
    Pop-Location
}
