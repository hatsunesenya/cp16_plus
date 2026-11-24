# 推送到 GitHub（最后一步，需要你的凭据）
#
# 现状：本地已经在 origin/main 之上做好了提交，是干净的快进（fast-forward）：
#     ef33047..6c5fc38  main -> main
#
# 只差认证。两种方式任选：
#
#   A. 直接运行本脚本，按提示输入 GitHub 用户名 + Personal Access Token
#      （Token 权限勾 repo / public_repo；GitHub 已不支持密码推送）
#
#   B. 用 Git Credential Manager：先 `git config --global credential.helper manager`
#      再运行本脚本，会弹浏览器登录
#
# 运行：
#   powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\push-to-github.ps1

$ErrorActionPreference = 'Stop'
Set-Location 'D:\CP16安装及练习文件公开下载版\CP16Plus-Protocol'

Write-Host '== 待推送的提交 ==' -ForegroundColor Cyan
git log --oneline -2

Write-Host ''
Write-Host '== 将要上传的内容（顶层）==' -ForegroundColor Cyan
git ls-tree --name-only HEAD

Write-Host ''
Write-Host '== 开始推送 ==' -ForegroundColor Cyan
Write-Host '提示：用户名填 hatsunesenya，密码处粘贴 Personal Access Token（不会显示）'
git push -u origin main

if ($LASTEXITCODE -eq 0) {
    Write-Host ''
    Write-Host '推送成功：https://github.com/hatsunesenya/cp16_plus' -ForegroundColor Green
} else {
    Write-Host ''
    Write-Host '推送失败。常见原因：' -ForegroundColor Yellow
    Write-Host '  · Token 没有 repo 权限 / 已过期'
    Write-Host '  · 网络中断：本机到 github.com 不稳，重试几次，或挂代理后再试'
    Write-Host '  · 远端被别处更新了：先 git fetch origin main，再重做提交'
}
