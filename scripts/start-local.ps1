param(
    [int]$Port = 8011,
    [string]$Python = '',
    [string]$ConfigFile = ''
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$commonDir = git -C $root rev-parse --path-format=absolute --git-common-dir
$mainRoot = Split-Path -Parent $commonDir
if (!$Python) {
    $Python = Join-Path $root '.venv/Scripts/python.exe'
    if (!(Test-Path -LiteralPath $Python)) { $Python = Join-Path $mainRoot '.venv/Scripts/python.exe' }
}
if (!$ConfigFile) {
    $ConfigFile = Join-Path $root '.env'
    if (!(Test-Path -LiteralPath $ConfigFile)) { $ConfigFile = Join-Path $mainRoot '.env' }
}
if (!(Test-Path -LiteralPath $Python)) { throw 'Python environment not found. Pass -Python with an existing interpreter path.' }
if (!(Test-Path -LiteralPath $ConfigFile)) { throw 'Model configuration not found. Pass -ConfigFile with your private .env path.' }
if (!(Test-Path -LiteralPath (Join-Path $root 'frontend/dist/index.html'))) { throw 'Build the frontend first: npm.cmd --prefix frontend run build' }
if (Get-NetTCPConnection -State Listen -LocalPort $Port -ErrorAction SilentlyContinue) { throw "Port $Port is in use. Choose another -Port." }
$env:DATA_DIR = Join-Path $root '.local-acceptance/webui-data'
$env:INITIAL_ADMIN_PASSWORD = 'admin'
Write-Host "Local data: $env:DATA_DIR"
Write-Host "User: http://127.0.0.1:$Port/"
Write-Host "Admin: http://127.0.0.1:$Port/#/admin"
Push-Location $root
try {
    & $Python -c 'import sys; from dotenv import load_dotenv; load_dotenv(sys.argv[1]); sys.path.insert(0, "backend"); import uvicorn; uvicorn.run("app.main:app", host="127.0.0.1", port=int(sys.argv[2]), workers=1)' $ConfigFile $Port
    if ($LASTEXITCODE -ne 0) { throw "Local server exited with code $LASTEXITCODE" }
} finally { Pop-Location }
