# install-new.ps1 — 把本目录下新构建的 stayawake.exe 装到系统里
# 需要管理员权限(UAC)。日志写到本目录 install-log.txt。
$ErrorActionPreference = 'Continue'
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$log  = Join-Path $root 'install-log.txt'

function Say($m) {
    $line = "{0}  {1}" -f (Get-Date -Format 'HH:mm:ss'), $m
    try { Add-Content -LiteralPath $log -Value $line -ErrorAction Stop } catch {}
    Write-Host $line
}

try { Set-Content -LiteralPath $log -Value ("=== install {0} ===" -f (Get-Date -Format 'yyyy-MM-dd HH:mm:ss')) -ErrorAction Stop } catch {}

$src = Join-Path $root 'stayawake.exe'
if (-not (Test-Path -LiteralPath $src)) { Say "FAIL 找不到 $src"; exit 1 }
Say ("源文件: {0} ({1} 字节)" -f $src, (Get-Item -LiteralPath $src).Length)

$targets = @(
    'C:\Program Files\stayawake\stayawake.exe',
    (Join-Path $root 'target\release\stayawake.exe')
)

# 1) 停掉正在运行的老版本(否则 exe 被占用, 覆盖会失败)
$procs = @(Get-Process -Name stayawake -ErrorAction SilentlyContinue)
if ($procs.Count -gt 0) {
    Say ("停止 {0} 个运行中的 stayawake" -f $procs.Count)
    $procs | Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 800
} else {
    Say '没有运行中的 stayawake'
}

# 2) 逐个覆盖
foreach ($t in $targets) {
    $dir = Split-Path -Parent $t
    if (-not (Test-Path -LiteralPath $dir)) { Say ("跳过(目录不存在): {0}" -f $t); continue }
    if (Test-Path -LiteralPath $t) {
        $bak = "$t.old"
        Remove-Item -LiteralPath $bak -Force -ErrorAction SilentlyContinue
        try { Rename-Item -LiteralPath $t -NewName (Split-Path -Leaf $bak) -ErrorAction Stop }
        catch { Say ("warn 备份改名失败: {0} -> {1}" -f $t, $_.Exception.Message) }
    }
    try {
        Copy-Item -LiteralPath $src -Destination $t -Force -ErrorAction Stop
        Say ("OK -> {0} ({1} 字节)" -f $t, (Get-Item -LiteralPath $t).Length)
        Remove-Item -LiteralPath "$t.old" -Force -ErrorAction SilentlyContinue
    } catch {
        Say ("FAIL -> {0}: {1}" -f $t, $_.Exception.Message)
    }
}

# 3) 刷新程序目录里的图标 (同时顺带验证新 exe 能启动)
$icoTarget = 'C:\Program Files\stayawake\stayawake.ico'
if (Test-Path -LiteralPath (Split-Path -Parent $icoTarget)) {
    try {
        & $src --write-ico $icoTarget | Out-Null
        if (Test-Path -LiteralPath $icoTarget) { Say ("OK 已刷新图标 ({0} 字节)" -f (Get-Item -LiteralPath $icoTarget).Length) }
        else { Say 'warn 图标未生成' }
    } catch { Say ("warn 刷新图标失败: {0}" -f $_.Exception.Message) }
}

# 4) 启动新版本
try {
    Start-Process -FilePath 'C:\Program Files\stayawake\stayawake.exe' -ErrorAction Stop
    Say 'OK 已启动新版本'
} catch { Say ("warn 启动失败: {0}" -f $_.Exception.Message) }

Say 'DONE'
