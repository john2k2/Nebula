# Nébula para Windows

Aplicación portable con catálogo de películas y series, temporadas, favoritos y reproductor integrado basado en libVLC.

## Descargar

[Descargar la última versión para Windows](https://github.com/john2k2/Nebula/releases/latest)

1. Descarga **Nebula-Windows-portable.zip** desde la versión publicada.
2. Extrae todo el ZIP en una carpeta con permiso de escritura.
3. Abre **Nebula.exe**. Conserva las carpetas `vlc` y `licenses`, y `WebView2Loader.dll` junto al ejecutable.

Requiere Windows de 64 bits y Microsoft Edge WebView2 Runtime. libVLC está incluido: no necesitas instalar VLC por separado.

## Actualizaciones

Desde la versión 0.5.0, Nébula consulta las versiones estables de este repositorio. Usa **Buscar actualizaciones** al pie del catálogo o el aviso de nueva versión. Antes de instalar, se comprueban el tamaño y el SHA-256 que informa GitHub. La aplicación se reinicia al terminar.

La carpeta `data` conserva sesión local, favoritos, historial y caché de imágenes. El actualizador reemplaza los componentes de la aplicación y guarda la versión anterior para recuperación. Si actualizas manualmente, cierra Nébula y extrae el paquete sobre la misma carpeta, conservando `data`.

## Compartir

Puedes compartir Nébula desde el pie del catálogo o compartir una ficha desde el destacado, los detalles y el reproductor. Se comparte texto y el enlace de descarga; los enlaces temporales de reproducción y la sesión local no se incluyen.

## Versiones

**0.5.0** muestra primero las plataformas y las categorías de 2026, separadas para películas y series. Incluye accesos a Netflix, Disney+, Prime Video, HBO, Apple TV+, Hulu y Paramount+ según las categorías del servicio. También incorpora fondo panorámico con difusión y transición suave, ajustes para ventanas pequeñas y pantallas anchas, compartir y actualizador integrado. Incluye filtros por tipo, género, año, país, idioma de audio y puntaje mínimo, caché persistente de imágenes y desplazamiento animado de filas.

La disponibilidad de títulos, idiomas y calidades depende del servicio de origen. La aplicación no incluye archivos de películas o series.

## Componentes de terceros

El paquete incluye avisos y licencias de las dependencias en `THIRD-PARTY-NOTICES.txt` y `licenses`. VLC/libVLC se distribuye sin modificar, con licencias LGPL/GPL según el componente; sus fuentes oficiales están disponibles en [VideoLAN](https://www.videolan.org/vlc/download-sources.html).

Este repositorio contiene las versiones de distribución y su documentación. No incluye datos personales, sesiones, capturas ni informes locales de diagnóstico.
