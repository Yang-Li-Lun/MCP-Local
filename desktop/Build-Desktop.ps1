param([switch]$noBuild, [string]$OutputDirectory, [string]$IntermediateDirectory)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$projectPath = Join-Path $PSScriptRoot 'wpf\McpLocal.Desktop.csproj'
if (-not $noBuild) {
    if (-not $OutputDirectory) { $OutputDirectory = Join-Path $PSScriptRoot 'runtime' }
    $buildArguments = @('build', $projectPath, '--configuration', 'Release', '--configfile', (Join-Path $PSScriptRoot 'wpf\NuGet.Config'), '--output', $OutputDirectory, '--nologo', '--verbosity', 'quiet')
    if ($IntermediateDirectory) { $buildArguments += ('-p:BaseIntermediateOutputPath=' + [IO.Path]::GetFullPath($IntermediateDirectory).TrimEnd('\') + '\') }
    & dotnet @buildArguments
    if ($LASTEXITCODE -ne 0) { throw "WPF 建置失敗：$LASTEXITCODE" }
}
