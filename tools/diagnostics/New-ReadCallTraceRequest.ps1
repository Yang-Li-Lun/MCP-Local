param([switch]$ValidateOnly)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$projectRoot = Split-Path (Split-Path $PSScriptRoot -Parent) -Parent
$temporaryRoot = Join-Path $projectRoot '.test-tmp'
$requestPath = Join-Path $temporaryRoot 'read-host-trace.request'
if ($ValidateOnly) {
    [ordered]@{ request_present = (Test-Path -LiteralPath $requestPath); created = $false; restarts_service = $false }
    return
}
foreach ($path in @($projectRoot, $temporaryRoot)) {
    if ((Test-Path -LiteralPath $path) -and
        ((Get-Item -LiteralPath $path -Force).Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw '診斷目錄不可為連結。'
    }
}
[IO.Directory]::CreateDirectory($temporaryRoot) | Out-Null
$directoryName = 'read-host-trace-' + [Guid]::NewGuid().ToString('N')
$stream = [IO.File]::Open($requestPath, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
try {
    $bytes = [Text.Encoding]::ASCII.GetBytes($directoryName)
    $stream.Write($bytes, 0, $bytes.Length)
} finally {
    $stream.Dispose()
}
[ordered]@{ created = $true; restarts_service = $false; diagnostic_directory = (Join-Path $temporaryRoot $directoryName) }
