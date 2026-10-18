# Cambios

## 0.6.2 — 2026-10-18

- Se recuperan las opciones de 720p y 1080p que ofrece el servicio. La disponibilidad depende del título.
- Calidad sustituye al brillo; Chromecast comparte la barra de controles.
- Se prefiere MP4 cuando existe una variante autorizada de la misma resolución y el mismo conjunto de idiomas, para disponer de duración y búsqueda en el reproductor.
- Se corrigen los nombres de audio y las entradas duplicadas del catálogo. Pistas distintas del mismo idioma se distinguen por número.
- El contador refleja la posición real del motor mientras se solicita un salto. La reanudación espera a que la reproducción esté preparada.
- Se conserva la posición y la pausa al cambiar de calidad.
- Se registra el código de la aplicación, documentación de compilación, pruebas sintéticas y resultados de validación sin datos personales.

Validación: 106 pruebas Rust aprobadas, 13 diagnósticos optativos omitidos; comprobación de controles a tres anchos de ventana; reproducción real en 1080p, selección de audio, salto a 2 minutos y regreso a 30 segundos, barra en pausa, botones de ±10 segundos y cambio a 720p conservando la pausa en el minuto 5. [Resultados](docs/validation/v0.6.2-playback.json) e [integridad del paquete](docs/validation/v0.6.2-package.json).

Esta publicación reúne las correcciones locales de 0.6.1 y 0.6.2; 0.6.1 no se publicó por separado. En una muestra de 20 títulos solo aparecieron 480p, 720p y 1080p; esto no demuestra la ausencia de otras resoluciones en todo el catálogo.

## 0.6.0 — 2026-10-18

- Chromecast integrado con descubrimiento de receptores y controles de reproducción dentro de Nébula.

## 0.5.0 — 2026-10-18

- Plataformas, categorías de 2026, diseño adaptable, compartir y actualizador integrado.
