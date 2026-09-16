$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$pythonPath = Join-Path $PSScriptRoot '.venv\Scripts\python.exe'
$launcherPath = Join-Path $PSScriptRoot 'connection_cli.py'
foreach ($requiredPath in @($pythonPath, $launcherPath)) {
    if (-not (Test-Path -LiteralPath $requiredPath -PathType Leaf)) {
        throw "Required file not found: $requiredPath"
    }
}
& $pythonPath -B $launcherPath
if ($LASTEXITCODE -ne 0) {
    throw "Connection failed with exit code $LASTEXITCODE. Open connection settings to resolve the reported problem."
}
