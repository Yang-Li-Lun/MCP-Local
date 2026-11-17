# Windows Sandbox LogonCommand adapter. The Rust guard supplies $Mcp as data.
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$utf8 = [Text.UTF8Encoding]::new($false)
$windowsPowerShell = 'C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe'
$exitResult = '{"exit_code":1,"error":"GUEST_IO_FAILED"}'
$process = $null
$started = $false
$output = $null

function PublishStatus([string]$name, [string]$json) {
    $staging = Join-Path $Mcp.spool ('guest-' + [Guid]::NewGuid().ToString('N') + '.json')
    try {
        [IO.File]::WriteAllText($staging, $json, $utf8)
        [IO.File]::Move($staging, (Join-Path $Mcp.spool $name))
    } finally {
        [IO.File]::Delete($staging)
    }
}

function ReadPacket([string]$path, [int]$maximum) {
    try {
        $stream = [IO.File]::Open($path, 'Open', 'Read', 'Read')
    } catch [IO.IOException] {
        # A host publisher may still hold the handle after the atomic rename.
        if (($_.Exception.HResult -band 0xffff) -in 2, 3, 32, 33) { return $null }
        throw
    }
    try {
        if ($stream.Length -lt 1 -or $stream.Length -gt $maximum) { throw 'Invalid packet length' }
        $reader = [IO.StreamReader]::new($stream, $utf8, $false)
        try { $reader.ReadToEnd() | ConvertFrom-Json }
        finally { $reader.Dispose() }
    } finally {
        $stream.Dispose()
    }
}

try {
    if (-not [IO.Directory]::Exists($Mcp.cwd) -or -not [IO.Directory]::Exists($Mcp.spool)) {
        throw 'Guest directories unavailable'
    }
    [Console]::InputEncoding = $utf8
    [Console]::OutputEncoding = $utf8
    $env:PATH = $Mcp.path
    $env:PYTHONUTF8 = '1'
    $env:PYTHONIOENCODING = 'utf-8'
    $env:PYTHONDONTWRITEBYTECODE = '1'
    $env:PYTHONNOUSERSITE = '1'
    $env:CARGO_HOME = Join-Path $env:USERPROFILE '.cargo'
    $env:GOPATH = Join-Path $env:USERPROFILE 'go'
    $env:MCP_WORKSPACE = $Mcp.cwd
    $env:GIT_CONFIG_NOSYSTEM = '1'
    $env:GIT_CONFIG_COUNT = @($Mcp.git_roots).Count.ToString()
    for ($index = 0; $index -lt @($Mcp.git_roots).Count; $index++) {
        [Environment]::SetEnvironmentVariable("GIT_CONFIG_KEY_$index", 'safe.directory', 'Process')
        [Environment]::SetEnvironmentVariable("GIT_CONFIG_VALUE_$index", $Mcp.git_roots[$index], 'Process')
    }

    PublishStatus 'ready.json' '{"ready":true}'
    $requestPath = Join-Path $Mcp.spool 'request.json'
    $request = ReadPacket $requestPath 65536
    while ($null -eq $request) {
        Start-Sleep -Milliseconds 50
        $request = ReadPacket $requestPath 65536
    }
    if (@($request.PSObject.Properties).Count -ne 1 -or $request.command -isnot [string] -or
        [string]::IsNullOrWhiteSpace($request.command) -or $request.command.Contains([char]0)) {
        throw 'Invalid request'
    }
    $prefix = @'
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
[Console]::InputEncoding = [Console]::OutputEncoding
$OutputEncoding = [Console]::OutputEncoding
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
Set-Location -LiteralPath $env:MCP_WORKSPACE
[IO.Directory]::SetCurrentDirectory($env:MCP_WORKSPACE)
try { & {
'@
    $suffix = @'
} } catch { [Console]::WriteLine($_.ToString()); exit 1 }
'@
    $command = $prefix + "`n" + $request.command + "`n" + $suffix
    $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($command))
    $startInfo = [Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $windowsPowerShell
    $startInfo.Arguments = '-NoLogo -NoProfile -NonInteractive -InputFormat Text -OutputFormat Text -EncodedCommand ' + $encoded
    $startInfo.WorkingDirectory = $Mcp.cwd
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardInput = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $startInfo.StandardOutputEncoding = $utf8
    $startInfo.StandardErrorEncoding = $utf8
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    $started = $process.Start()
    if (-not $started) { throw 'Guest process unavailable' }

    $channel = [IO.File]::Open((Join-Path $Mcp.spool 'output.txt'), 'CreateNew', 'Write', 'Read')
    $output = [IO.StreamWriter]::new($channel, $utf8)
    $output.AutoFlush = $true
    $outputBytes = 0
    $readers = @($process.StandardOutput, $process.StandardError)
    $buffers = @([char[]]::new(2048), [char[]]::new(2048))
    $reads = @($readers[0].ReadAsync($buffers[0], 0, 2048), $readers[1].ReadAsync($buffers[1], 0, 2048))
    $stdin = $process.StandardInput.BaseStream
    $stdinClosed = $false
    $number = 0L
    $pendingWrite = $null
    $flushing = $false
    $closeAfterWrite = $false
    $inputPath = $null
    $exitedAt = $null
    while ($null -ne $reads[0] -or $null -ne $reads[1] -or -not $process.HasExited) {
        if ($process.HasExited) {
            if ($null -eq $exitedAt) { $exitedAt = [DateTime]::UtcNow }
            if (([DateTime]::UtcNow - $exitedAt).TotalMilliseconds -ge 500) { break }
        }
        for ($index = 0; $index -lt 2; $index++) {
            if ($null -eq $reads[$index] -or -not $reads[$index].IsCompleted) { continue }
            $count = $reads[$index].GetAwaiter().GetResult()
            if ($count -eq 0) {
                $reads[$index] = $null
                continue
            }
            $outputBytes += $utf8.GetByteCount($buffers[$index], 0, $count)
            if ($outputBytes -gt 1MB) {
                $exitResult = '{"exit_code":1,"error":"GUEST_OUTPUT_LIMIT"}'
                throw 'Guest output limit'
            }
            $output.Write($buffers[$index], 0, $count)
            $reads[$index] = $readers[$index].ReadAsync($buffers[$index], 0, 2048)
        }
        if (-not $process.HasExited -and -not $stdinClosed) {
            if ($null -ne $pendingWrite -and $pendingWrite.IsCompleted) {
                [void]$pendingWrite.GetAwaiter().GetResult()
                if ($flushing) {
                    [IO.File]::Delete($inputPath)
                    $number++
                    $pendingWrite = $null
                    if ($closeAfterWrite) {
                        $stdin.Dispose()
                        $stdinClosed = $true
                    }
                } else {
                    $pendingWrite = $stdin.FlushAsync()
                    $flushing = $true
                }
            }
            if ($null -eq $pendingWrite -and -not $stdinClosed) {
                $inputPath = Join-Path $Mcp.spool ('stdin-{0:d6}.json' -f $number)
                $packet = ReadPacket $inputPath 8192
                if ($null -ne $packet) {
                    $hasEof = $null -ne $packet.PSObject.Properties['eof']
                    $expected = if ($hasEof) { 2 } else { 1 }
                    if (@($packet.PSObject.Properties).Count -ne $expected -or $packet.data -isnot [string] -or
                        ($hasEof -and ($packet.eof -isnot [bool] -or -not $packet.eof))) {
                        throw 'Invalid input packet'
                    }
                    $bytes = [Convert]::FromBase64String($packet.data)
                    if ($bytes.Length -gt 4096) { throw 'Guest input limit' }
                    $closeAfterWrite = $hasEof
                    $pendingWrite = $stdin.WriteAsync($bytes, 0, $bytes.Length)
                    $flushing = $false
                }
            }
        }
        Start-Sleep -Milliseconds 25
    }
    $process.WaitForExit()
    $output.Dispose()
    $output = $null
    $exitResult = @{exit_code = $process.ExitCode} | ConvertTo-Json -Compress
} catch {
    # Only fixed status codes cross the boundary. Errors from the task are output data.
} finally {
    try {
        if ($null -ne $output) { $output.Dispose() }
    } finally {
        try {
            if ($started -and -not $process.HasExited) { $process.Kill() }
        } finally {
            if ($null -ne $process) { $process.Dispose() }
            PublishStatus 'result.json' $exitResult
        }
    }
}
