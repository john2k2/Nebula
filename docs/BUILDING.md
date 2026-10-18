# Compilación y pruebas

Este repositorio registra el código de la aplicación y los archivos de distribución. La configuración de compatibilidad del servicio es privada y no se publica. El código no constituye una configuración lista para conectarse al servicio por sí solo.

## Requisitos

- Windows de 64 bits, Rust con destino `x86_64-pc-windows-gnu` y las herramientas de enlace GNU correspondientes.
- Microsoft Edge WebView2 Runtime.
- El paquete oficial VLC 3.0.24 de 64 bits, sin modificar, en `runtime/vlc/`, con `libvlc.dll`, `libvlccore.dll` y `plugins/`. Descarga y licencias: [VideoLAN](https://www.videolan.org/vlc/download-sources.html).
- Los archivos locales autorizados `analysis/protocol.constants.local.json` y `analysis/playback/ranger.constants.local.json`. Son entradas privadas de compilación utilizadas por `include_str!`; sus claves, parámetros y valores no se incluyen en Git.

El proceso de desarrollo original puede cargar opcionalmente una cadena de herramientas desde `../.tasktools/rust-env.ps1`. Si no existe, `build.ps1` utiliza la instalación de Rust disponible en `PATH`.

## Compilar

```powershell
.\build.ps1 -Package
```

El paquete se genera en `dist/Nebula-Windows-portable.zip`. `-Offline` requiere haber descargado antes las dependencias de `Cargo.lock`. La carpeta `dist/Nebula-Windows` debe estar libre de datos de usuario para empaquetar.

## Pruebas

Con las entradas locales anteriores:

```powershell
cargo test --locked --target x86_64-pc-windows-gnu -- --test-threads=1
```

Regresión de búsqueda del adaptador, sin servicio, configuración privada ni dependencias npm:

```powershell
node analysis/playback/qa-native-seek.cjs
```

Los diagnósticos conectados están omitidos por defecto. No deben ejecutarse contra una sesión ajena ni copiar identidades de un teléfono. Los resultados publicados en `docs/validation/` contienen únicamente comprobaciones técnicas y hashes del paquete.

Los archivos `*.synthetic.json` y `protocol-vectors.safe.json` son entradas de prueba; no contienen sesiones ni licencias de reproducción. Las pruebas criptográficas de compatibilidad requieren que las entradas locales coincidan con los vectores correspondientes.

## Archivos excluidos

No se publican el APK de referencia, configuración privada, sesiones, favoritos, caché, capturas, tráfico de red, registros completos, datos del teléfono, binarios de compilación ni herramientas locales. El ZIP descargable contiene el ejecutable y sus dependencias de ejecución; conserva las licencias originales de terceros.
