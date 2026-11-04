param([switch]$noBuild, [switch]$load, [string]$ExecutablePath)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if ($ExecutablePath) {
    if (-not $noBuild) { throw '指定隔離執行檔時須使用 -noBuild。' }
    $ExecutablePath = (Resolve-Path -LiteralPath $ExecutablePath).Path
} else {
    & (Join-Path $PSScriptRoot 'Build-Desktop.ps1') -noBuild:$noBuild
    $ExecutablePath = Join-Path $PSScriptRoot 'runtime\McpLocal.Desktop.exe'
}
$fixturePath = Join-Path ([IO.Path]::GetTempPath()) ('mcp-wpf-isolated-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $fixturePath | Out-Null
$resolvedFixture = (Resolve-Path -LiteralPath $fixturePath).Path
$desktopArguments = @('--fixture', ('"' + $resolvedFixture + '"'))
if ($load) { $desktopArguments += '--fixture-load' }
$desktopProcess = $null
try {
    # This is the explicitly requested interactive test window.
    $desktopProcess = Start-Process -FilePath $ExecutablePath -ArgumentList $desktopArguments -PassThru
    $desktopProcess.WaitForExit()
    if ($desktopProcess.ExitCode -ne 0) { throw "隔離桌面結束代碼：$($desktopProcess.ExitCode)" }
} finally {
    if ($null -eq $desktopProcess -or $desktopProcess.HasExited) {
        $actualPath = (Resolve-Path -LiteralPath $fixturePath).Path
        if ($actualPath -ne $resolvedFixture -or -not ([IO.Path]::GetFileName($actualPath).StartsWith('mcp-wpf-isolated-'))) {
            throw '拒絕清理非本次建立的 fixture。'
        }
        Remove-Item -LiteralPath $actualPath -Recurse -Force
    }
}
