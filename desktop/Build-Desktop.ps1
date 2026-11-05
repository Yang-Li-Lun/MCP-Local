param([switch]$noBuild, [string]$OutputDirectory, [string]$IntermediateDirectory, [string]$DotnetPath = 'dotnet')
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$projectPath = Join-Path $PSScriptRoot 'wpf\McpLocal.Desktop.csproj'
if (-not $noBuild) {
    $sdkVersions = & $DotnetPath --list-sdks
    if ($LASTEXITCODE -ne 0 -or -not ($sdkVersions -match '^10\.0\.\d+ ')) { throw '建置需要 .NET 10 SDK（Windows x64）；只有 Desktop Runtime 無法建置。可用 -DotnetPath 指定已安裝的 SDK。' }
    if (-not $OutputDirectory) { $OutputDirectory = Join-Path $PSScriptRoot 'runtime' }
    $OutputDirectory = [IO.Path]::GetFullPath($OutputDirectory)
    if (-not $IntermediateDirectory) { $IntermediateDirectory = Join-Path $PSScriptRoot 'wpf\obj' }
    $IntermediateDirectory = [IO.Path]::GetFullPath($IntermediateDirectory)
    $pythonPath = Join-Path $PSScriptRoot '..\.venv\Scripts\python.exe'
    $identityTool = Join-Path $PSScriptRoot '..\tools\desktop_build_identity.py'
    $identityPath = Join-Path $IntermediateDirectory 'desktop-build.json'
    & $pythonPath -B $identityTool --output $identityPath
    if ($LASTEXITCODE -ne 0) { throw '無法建立桌面建置識別。' }
    $buildArguments = @('build', $projectPath, '--configuration', 'Release', '--configfile', (Join-Path $PSScriptRoot 'wpf\NuGet.Config'), '--output', $OutputDirectory, '--nologo', '--verbosity', 'quiet')
    if ($IntermediateDirectory) { $buildArguments += ('-p:BaseIntermediateOutputPath=' + [IO.Path]::GetFullPath($IntermediateDirectory).TrimEnd('\') + '\') }
    $buildArguments += ('-p:DesktopBuildManifest=' + $identityPath)
    & $DotnetPath @buildArguments
    if ($LASTEXITCODE -ne 0) { throw "WPF 建置失敗：$LASTEXITCODE" }
    $deployedManifest = Join-Path $OutputDirectory 'desktop-build.json'
    Copy-Item -LiteralPath $identityPath -Destination $deployedManifest
    & $pythonPath -B $identityTool --output $deployedManifest --artifacts $OutputDirectory
    if ($LASTEXITCODE -ne 0) { throw '無法建立桌面部署摘要。' }
}
