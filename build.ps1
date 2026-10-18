param(
    [switch]$DebugBuild,
    [switch]$Offline,
    [switch]$SmokeTest,
    [switch]$Package
)

$ErrorActionPreference = 'Stop'
$workspacePath = Split-Path -Parent $PSScriptRoot
$localEnvironment = Join-Path $workspacePath '.tasktools\rust-env.ps1'
if (Test-Path -LiteralPath $localEnvironment) {
    . $localEnvironment
}
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw 'No se encontró Rust. Hace falta la cadena de herramientas local o una instalación compatible.'
}
$localAssembler = Join-Path $workspacePath '.tasktools\rust\binutils\mingw64\bin'
if (Test-Path -LiteralPath $localAssembler) {
    $localSysroot = & rustc --print sysroot
    $localLinkerTools = Join-Path $localSysroot 'lib\rustlib\x86_64-pc-windows-gnu\bin\self-contained'
    $env:PATH = "$localAssembler;$localLinkerTools;$env:PATH"
}
$manifest = Join-Path $PSScriptRoot 'Cargo.toml'
$buildArguments = @('build', '--locked', '--target', 'x86_64-pc-windows-gnu', '--manifest-path', $manifest)
if (-not $DebugBuild) { $buildArguments += '--release' }
if ($Offline) { $buildArguments += '--offline' }
& cargo @buildArguments
if ($LASTEXITCODE -ne 0) { throw "Falló la compilación ($LASTEXITCODE)." }

$metadataArguments = @('metadata', '--format-version', '1', '--locked', '--offline', '--filter-platform', 'x86_64-pc-windows-gnu', '--manifest-path', $manifest)
$metadata = (& cargo @metadataArguments) | ConvertFrom-Json
if ($LASTEXITCODE -ne 0) { throw 'No se pudieron localizar las dependencias de ejecución.' }
$webviewPackage = $metadata.packages | Where-Object { $_.name -eq 'webview2-com-sys' } | Select-Object -First 1
if (-not $webviewPackage) { throw 'No se encontró el cargador de WebView2.' }
$loader = Join-Path (Split-Path -Parent $webviewPackage.manifest_path) 'x64\WebView2Loader.dll'
$profile = if ($DebugBuild) { 'debug' } else { 'release' }
$outputDirectory = Join-Path $metadata.target_directory "x86_64-pc-windows-gnu\$profile"
Copy-Item -LiteralPath $loader -Destination (Join-Path $outputDirectory 'WebView2Loader.dll')
$vlcRuntimeDirectory = Join-Path $PSScriptRoot 'runtime\vlc'
if (-not (Test-Path -LiteralPath (Join-Path $vlcRuntimeDirectory 'libvlc.dll'))) { throw 'Falta el motor VLC verificado en runtime\vlc.' }
Copy-Item -LiteralPath $vlcRuntimeDirectory -Destination $outputDirectory -Recurse -Force
$executable = Join-Path $outputDirectory 'xuper-windows.exe'
& (Join-Path $PSScriptRoot 'packaging\Set-AppIcon.ps1') -Executable $executable -Icon (Join-Path $PSScriptRoot 'assets\nebula.ico')
Write-Output "Ejecutable: $executable"
if ($SmokeTest) {
    & $executable --smoke-test
    if ($LASTEXITCODE -ne 0) { throw "Falló la comprobación de inicio ($LASTEXITCODE)." }
}

if ($Package) {
    if ($DebugBuild) { throw 'El paquete portable requiere una compilación release.' }
    $packageDirectory = Join-Path $PSScriptRoot 'dist\Nebula-Windows'
    if (Test-Path -LiteralPath (Join-Path $packageDirectory 'data')) {
        throw 'La carpeta de paquete contiene datos de una ejecución anterior. Usa otra carpeta para conservar esa sesión antes de empaquetar.'
    }
    $licenseDirectory = Join-Path $packageDirectory 'licenses'
    New-Item -ItemType Directory -Path $licenseDirectory -Force | Out-Null
    Copy-Item -LiteralPath $executable -Destination (Join-Path $packageDirectory 'Nebula.exe')
    Copy-Item -LiteralPath $loader -Destination (Join-Path $packageDirectory 'WebView2Loader.dll')
    Copy-Item -LiteralPath $vlcRuntimeDirectory -Destination $packageDirectory -Recurse -Force
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'packaging\LEEME.txt') -Destination $packageDirectory
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'frontend\vendor\hls.LICENSE') -Destination (Join-Path $licenseDirectory 'hls.js-1.7.3.LICENSE')
    Get-ChildItem -LiteralPath (Join-Path $PSScriptRoot 'packaging\licenses') -File | ForEach-Object {
        Copy-Item -LiteralPath $_.FullName -Destination $licenseDirectory
    }
    $notices = [System.Collections.Generic.List[string]]::new()
    $notices.Add('Componentes de terceros de Nébula para Windows')
    $notices.Add('')
    $notices.Add('hls.js 1.7.3 | Apache-2.0 | https://github.com/video-dev/hls.js')
    $notices.Add('VLC/libVLC 3.0.24 | LGPL-2.1+ core, GPL-2.0+ modules | licencia en vlc/COPYING.txt | fuente: https://download.videolan.org/pub/videolan/vlc/3.0.24/vlc-3.0.24.tar.xz')
    $notices.Add('Microsoft WebView2Loader 1.0.3800.47 | licencia incluida | https://www.nuget.org/packages/Microsoft.Web.WebView2/1.0.3800.47/License')
    $notices.Add('')
    foreach ($dependency in ($metadata.packages | Where-Object { $_.name -ne 'xuper-windows' } | Sort-Object name,version)) {
        $sourceDirectory = Split-Path -Parent $dependency.manifest_path
        $destinationDirectory = Join-Path $licenseDirectory "$($dependency.name)-$($dependency.version)"
        New-Item -ItemType Directory -Path $destinationDirectory -Force | Out-Null
        $licenseFiles = @(Get-ChildItem -LiteralPath $sourceDirectory -File | Where-Object { $_.Name -match '^(LICENSE|LICENCE|COPYING|NOTICE|UNLICENSE)' })
        foreach ($licenseFile in $licenseFiles) {
            Copy-Item -LiteralPath $licenseFile.FullName -Destination $destinationDirectory
        }
        if ($dependency.license_file) {
            $extraLicense = Join-Path $sourceDirectory $dependency.license_file
            if (Test-Path -LiteralPath $extraLicense) { Copy-Item -LiteralPath $extraLicense -Destination $destinationDirectory }
        }
        if ($dependency.name -like 'webview2-com*') {
            Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'packaging\licenses\webview2-rs.LICENSE') -Destination $destinationDirectory
        }
        $notices.Add("$($dependency.name) $($dependency.version) | $($dependency.license) | $($dependency.repository)")
    }
    $toolchainPath = & rustc --print sysroot
    $runtimeDirectory = Join-Path $licenseDirectory 'rust-runtime'
    New-Item -ItemType Directory -Path $runtimeDirectory -Force | Out-Null
    foreach ($relativePath in @('share\doc\rust\COPYRIGHT-library.html', 'share\doc\COPYING.RUNTIME', 'share\doc\COPYING3')) {
        $runtimeLicense = Join-Path $toolchainPath $relativePath
        if (Test-Path -LiteralPath $runtimeLicense) { Copy-Item -LiteralPath $runtimeLicense -Destination $runtimeDirectory }
    }
    $rustLicenseDirectory = Join-Path $toolchainPath 'share\doc\rust\licenses'
    if (Test-Path -LiteralPath $rustLicenseDirectory) {
        $runtimeLicenseTexts = Join-Path $runtimeDirectory 'licenses'
        New-Item -ItemType Directory -Path $runtimeLicenseTexts -Force | Out-Null
        Get-ChildItem -LiteralPath $rustLicenseDirectory -File | ForEach-Object {
            Copy-Item -LiteralPath $_.FullName -Destination $runtimeLicenseTexts
        }
    }
    $notices.Add('')
    $notices.Add('Rust std y runtime GNU: avisos originales en licenses/rust-runtime.')
    $notices.Add('Inventario generado a partir de Cargo.lock y de los paquetes locales para Windows x86_64.')
    $notices | Set-Content -LiteralPath (Join-Path $packageDirectory 'THIRD-PARTY-NOTICES.txt') -Encoding utf8
    $archivePath = Join-Path $PSScriptRoot 'dist\Nebula-Windows-portable.zip'
    # Whitelist package files so local data, analysis and source never enter the archive.
    $archiveItems = @('Nebula.exe', 'WebView2Loader.dll', 'LEEME.txt', 'THIRD-PARTY-NOTICES.txt', 'licenses', 'vlc') | ForEach-Object { Join-Path $packageDirectory $_ }
    Get-ChildItem -LiteralPath $licenseDirectory -Recurse -File | Where-Object { $_.LastWriteTime.Year -lt 1980 } | ForEach-Object { $_.LastWriteTime = [datetime]'1980-01-01' }
    Compress-Archive -LiteralPath $archiveItems -DestinationPath $archivePath -Force
    Write-Output "Paquete: $archivePath"
}
