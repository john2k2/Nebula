param([Parameter(Mandatory=$true)][string]$NotesFile)
$ErrorActionPreference='Stop'
$appRoot=Split-Path -Parent $PSScriptRoot
$manifest=Get-Content -LiteralPath (Join-Path $appRoot 'Cargo.toml') -Raw
$version=[regex]::Match($manifest,'(?m)^version\s*=\s*"(\d+\.\d+\.\d+)"').Groups[1].Value
if(-not $version){throw 'No se pudo leer la versión de la aplicación.'}
if(-not (Get-Command gh -ErrorAction SilentlyContinue)){throw 'Instala GitHub CLI desde su fuente oficial y usa gh auth login antes de publicar.'}
if(-not (Test-Path -LiteralPath $NotesFile -PathType Leaf)){throw 'Falta el archivo de notas de esta versión.'}
& gh auth status
if($LASTEXITCODE -ne 0){throw 'Inicia sesión en GitHub CLI antes de publicar.'}
& (Join-Path $appRoot 'build.ps1') -Offline -Package
$zip=Join-Path $appRoot 'dist/Nebula-Windows-portable.zip'
Add-Type -AssemblyName System.IO.Compression.FileSystem
$archive=[System.IO.Compression.ZipFile]::OpenRead($zip)
try{
  foreach($entry in $archive.Entries){
    $name=$entry.FullName.Replace('\','/')
    if($name -notmatch '^(Nebula\.exe|WebView2Loader\.dll|LEEME\.txt|THIRD-PARTY-NOTICES\.txt|licenses/.*|vlc/.*)$'){throw "Archivo inesperado en el paquete: $name"}
  }
  if(-not $archive.GetEntry('Nebula.exe')){throw 'Falta el ejecutable.'}
}finally{$archive.Dispose()}
$sums=Join-Path $appRoot 'dist/SHA256SUMS.txt'
Set-Content -LiteralPath $sums -Encoding ascii -Value ((Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLower()+'  Nebula-Windows-portable.zip')
& gh release create "v$version" $zip $sums --repo john2k2/Nebula --title "Nébula $version" --notes-file $NotesFile --latest
if($LASTEXITCODE -ne 0){throw 'GitHub no confirmó la publicación. Revisa la versión antes de volver a intentarlo.'}
Write-Output "Versión publicada: https://github.com/john2k2/Nebula/releases/tag/v$version"
