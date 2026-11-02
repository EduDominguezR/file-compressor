# Auditoría de MegaCompress

**Estado:** compilado, probado y empaquetado en Windows. Esta auditoría sustituye a la anterior,
que decía «no se ha compilado ni ejecutado nada»; eso ya no es cierto.

- **Fecha:** 2026-11-01
- **Sistema:** Windows NT 10.0.26200.0 (Windows 11)
- **Toolchain:** `rustc 1.99.1 (58cf8f9c3 2026-10-13)`, `cargo 1.99.1 (5f94df478 2026-08-27)`,
  host `x86_64-pc-windows-gnu`.
- **Artefacto:** `dist/MegaCompress.exe` (8 526 104 bytes; SHA-256
  `BAEC53FA41E7C03F04323ED631B857C30E41CC0285E5F1CF17B225FCCC44E72A`), copia de
  `target/release/megacompress.exe`. Solo depende de DLLs del sistema (no `libgcc`/`libstdc++`).
- **Build release:** `cargo build --release` terminó en 3 min 38 s.

No se ha probado en Linux/macOS, ni con ZIPs reales de decenas de GB, ni con un disco realmente
lleno. Todo lo que sigue distingue entre **ejecutado** y **pendiente**.

## 1. Correcciones de esta entrega

### 1.1 Interfaz

- **Rejilla de estadísticas:** `src/ui/app.rs` fija el ancho de cada columna con
  `Grid::min_col_width`/`max_col_width` y `src/ui/widgets.rs` dibuja la etiqueta con
  `TextWrapMode::Extend`. Ya no puede partirse «Tamañ o origina l» letra a letra.
- **Barra de progreso:** se añadió `theme::TRACK` (`#31302C`) y se asigna a
  `Visuals::extreme_bg_color`, que es el color de la pista de `egui::ProgressBar`. A 0 % se usa
  también `TRACK` como relleno para que no aparezca un punto azul suelto. La barra ocupa el ancho
  disponible (`desired_width(ui.available_width())`).
- **Registro:** `format_log_line` genera siempre `[<segundos>s] texto` con el corchete de cierre
  dentro de la misma cadena; las líneas se dibujan con `TextWrapMode::Extend` y scroll horizontal,
  así que el corchete no puede quedar huérfano en otra línea. Hay dos pruebas unitarias para el
  formato.
- **Trazo:** literales `1.0_f32`/`1.5_f32` para evitar ambigüedad de tipo en `Stroke::new`.

**Comprobación visual ejecutada:** se lanzó el `.exe`, se trajo la ventana a primer plano y se
capturó a 1014×718 (la captura está en `docs/ui-window.png`). El OCR de Windows reconoció las
etiquetas completas «Tamaño original»,
«Tamaño estimado», «RAM estimada» e «Hilos utilizados». En la captura, la pista de progreso a
0.0 % mide 597 px de ancho × 18 px de alto (filas 236–253) y su color es `#31302C`, distinto del
fondo `#1B1A18`. La línea de registro se ve como `[0.0s] Megacompress listo...`; la prueba unitaria
fija el formato exacto. La comprobación es por OCR y muestreo de píxeles, no por inspección humana
de la captura.

### 1.2 Motor y seguridad

- **COMPRIMIR → orquestador:** el botón envía `Msg::Start` y `orchestrator::spawn` ejecuta el
  pipeline en un hilo aparte; la UI recibe `Msg::Progress`/`Msg::Finished` por canal. El registro
  histórico de la app muestra trabajos lanzados desde la UI y terminados correctamente. No se
  automatizó un clic con un driver de GUI; la conexión se verificó por código y por los jobs que
  aparecen en el log.
- **7z con archivos de 0 bytes:** `sink_7z_lzma2.rs` usa
  `push_archive_entry(entry, None::<io::Empty>)`; `sevenz-rust2` marca `has_stream = false` y
  `size = 0`. La prueba nueva comprueba dos archivos vacíos rodeando a uno con contenido.
- **Duplicados reales:** `zip 2.6.1` guarda el directorio central en un `IndexMap` y colapsa
  nombres repetidos. Por eso `reader_zip::scan_raw_central` lee el EOCD/ZIP64 y el directorio
  central en bruto antes de abrir el crate, y rechaza duplicados exactos con
  `Error::DuplicateEntry`. Después se rechazan también las colisiones tras normalizar
  mayúsculas/`//` y tras NFC.
- **NFC:** `src/fs/safe_path.rs` normaliza cada componente a NFC con `unicode-normalization` y
  valida el nombre ya normalizado. Dos formas canónicamente equivalentes colisionan y se rechazan.
- **Pánico del hilo del motor:** el `JoinHandle` se inspecciona y el payload del pánico se
  convierte en `Error::Internal` visible en la UI; la UI no se queda esperando.
- **Disco lleno:** `src/error.rs` clasifica por código (`ErrorKind::StorageFull`, `ENOSPC` 28,
  `ERROR_DISK_FULL` 112, `ERROR_HANDLE_DISK_FULL` 39) y por las variantes tipadas de
  `sevenz-rust2` (`Io`, `FileOpen`, `MaybeBadPassword`). Un `io::Error` con el texto «no space left
  on device» pero sin código **no** se considera disco lleno; hay una prueba para eso.
- **Publicación atómica:** se reserva el nombre final con `OpenOptions::create_new` y se renombra
  el temporal verificado. Si el nombre ya existe, se reintenta con `archivo (1).ext`,
  `archivo (2).ext`, etc. Es más fuerte que una comprobación previa de existencia, que sería
  race-prone. El ZIP original se abre con `File::open` (solo lectura) y nunca se borra.
- **Verificación antes de publicar:** se descomprime el resultado completo en streaming, se calcula
  CRC32 y tamaño de cada entrada y se comparan con el manifiesto del ZIP original. Si algo falla,
  se borra el temporal y no se publica nada.
- **Progreso real:** `CheckedReader::read` llama a `Tracker::add_bytes(n)` solo después de leer `n`
  bytes que se entregan al sink. No hay porcentajes inventados; la finalización usa pasos reales y
  el 100 % solo se pinta tras `Finished(Ok)`.

## 2. Pruebas ejecutadas

| Comando | Resultado |
|---|---|
| `cargo fmt --check` | **OK** (sin diferencias) |
| `cargo check --all-targets` | **OK** (sin errores ni warnings) |
| `cargo test --all-targets` | **98 passed, 0 failed, 7 ignored** |
| `cargo clippy --all-targets --all-features -- -D warnings` | **OK** (sin warnings) |
| `cargo build --release` | **OK** en 3 min 38 s |
| Arranque del `.exe` | **OK**; ventana creada y capturada |

### 2.1 Desglose de `cargo test --all-targets`

| Binario/prueba | Pasan | Ignoradas |
|---|---:|---:|
| Unittests `src/lib.rs` | 56 | 0 |
| Unittests `src/main.rs` | 2 | 0 |
| `integration_cancel` | 2 | 0 |
| `integration_corrupt` | 6 | 0 |
| `integration_fast_roundtrip` (nueva) | 2 | 0 |
| `integration_integrity` | 8 | 0 |
| `integration_security` | 10 | 0 |
| `integration_small` | 7 | 0 |
| `integration_stability` | 4 | 1 |
| `integration_zip64` | 1 | 0 |
| `stress` | 0 | 6 |
| **Total** | **98** | **7** |

Ignoradas a propósito:

- `integration_stability::helper_hold_lock`: es el proceso hijo que lanza
  `abandoned_lock_after_process_kill_is_recovered`; no debe ejecutarse suelto.
- Las 6 de `stress.rs`: requieren GBs de disco/RAM, un ZIP64 real de más de 4 GiB o un disco
  realmente lleno. **No se ejecutaron.**

### 2.2 Prueba de punta a punta nueva

`tests/integration_fast_roundtrip.rs`:

1. `fast_mode_roundtrip_preserves_bytes_and_crc32`: crea un ZIP pequeño con texto, un archivo de
   0 bytes, una ruta Unicode y una carpeta anidada; comprime en modo Rápido; extrae el `.tar.zst`;
   compara bytes y CRC32 de cada entrada; comprueba que el ZIP original no cambió y que no quedan
   temporales ni locks.
2. `max_mode_records_zero_byte_files_without_corrupting_the_stream`: comprime en modo Máxima un ZIP
   con dos archivos vacíos y uno con contenido; extrae el `.7z` y compara el mapa completo.

También se añadió `rejects_unicode_collision_after_nfc_normalization` en
`integration_security.rs` y `unicode_names_are_normalized_to_nfc` en `safe_path.rs`.

### 2.3 Corrección de una prueba de estabilidad

La prueba `abandoned_lock_after_process_kill_is_recovered` fallaba porque buscaba el marcador con
`line.strip_prefix("MC_READY ")`, pero libtest escribe `test helper_hold_lock ... MC_READY ...` en
la misma línea. Se cambió a `line.split_once("MC_READY ")`; ahora pasa en 0.04 s. Era un fallo de la
prueba, no del bloqueo.

## 3. Limitaciones y decisiones honestas

### 3.1 7z sólido: no es sólido en todo el archivo

`sevenz-rust2 0.21.4` solo ofrece compresión sólida con
`ArchiveWriter::push_archive_entries(entries, readers)`, que exige **todos** los lectores del bloque
a la vez. `ZipArchive` entrega una entrada cada vez. La solución implementada:

- las entradas pequeñas se bufferizan en RAM hasta `block_bytes` (256 MiB por defecto) y se
  escriben como **un bloque sólido**;
- una entrada ≥ `block_bytes` se transmite como **su propio bloque**, sin cargarla entera en RAM;
- el diccionario se reinicia entre bloques.

Por tanto, la compresión sólida es **por bloque**, no sobre el archivo completo. Un archivo grande
no se beneficia de estar sólido con los demás. La RAM del bloque está acotada por `block_bytes`.
La alternativa para tener sólido real y streaming sin bufferizar sería **`.tar.xz`** con un
codificador xz en streaming (`xz2`/`liblzma`): las entradas tar se escriben secuencialmente al
codificador y este mantiene un solo diccionario. No está implementada; añadiría una dependencia y
perdería el contenedor 7z. Se documenta como propuesta, no como hecho.

### 3.2 RAM: hay presupuesto, no medición

El modo Máxima calcula un presupuesto y el log de una máquina de 8 hilos mostró
`RAM estimada 822 083 584 B` (≈ 784 MiB), por debajo de 1 GB. Es una **estimación** de la tabla de
`engine/mode.rs`, no una medición de RSS pico. No se ejecutó un perfilador de memoria ni el estrés
de 1 GB. La verificación completa añade una pasada de descompresión, pero lee en streaming y no
acumula el archivo.

### 3.3 Disco lleno real: no probado

La clasificación por código está probada con errores sintéticos (`ErrorKind::StorageFull`, 28, 112,
39) y con el caso de texto sin código. **No se llenó un disco real** ni se ejecutó
`stress_real_disk_full`.

### 3.4 ZIP64: cabeceras sintéticas, no >4 GiB

`integration_zip64` prueba las cabeceras ZIP64 con un ZIP pequeño. No se ha creado un ZIP real de
más de 4 GiB; `stress_real_zip64_over_4gib` sigue ignorada.

### 3.5 Symlinks: se omiten con aviso, no se rechaza el ZIP

Los enlaces simbólicos se detectan por el modo Unix (`0o120000`), **no se reproducen** (su destino
podría escapar del árbol), se cuentan y la UI avisa antes y después: «N enlaces simbólicos del ZIP
no se incluirán en el resultado». No se rechaza el ZIP completo para no bloquear archivos legítimos.
Si se prefiere rechazo total, es un cambio localizado en `reader_zip.rs`. La prueba
`symlink_entries_are_omitted_and_counted` fabrica un ZIP con una entrada `0o120777`, comprueba que
`skipped_symlinks == 1` y que el enlace no aparece en el `.tar.zst`.

### 3.6 Nombres: más estricto que Windows-only

Se rechazan rutas absolutas, UNC, `..`, unidades, ADS (`:`), nombres terminados en `.` o espacio,
`<>"|?*`, controles y reservados (`CON`, `NUL`, `COM1`…, `LPT9`, `CONIN$`, `CONOUT$`) con o sin
extensión. En esta entrega el destino es Windows, así que es correcto; en otras plataformas sería
más estricto de lo necesario.

### 3.7 Interfaz: captura puntual, no prueba automática

La app se lanzó y se capturó una vez a 1014×718. No se probó redimensionando a todos los anchos
(3/2/1 columnas), ni con escalado DPI alto, ni con un ZIP real arrastrado en esa sesión. Las
correcciones de rejilla/progreso/registro están cubiertas por código y por la captura, pero no por
un test de UI automatizado.

### 3.8 Verificación: cuesta una descompresión completa

Antes de publicar se descomprime todo el resultado para comparar CRC32/tamaño. Es lo que garantiza
que el contenedor es legible, pero en 7z cuesta aproximadamente lo mismo que descomprimir. No se ha
medido el tiempo total en un archivo grande.

### 3.9 Dependencias

Se añadió `unicode-normalization = "0.1"` (con `tinyvec` transitivo). `sevenz-rust2` sigue fijado a
`0.21.4`; no se auditó la 0.23. No se inventaron APIs: `push_archive_entries` y
`push_archive_entry(entry, None)` se contrastaron con docs.rs de 0.21.4.

## 4. Cómo reproducir

```powershell
$env:PATH = "C:\Users\reyes\.cargo\bin;" + $env:PATH
cd "C:\Users\reyes\OneDrive\Documentos\Proyecto predeterminado\megacompress"

cargo fmt --check
cargo check --all-targets
cargo test --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo build --release
```

El ejecutable queda en `target/release/megacompress.exe`; la copia entregada es
`dist/MegaCompress.exe`.

## 5. Pendiente / no probado

- Medir RSS pico real en Rápido y Máxima con un ZIP grande.
- Provocar un `ENOSPC` real.
- Ejecutar los 6 tests de `stress.rs` (1 GB, >4 GiB, disco lleno).
- Probar en Linux/macOS (el código usa `try_lock` y nombres Windows; no se validó).
- Probar ZIPs de producción con muchos miles de entradas y nombres Unicode reales.
- Prueba visual automatizada de la UI y de todos los anchos/DPI.
- Firma de código del `.exe` (no se firmó).
- Instalador; la entrega es un `.exe` portable.
