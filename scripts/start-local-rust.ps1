param(
    [int]$Port = 8012,
    [string]$Binary = '',
    [string]$ConfigFile = ''
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot

if (!$ConfigFile) {
    $ConfigFile = Join-Path $root '.env'
}

if (!(Test-Path -LiteralPath $ConfigFile)) {
    throw "Model configuration not found: $ConfigFile. Please provide a valid -ConfigFile."
}

# 优先查找 release 二进制，未找到则查找 debug 二进制
if (!$Binary) {
    $relBin = Join-Path $root 'backend-rust/target/release/campus_policy_backend.exe'
    $dbgBin = Join-Path $root 'backend-rust/target/debug/campus_policy_backend.exe'
    if (Test-Path -LiteralPath $relBin) {
        $Binary = $relBin
    } elseif (Test-Path -LiteralPath $dbgBin) {
        $Binary = $dbgBin
    } else {
        # 如果是 Linux/WSL 环境构建可能无 .exe
        $relLinux = Join-Path $root 'backend-rust/target/release/campus_policy_backend'
        if (Test-Path -LiteralPath $relLinux) {
            $Binary = $relLinux
        } else {
            throw 'Rust binary not found. Build it first with: cargo build --manifest-path backend-rust/Cargo.toml --release'
        }
    }
}

$frontendDist = Join-Path $root 'frontend/dist'
if (!(Test-Path -LiteralPath (Join-Path $frontendDist 'index.html'))) {
    throw 'Frontend dist not found. Build the frontend first: npm.cmd --prefix frontend run build'
}

if (Get-NetTCPConnection -State Listen -LocalPort $Port -ErrorAction SilentlyContinue) {
    throw "Port $Port is already in use. Choose another -Port."
}

# 读取 .env 键值并载入进程环境变量（不向终端打印密钥值）
Get-Content -LiteralPath $ConfigFile | ForEach-Object {
    $line = $_.Trim()
    if ($line -and !$line.StartsWith('#') -and $line.Contains('=')) {
        $parts = $line.Split('=', 2)
        $name = $parts[0].Trim()
        $val = $parts[1].Trim()
        if ($val.StartsWith('"') -and $val.EndsWith('"')) {
            $val = $val.Substring(1, $val.Length - 2)
        }
        [System.Environment]::SetEnvironmentVariable($name, $val, 'Process')
    }
}

# 设置本地 Rust 专用独立数据目录与端口配置，绝不影响现有正式目录或试用目录
$dataDir = Join-Path $root '.local-acceptance/rust-data'
if (!(Test-Path -LiteralPath $dataDir)) {
    New-Item -ItemType Directory -Path $dataDir -Force | Out-Null
}

$env:DATA_DIR = $dataDir
$env:FRONTEND_DIST = $frontendDist
$env:PORT = $Port.ToString()
$env:HOST = '127.0.0.1'
if (!$env:INITIAL_ADMIN_PASSWORD) {
    $env:INITIAL_ADMIN_PASSWORD = 'admin'
}

Write-Host "=========================================="
Write-Host "Campus Policy Agent (Rust Backend)"
Write-Host "=========================================="
Write-Host "Binary:      $Binary"
Write-Host "Data Dir:    $env:DATA_DIR"
Write-Host "Frontend:    $env:FRONTEND_DIST"
Write-Host "User URL:    http://127.0.0.1:$Port/"
Write-Host "Admin URL:   http://127.0.0.1:$Port/#/admin"
Write-Host "=========================================="

Push-Location $root
try {
    & $Binary
    if ($LASTEXITCODE -ne 0) {
        throw "Rust backend exited with code $LASTEXITCODE"
    }
} finally {
    Pop-Location
}
