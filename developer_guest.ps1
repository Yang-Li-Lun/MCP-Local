# Runs only inside the disposable VM. McpSpool/McpCwd/McpPath are fixed by the host.
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$utf8 = [Text.UTF8Encoding]::new($false)
[Console]::InputEncoding = $utf8
[Console]::OutputEncoding = $utf8
$env:PATH = $McpPath
$env:PYTHONUTF8 = '1'
$env:PYTHONIOENCODING = 'utf-8'
$env:PYTHONDONTWRITEBYTECODE = '1'
$env:PYTHONNOUSERSITE = '1'
$env:CARGO_HOME = Join-Path $env:USERPROFILE '.cargo'
$env:GOPATH = Join-Path $env:USERPROFILE 'go'
$env:MCP_WORKSPACE = $McpCwd
$env:GIT_CONFIG_NOSYSTEM = '1'
$env:GIT_CONFIG_COUNT = $McpGitRoots.Count.ToString()
for ($gitIndex = 0; $gitIndex -lt $McpGitRoots.Count; $gitIndex++) {
    [Environment]::SetEnvironmentVariable(('GIT_CONFIG_KEY_' + $gitIndex), 'safe.directory', 'Process')
    [Environment]::SetEnvironmentVariable(('GIT_CONFIG_VALUE_' + $gitIndex), $McpGitRoots[$gitIndex], 'Process')
}
[IO.File]::WriteAllText((Join-Path $McpSpool 'ready.json'), '{"ready":true}', $utf8)
try {
    $requestPath = Join-Path $McpSpool 'request.json'
    while (-not [IO.File]::Exists($requestPath)) { Start-Sleep -Milliseconds 50 }
    $request = [IO.File]::ReadAllText($requestPath, $utf8) | ConvertFrom-Json
    $program = "[Console]::OutputEncoding=[Text.UTF8Encoding]::new(); [Console]::InputEncoding=[Text.UTF8Encoding]::new(); `$OutputEncoding=[Console]::OutputEncoding; `$ErrorActionPreference='Stop'; `$ProgressPreference='SilentlyContinue'; Set-Location -LiteralPath `$env:MCP_WORKSPACE; [IO.Directory]::SetCurrentDirectory(`$env:MCP_WORKSPACE); try { & {`n" + [string]$request.command + "`n} } catch { [Console]::WriteLine(`$_.ToString()); exit 1 }"
    $startInfo = [Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = 'C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe'
    $startInfo.Arguments = '-NoLogo -NoProfile -NonInteractive -InputFormat Text -OutputFormat Text -EncodedCommand ' + [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($program))
    $startInfo.WorkingDirectory = $McpCwd
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardInput = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $startInfo.StandardOutputEncoding = $utf8
    $startInfo.StandardErrorEncoding = $utf8
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    [void]$process.Start()
    $output = [IO.StreamWriter]::new((Join-Path $McpSpool 'output.txt'), $false, $utf8)
    $output.AutoFlush = $true
    $buffers = @([char[]]::new(2048), [char[]]::new(2048))
    $readers = @($process.StandardOutput, $process.StandardError)
    $tasks = @($readers[0].ReadAsync($buffers[0],0,2048), $readers[1].ReadAsync($buffers[1],0,2048))
    $inputIndex = 0
    $exitedAt = $null
    try {
        while ($null -ne $tasks[0] -or $null -ne $tasks[1] -or -not $process.HasExited) {
            if ($process.HasExited) {
                if ($null -eq $exitedAt) { $exitedAt = [DateTime]::UtcNow }
                if (([DateTime]::UtcNow - $exitedAt).TotalSeconds -gt 0.5) { break }
            }
            for ($index = 0; $index -lt 2; $index++) {
                if ($null -ne $tasks[$index] -and $tasks[$index].IsCompleted) {
                    $count = $tasks[$index].Result
                    if ($count -eq 0) { $tasks[$index] = $null }
                    else {
                        $output.Write($buffers[$index], 0, $count)
                        $tasks[$index] = $readers[$index].ReadAsync($buffers[$index],0,2048)
                    }
                }
            }
            $inputPath = Join-Path $McpSpool ('stdin-{0:d6}.json' -f $inputIndex)
            if (-not $process.HasExited -and [IO.File]::Exists($inputPath)) {
                $inputValue = [IO.File]::ReadAllText($inputPath,$utf8) | ConvertFrom-Json
                if ($inputValue.PSObject.Properties['eof'] -and $inputValue.eof) { $process.StandardInput.BaseStream.Close() }
                else {
                    $inputBytes = [Convert]::FromBase64String($inputValue.data)
                    $process.StandardInput.BaseStream.Write($inputBytes,0,$inputBytes.Length)
                    $process.StandardInput.BaseStream.Flush()
                }
                [IO.File]::Delete($inputPath)
                $inputIndex++
            }
            Start-Sleep -Milliseconds 30
        }
        $process.WaitForExit()
    } finally { $output.Dispose() }
    [IO.File]::WriteAllText((Join-Path $McpSpool 'result.json'), (@{exit_code=$process.ExitCode}|ConvertTo-Json -Compress), $utf8)
} catch {
    [IO.File]::WriteAllText((Join-Path $McpSpool 'result.json'), '{"exit_code":1,"error":"GUEST_IO_FAILED"}', $utf8)
}
