# 把本地源码提交/推送到 GitHub（在你自己的机器上运行）
#
# 为什么给你这个脚本：这台机器到 github.com 的链路不稳定
# （ls-remote 能通，但 fetch 打包下载会在中途断开："early EOF / unpack-objects failed"），
# 所以推送这一步请在网络正常的机器上做。
#
# 用法：
#   powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\push-to-github.ps1
#
# 首次会要求输入 GitHub 用户名和 Personal Access Token（不要用账号密码）。
# 需要 token 的权限：repo（public_repo 即可）。

$ErrorActionPreference = 'Stop'
$root = 'D:\CP16安装及练习文件公开下载版\CP16Plus-Protocol'
Set-Location $root
$url = 'https://github.com/hatsunesenya/cp16_plus.git'

if (-not (Test-Path '.git')) { throw '这里没有 .git，先跑 tools/git-init-commit.ps1' }

$has = git config --get remote.origin.url 2>$null
if ($has) { git remote set-url origin $url } else { git remote add origin $url }

# 1) 先拿到远端的历史（保留你原有的提交与 tag）
Write-Host '[1/4] 拉取远端 main …'
git -c http.postBuffer=52428800 -c http.lowSpeedLimit=1000 -c http.lowSpeedTime=60 fetch origin main
if ($LASTEXITCODE -ne 0) { throw 'fetch 失败：检查网络/代理' }

# 2) 以远端 main 为基线建分支，再把本地这套源码覆盖上去
Write-Host '[2/4] 以 origin/main 为基线 …'
git tag -f local-tree 2>$null | Out-Null
git checkout -B main origin/main
git checkout local-tree -- .
git add -A

# 3) 提交
Write-Host '[3/4] 提交 …'
$msgFile = '.git/COMMIT_MSG.txt'
git commit -q -F $msgFile
git log --oneline -3

# 4) 推送
Write-Host '[4/4] 推送（需要输入用户名 + PAT）…'
git push -u origin main

Write-Host ''
Write-Host '完成。查看：https://github.com/hatsunesenya/cp16_plus' -ForegroundColor Green
