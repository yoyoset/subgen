# subgen 交付验证:release 构建(零警告) + 单元测试 + clippy(-D warnings) + 行数预算。
# 把"改完必须全绿才能交付/提交"的门槛从三条手动命令收敛成这一条,防止漏跑某一项。
# 用法: powershell -File scripts/verify.ps1
#
# 注意:这里不用 $ErrorActionPreference='Stop'——PowerShell 5.1 里它和原生命令的
# stderr(2>&1 重定向)互斥,会把 cargo 的正常进度输出当成终止错误。改为显式检查
# $LASTEXITCODE。

function Fail {
    param([string]$Step)
    Write-Host "FAILED: $Step" -ForegroundColor Red
    exit 1
}

function Run-Step {
    param([string]$Name, [scriptblock]$Body)
    Write-Host "`n==> $Name" -ForegroundColor Cyan
    & $Body
    if ($LASTEXITCODE -ne 0) {
        Fail $Name
    }
}

Write-Host 'subgen 交付验证:release 构建(零警告) + test + clippy(-D warnings) + 行数预算' -ForegroundColor Cyan

# 1. release 构建,零警告是硬性门槛(警告一律视为未通过,不能带着警告交付)。
# 用 cmd /c 合并 stdout/stderr,避免 PowerShell 5.1 把 cargo 的 stderr 进度误当错误。
Run-Step 'cargo build --release(零警告)' {
    $output = cmd /c 'cargo build --release 2>&1'
    if ($LASTEXITCODE -ne 0) {
        $output | ForEach-Object { Write-Host "  $_" -ForegroundColor Red }
        Write-Host '构建失败' -ForegroundColor Red
        exit 1
    }
    $warnings = @($output | Select-String -Pattern 'warning:')
    if ($warnings.Count -gt 0) {
        $warnings | ForEach-Object { Write-Host "  $_" -ForegroundColor Yellow }
        Write-Host '构建出现警告,视为未通过' -ForegroundColor Red
        exit 1
    }
    Write-Host '  构建完成,零警告' -ForegroundColor Green
}

# 2. 单元测试全绿。
Run-Step 'cargo test(全绿)' { cargo test }

# 3. clippy 零警告。
Run-Step 'cargo clippy --all-targets -- -D warnings(零警告)' {
    cargo clippy --all-targets -- -D warnings
}

# 4. app.rs 行数预算。
Run-Step 'scripts/check-line-budgets.ps1' {
    powershell -File (Join-Path $PSScriptRoot 'check-line-budgets.ps1')
}

Write-Host "`n全部通过。release 产物:" -ForegroundColor Green
$exe = Join-Path (Join-Path $PSScriptRoot '..') 'target\release\subgen.exe'
if (Test-Path $exe) {
    $size = (Get-Item $exe).Length
    Write-Host "  $exe ($([math]::Round($size / 1MB, 1)) MB)" -ForegroundColor Green
} else {
    Write-Host "  $exe(未找到)" -ForegroundColor Yellow
}
