[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Executable,
    [Parameter(Mandatory)][string]$OutputZip,
    [string]$VendorArchive
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.IO.Compression

function Assert-OrdinaryPath([IO.FileSystemInfo]$Item) {
    $current = $Item
    while ($null -ne $current) {
        if ($current.Attributes -band [IO.FileAttributes]::ReparsePoint) {
            throw "Reparse points are not supported for package inputs or output: $($current.FullName)"
        }
        $current = if ($current -is [IO.FileInfo]) { $current.Directory } else { $current.Parent }
    }
}

function Open-PackageInput([string]$Path) {
    $item = Get-Item -LiteralPath $Path -Force
    if ($item -isnot [IO.FileInfo]) { throw "Package input must be a regular file: $Path" }
    Assert-OrdinaryPath $item
    [IO.File]::OpenRead($item.FullName)
}

function Get-Sha256([IO.Stream]$Stream) {
    (Get-FileHash -InputStream $Stream -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Assert-DependencyLicenses([IO.Stream]$Source) {
    $reader = [IO.StreamReader]::new($Source, [Text.Encoding]::UTF8, $true, 4096, $true)
    try {
        $null = $reader.ReadLine()
        $null = $reader.ReadLine()
        foreach ($name in 'Cargo.toml', 'Cargo.lock') {
            $text = [IO.File]::ReadAllText((Join-Path $PSScriptRoot $name)).Replace("`r`n", "`n")
            $bytes = [IO.MemoryStream]::new([Text.Encoding]::UTF8.GetBytes($text))
            try {
                $digest = Get-Sha256 $bytes
            } finally { $bytes.Dispose() }
            if ($reader.ReadLine() -cne "$name SHA-256 (LF): $digest") {
                throw "Update THIRD-PARTY-LICENSES.txt for the current $name before packaging."
            }
        }
    } finally {
        $reader.Dispose()
        $Source.Position = 0
    }
}

function Add-Payload([IO.Compression.ZipArchive]$Zip, [string]$Name, [IO.Stream]$Source) {
    $target = $Zip.CreateEntry($Name, [IO.Compression.CompressionLevel]::Optimal).Open()
    try { $Source.CopyTo($target) } finally { $target.Dispose() }
}

function Assert-X64Executable([IO.Stream]$Source) {
    $reader = [IO.BinaryReader]::new($Source, [Text.Encoding]::UTF8, $true)
    try {
        if ($Source.Length -lt 512 -or $reader.ReadUInt16() -ne 0x5a4d) {
            throw 'An x64 Windows release executable is required.'
        }
        $Source.Position = 0x3c
        $offset = $reader.ReadUInt32()
        if ($offset -lt 64 -or $offset -gt $Source.Length - 26) { throw 'Invalid PE header offset.' }
        $Source.Position = $offset
        if ($reader.ReadUInt32() -ne 0x4550 -or $reader.ReadUInt16() -ne 0x8664) {
            throw 'An x64 Windows release executable is required.'
        }
        $Source.Position = $offset + 20
        $optionalSize = $reader.ReadUInt16()
        $flags = $reader.ReadUInt16()
        if (($flags -band 0x2002) -ne 2 -or $reader.ReadUInt16() -ne 0x20b) {
            throw 'An x64 Windows release executable is required.'
        }
        $optionalOffset = $offset + 24
        if ($optionalSize -lt 112 -or $optionalOffset + $optionalSize -gt $Source.Length) {
            throw 'Invalid PE32+ optional header length.'
        }
        # The PE32+ data-directory table starts after its 112-byte header prefix.
        $Source.Position = $optionalOffset + 108
        $directoryCount = $reader.ReadUInt32()
        if ($directoryCount -gt [Math]::Floor(($optionalSize - 112) / 8)) {
            throw 'Invalid PE32+ data directory count.'
        }
        if ($directoryCount -gt 14) {
            $Source.Position = $optionalOffset + 112 + 14 * 8
            $clrAddress = $reader.ReadUInt32()
            $clrSize = $reader.ReadUInt32()
            if ($clrAddress -ne 0 -or $clrSize -ne 0) {
                throw 'The x64 Windows executable contains a CLR runtime header. Use the native Rust release executable.'
            }
        }
    } finally {
        $reader.Dispose()
        $Source.Position = 0
    }
}

function Confirm-Package([string]$Path, $Hashes) {
    $file = [IO.File]::OpenRead($Path)
    try {
        $zip = [IO.Compression.ZipArchive]::new($file, [IO.Compression.ZipArchiveMode]::Read, $true)
        try {
            if ($zip.Entries.Count -ne $Hashes.Count) { throw 'Package entry count does not match its payload.' }
            foreach ($name in $Hashes.Keys) {
                $entry = $zip.GetEntry($name)
                $expected = $Hashes[$name]
                if ($null -eq $entry -or $entry.Length -ne $expected.bytes) {
                    throw "Missing package entry or size mismatch: $name"
                }
                $stream = $entry.Open()
                try {
                    if ((Get-Sha256 $stream) -cne $expected.sha256) { throw "Package digest mismatch: $name" }
                } finally { $stream.Dispose() }
            }
        } finally { $zip.Dispose() }
    } finally { $file.Dispose() }
}

$repository = (Get-Item -LiteralPath $PSScriptRoot).FullName.TrimEnd('\')
$destination = [IO.Path]::GetFullPath($ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputZip))
$outputDirectory = Get-Item -LiteralPath ([IO.Path]::GetDirectoryName($destination)) -Force
if ($outputDirectory -isnot [IO.DirectoryInfo] -or [IO.Path]::GetExtension($destination) -ine '.zip') {
    throw 'OutputZip must name a ZIP file in an existing directory.'
}
Assert-OrdinaryPath $outputDirectory
$destination = Join-Path $outputDirectory.FullName ([IO.Path]::GetFileName($destination))
if ($destination.StartsWith($repository + '\', [StringComparison]::OrdinalIgnoreCase)) {
    throw 'The deployment ZIP must be written outside the source repository.'
}
if (Test-Path -LiteralPath $destination) { throw "Package already exists: $destination" }

$inputs = [ordered]@{
    'mcp-local-rust.exe' = $Executable
    'README.md' = Join-Path $PSScriptRoot 'README.md'
    'LICENSE' = Join-Path $PSScriptRoot 'LICENSE'
    'NOTICE' = Join-Path $PSScriptRoot 'NOTICE'
    'THIRD-PARTY-LICENSES.txt' = Join-Path $PSScriptRoot 'THIRD-PARTY-LICENSES.txt'
}
$hashes = [ordered]@{}
$stage = Join-Path $outputDirectory.FullName ('mcp-local-rust-package-' + [Guid]::NewGuid().ToString('N') + '.tmp')
$vendor = $null
$ownsStage = $false
try {
    if ($VendorArchive) {
        $pins = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'vendor/tunnel-client-windows-x64.json') -Raw | ConvertFrom-Json
        $vendor = Open-PackageInput $VendorArchive
        if ($vendor.Length -ne $pins.archive.bytes -or (Get-Sha256 $vendor) -cne $pins.archive.sha256) {
            throw "Vendor archive must match the official Windows x64 release $($pins.archive.name)."
        }
        $vendor.Position = 0
    }

    $output = [IO.File]::Open($stage, [IO.FileMode]::CreateNew, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
    $ownsStage = $true
    try {
        $zip = [IO.Compression.ZipArchive]::new($output, [IO.Compression.ZipArchiveMode]::Create, $true)
        try {
            foreach ($name in $inputs.Keys) {
                $source = Open-PackageInput $inputs[$name]
                try {
                    if ($name -ceq 'mcp-local-rust.exe') { Assert-X64Executable $source }
                    if ($name -ceq 'THIRD-PARTY-LICENSES.txt') { Assert-DependencyLicenses $source }
                    $hashes[$name] = @{ bytes = $source.Length; sha256 = Get-Sha256 $source }
                    $source.Position = 0
                    Add-Payload $zip $name $source
                } finally { $source.Dispose() }
            }

            if ($null -ne $vendor) {
                $zipIn = [IO.Compression.ZipArchive]::new($vendor, [IO.Compression.ZipArchiveMode]::Read, $true)
                try {
                    $files = @($pins.files.PSObject.Properties)
                    if ($zipIn.Entries.Count -ne $files.Count) { throw 'Vendor archive entry count differs from the release.' }
                    foreach ($file in $files) {
                        $entry = $zipIn.GetEntry($file.Name)
                        if ($null -eq $entry -or $entry.Length -ne $file.Value.bytes) {
                            throw "Missing vendor entry or size mismatch: $($file.Name)"
                        }
                        $name = "third_party/tunnel-client/$($file.Name)"
                        $hashes[$name] = $file.Value
                        $source = $entry.Open()
                        try { Add-Payload $zip $name $source } finally { $source.Dispose() }
                    }
                } finally { $zipIn.Dispose() }
            }

            $manifest = [Text.Encoding]::UTF8.GetBytes(($hashes | ConvertTo-Json -Depth 4))
            $source = [IO.MemoryStream]::new($manifest)
            try {
                $name = 'SHA256SUMS.json'
                $hashes[$name] = @{ bytes = $source.Length; sha256 = Get-Sha256 $source }
                $source.Position = 0
                Add-Payload $zip $name $source
            } finally { $source.Dispose() }
        } finally { $zip.Dispose() }
        $output.Flush($true)
    } finally { $output.Dispose() }

    Confirm-Package $stage $hashes
    [IO.File]::Move($stage, $destination)
    Write-Output $destination
} finally {
    if ($null -ne $vendor) { $vendor.Dispose() }
    if ($ownsStage -and (Test-Path -LiteralPath $stage -PathType Leaf)) {
        Remove-Item -LiteralPath $stage
    }
}
