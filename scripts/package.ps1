param([string]$LocalConfig)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$taskRoot = Split-Path $PSScriptRoot -Parent
Push-Location $taskRoot
try {
    $cargoOptions = @()
    if ($LocalConfig) { $cargoOptions += @('--config', $LocalConfig) }
    $remaps = @()
    if ($env:USERPROFILE) { $remaps += '--remap-path-prefix=' + $env:USERPROFILE.Replace('\', '/') + '=/user' }
    if ($env:CARGO_HOME) { $remaps += '--remap-path-prefix=' + $env:CARGO_HOME.Replace('\', '/') + '=/cargo' }
    $remaps += '--remap-path-prefix=' + $taskRoot.Replace('\', '/') + '=/src/quota-tray'
    $remapConfig = 'target.x86_64-pc-windows-msvc.rustflags=' + (ConvertTo-Json -InputObject @($remaps) -Compress)
    New-Item -ItemType Directory -Force -Path (Join-Path $taskRoot 'target') | Out-Null
    $remapFile = Join-Path $taskRoot 'target/package-remap.toml'
    Set-Content -LiteralPath $remapFile -Value $remapConfig -Encoding utf8
    & cargo build @cargoOptions --config $remapFile --locked --release --target x86_64-pc-windows-msvc
    if ($LASTEXITCODE -ne 0) { throw 'Release build failed.' }
    $metadataText = & cargo metadata --locked --offline --format-version 1 --filter-platform x86_64-pc-windows-msvc
    if ($LASTEXITCODE -ne 0) { throw 'Dependency metadata failed.' }
    $metadata = $metadataText | ConvertFrom-Json
    $version = ($metadata.packages | Where-Object name -eq 'quota-tray').version
    $dist = Join-Path $taskRoot 'dist'
    New-Item -ItemType Directory -Force -Path $dist | Out-Null
    $exe = Join-Path $dist 'quota-tray.exe'
    Copy-Item -LiteralPath 'target/x86_64-pc-windows-msvc/release/quota-tray.exe' -Destination $exe -Force
    Copy-Item -LiteralPath LICENSE,README.md -Destination $dist -Force

    $notices = [System.Text.StringBuilder]::new()
    [void]$notices.AppendLine('Third-party dependency notices')
    foreach ($package in ($metadata.packages | Where-Object name -ne 'quota-tray' | Sort-Object name)) {
        [void]$notices.AppendLine("`n$($package.name) $($package.version) ($($package.license))")
        $licenseFiles = @(Get-ChildItem -LiteralPath (Split-Path $package.manifest_path -Parent) -File |
            Where-Object { $_.Name -match '^(LICENSE|COPYING)' })
        if (!$licenseFiles.Count) { throw "No license text found for $($package.name)." }
        foreach ($licenseFile in $licenseFiles) {
            [void]$notices.AppendLine((Get-Content -LiteralPath $licenseFile.FullName -Raw))
        }
    }
    [void]$notices.AppendLine('Rust standard library licensing: https://www.rust-lang.org/policies/licenses')
    $noticePath = Join-Path $dist 'THIRD_PARTY_NOTICES.txt'
    Set-Content -LiteralPath $noticePath -Value $notices.ToString() -Encoding utf8

    $bytes = [IO.File]::ReadAllBytes($exe)
    foreach ($encoding in @([Text.Encoding]::UTF8, [Text.Encoding]::Unicode)) {
        $text = $encoding.GetString($bytes)
        if ($text -match '(?i)[a-z]:[\\/]Users[\\/][^\\/\s]+[\\/]' -or
            $text -match '/home/[^/\s]+/' -or
            ($env:USERPROFILE -and ($text.Contains($env:USERPROFILE) -or $text.Contains($env:USERPROFILE.Replace('\', '/'))))) {
            throw 'Release binary contains a user profile path. Check compiler path remapping.'
        }
    }
    $archive = Join-Path $dist "quota-tray-$version-windows-x64.zip"
    $files = @($exe, (Join-Path $dist 'README.md'), (Join-Path $dist 'LICENSE'), $noticePath)
    Compress-Archive -LiteralPath $files -DestinationPath $archive -Force
    Add-Type -AssemblyName System.IO.Compression
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $sourceArchive = Join-Path $dist "quota-tray-$version-source.zip"
    $sourceFiles = @('.cargo/config.toml', '.github/workflows/windows.yml', '.gitignore',
        'Cargo.toml', 'Cargo.lock', 'README.md', 'LICENSE', 'scripts/package.ps1')
    $sourceFiles += Get-ChildItem -LiteralPath (Join-Path $taskRoot 'src') -Filter '*.rs' -Recurse -File |
        Where-Object { $_.Name -notlike 'secrets.*' } |
        ForEach-Object { $_.FullName.Substring($taskRoot.Length + 1).Replace('\', '/') }
    $stream = [IO.File]::Open($sourceArchive, [IO.FileMode]::Create)
    $sourceZip = [IO.Compression.ZipArchive]::new($stream, [IO.Compression.ZipArchiveMode]::Create)
    try {
        foreach ($sourceFile in $sourceFiles) {
            [void][IO.Compression.ZipFileExtensions]::CreateEntryFromFile($sourceZip,
                (Join-Path $taskRoot $sourceFile), $sourceFile)
        }
    } finally {
        $sourceZip.Dispose()
        $stream.Dispose()
    }
    $hashes = @($exe, $archive, $sourceArchive) | ForEach-Object {
        $hash = Get-FileHash -LiteralPath $_ -Algorithm SHA256
        "$($hash.Hash.ToLowerInvariant())  $(Split-Path $_ -Leaf)"
    }
    Set-Content -LiteralPath (Join-Path $dist 'SHA256SUMS.txt') -Value $hashes -Encoding ascii
    Write-Output "Package ready: dist/$(Split-Path $archive -Leaf)"
} finally {
    Pop-Location
}
