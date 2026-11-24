# 初始化本地仓库并提交源码（不推送；推送需要你自己的 GitHub 凭据）
$ErrorActionPreference = 'Stop'
$root = 'D:\CP16安装及练习文件公开下载版\CP16Plus-Protocol'
Set-Location $root
$url = 'https://github.com/hatsunesenya/cp16_plus.git'

if (-not (Test-Path '.git')) { git init -b main | Out-Null }

# 仓库级身份（不动全局配置）
git config user.name  'BD8DQS'
git config user.email 'BD8DQS@163.com'
git config core.autocrlf false
git config core.quotepath false

$has = git config --get remote.origin.url 2>$null
if ($has) { git remote set-url origin $url } else { git remote add origin $url }

git add -A
$n = (git status --short | Measure-Object).Count
"待提交文件数：$n"

if ($n -gt 0) {
    $msg = @'
源码入库：CP16+ β0.1.5（B0.1.5）

本次相对上一版（B0.1.2）的实质改动：

接收 / 解码
- 导频标定改为"未锁定时每秒用最近 4 秒重估"，只接受物理上说得通的解
  （conf>=0.8、0.93<a<1.07、|b|<80Hz），修掉"先开接收后放音导致整场解不出"
- 盲锁段不再依赖 HDR/BCON，也不再只开一次；开段前做 8 点行相位扫描
- 下一个突发还在未来时先用盲锁补空档；段长时间不出字则放弃重对齐（看门狗）
- 新增 covered_to：任何开段不得早于已解区间，消除同一段音频被解两遍

界面
- 行数据发送前按平滑峰值归一化 + 科学计数法序列化
  （原先 4 位小数会把 1e-5 量级的行值全部抹成 0，主界面"缺像素/全黑"）
- 原始瀑布：中心频率/带宽/行率可调，流速固定 1×，频率轴按设备实际采样率换算

采集
- 双声道不再直接平均（避免梳状滤波），自动选更响的一路
- 新增 30 Hz 一阶高通去直流；采集电平/削顶/声道计量

工程
- 安装器：安装前结束残留进程，拷贝失败不再静默（会重试并报错）
- 启动器：比对 /api/ping 的版本，版本不一致时不再复用旧引擎
- VERSIONINFO（公司名 BD8DQS）、图标、免自解压安装包
'@
    Set-Content -Path .git/COMMIT_MSG.txt -Value $msg -Encoding UTF8
    git commit -q -F .git/COMMIT_MSG.txt
}

git log --oneline -3
"" 
"--- 提交内容规模 ---"
git count-objects -vH | Select-String 'count|size-pack|size:'
"--- 仓库根目录 ---"
git ls-tree --name-only HEAD
