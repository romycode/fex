# fex — explorador de archivos (Rust + GPUI)

> [!WARNING]
> **Proyecto de prueba desarrollado al 100 % por IA.** El código, los iconos, los tests y esta documentación se han desarrollado con asistentes de IA a partir de las indicaciones de una persona. La versión 0.2.0-beta.1 está en fase de validación para uso diario.

Plataformas: **Linux con Wayland** y **macOS en Apple Silicon (arm64)**. El objetivo de arranque en Linux es **< 200 ms desde exec hasta el primer frame**.

GPUI viene de Zed `main`, fijado al commit `d2a4fb6`. Usa wgpu en Linux y Metal en macOS.

## Qué hace

- **Paleta de comandos** (`Ctrl+K` / `⌘K`): busca y ejecuta cualquier acción disponible, con su atajo.
- Lista virtualizada (`uniform_list`): solo se pintan las filas visibles, aguanta 100k+ entradas.
- La ventana pinta primero y el listado llega después (`background_spawn`).
- El listado no hace `stat`: usa `d_type` vía `file_type()`. Tamaño y fecha llegan en una segunda pasada en background.
- Descarta resultados de navegaciones obsoletas mediante un contador de generación.
- Al subir de nivel, mantiene seleccionado el directorio del que vienes.
- Abre los ficheros con la aplicación predeterminada del sistema.
- Tema claro y oscuro. En modo Auto sigue la apariencia del sistema y cambia en caliente. Se puede forzar con `--light`/`--dark` o con el botón.
- Barra lateral con los directorios habituales del usuario y migas de pan clicables.
- **Carpetas fijadas** en la barra lateral. Fija la carpeta actual con el botón de la chincheta o con `Ctrl+D`. Los accesos se guardan entre sesiones.
- **Discos** muestra los volúmenes montados accesibles desde el explorador y refresca el listado automáticamente.
- Idiomas: español, catalán e inglés. Se detecta de `LC_ALL`/`LC_MESSAGES`/`LANG`/`LANGUAGE`; se fuerza con `--lang=ca|es|en` o se cambia en caliente con `i` o el botón del pie. Son tablas estáticas en `src/i18n.rs`, sin coste de arranque.
- **Refresco automático** con `notify`, debounce de 150 ms y como mucho ~2,5 refrescos/s. Conserva selección y scroll. Si borran el directorio actual, sube al primer ancestro que exista.
- **Operaciones de ficheros**, todas en background y sin bloquear la UI:
  - copiar, cortar y pegar, también en recursivo. Los symlinks se copian como symlinks. Entre sistemas de ficheros distintos, mover = copiar + borrar.
  - copia a un directorio temporal y publica el resultado completo mediante un renombrado exclusivo. Los nombres ocupados generan otro nombre, incluso con copias concurrentes.
  - conserva nombres no UTF-8, permisos, fecha de modificación y atributos extendidos de archivos y directorios.
  - al mover entre discos comprueba el origen antes de eliminarlo. Si el borrado falla, informa de la copia completa y de la ruta del origen.
  - muestra los bytes copiados y un resumen persistente de los errores. Permite reintentar los elementos fallidos cuyo destino sigue sin publicarse.
  - procesa una operación de archivos cada vez, en un hilo propio, y espera a que termine para permitir cerrar la ventana.
  - se puede cancelar desde el pie de la ventana o la paleta. Lo copiado a medias se descarta y lo pendiente queda para reintentar.
  - papelera del sistema mediante freedesktop en Linux y Foundation en macOS.
  - borrado permanente con confirmación. No sigue symlinks.
  - renombrado en línea: selecciona el nombre sin extensión, como Nautilus.
  - nueva carpeta, que entra directamente en modo renombrar.
- **Portapapeles del sistema**: al copiar deja las rutas como texto. Al pegar acepta rutas y URI `file://`, entradas de Nautilus/GNOME en Linux y archivos copiados desde Finder en macOS.
- **Deshacer (Ctrl+Z)**, con una pila de 50 pasos. Comprueba la identidad y los metadatos del árbol afectado antes de revertir copiar, mover, renombrar y crear carpetas:
  - papelera → restaura desde la papelera.
  - mover/cortar → lo devuelve a su sitio.
  - renombrar → recupera el nombre anterior.
  - copiar → manda las copias a la papelera (no las borra).
  - nueva carpeta → la quita si sigue como quedó al crearla o nombrarla. Crear y nombrar cuentan como un solo paso.
  - Nunca sobrescribe: si el sitio original está ocupado, avisa y no hace nada. El borrado permanente no se puede deshacer.
- Entrada de rutas con `Ctrl+L` / `⌘L`, con soporte de rutas absolutas, relativas, `~` y `~/`.
- Ordenación por nombre, tamaño o fecha al pulsar las cabeceras. Conserva las carpetas primero y la selección actual.
- Selección múltiple (Ctrl/Shift + clic, Shift + flechas, Ctrl+A) y menú contextual.
- Iconos SVG propios en `assets/icons/`, embebidos en el binario con `include_bytes!` y teñidos con el color del tema.

Atajos en Linux:

| Tecla | Acción |
|---|---|
| `↑` `↓` / `j` `k` | mover |
| `PgUp` `PgDn` `Home`/`g` `End` | saltar |
| `⏎` `→` `l` / doble click | abrir |
| `⌫` `←` `h` | subir |
| `.` | mostrar/ocultar ocultos |
| `t` | tema: Auto → Claro → Oscuro |
| `i` | idioma: ES → CA → EN |
| `Alt+↑` | subir |
| `Ctrl+C` `Ctrl+X` `Ctrl+V` | copiar / cortar / pegar |
| `Ctrl+Z` | deshacer (hasta 50 pasos) |
| `Ctrl+A` · `Shift+↑↓` · Ctrl/Shift+clic | seleccionar |
| `F2` | renombrar (`⏎` confirma, `Esc` cancela) |
| `Supr` | mover a la papelera |
| `Shift+Supr` | eliminar permanentemente (pide confirmación) |
| `Ctrl+Shift+N` | nueva carpeta |
| `F5` / `Ctrl+R` | actualizar |
| `Ctrl+H` / `.` | ocultos |
| `Ctrl+D` | fijar o desfijar la carpeta actual |
| `Ctrl+L` | escribir una ruta (`Enter` abre, `Esc` cancela) |
| clic derecho | menú contextual |
| `Esc` | cerrar menú / quitar selección |
| `Ctrl+K` / `Ctrl+Shift+P` | paleta de comandos |
| `q` / `Ctrl+Q` | salir |

En macOS usa `⌘` en los atajos con `Ctrl` y para alternar la selección con clic. Los archivos ocultos se alternan con `⇧⌘.`. Para mover a la papelera usa `⌘⌫`, y para el borrado permanente `⇧⌘⌫`.

## Amazon S3

### Conexiones

La sección **Amazon S3** de la barra lateral lista las conexiones guardadas. `+` añade una y el lápiz la edita. Cada conexión elige sus credenciales:

- **del sistema**: la cadena estándar de AWS (variables de entorno, `~/.aws/credentials`, SSO…), con un perfil opcional. Es la conexión `AWS` que existe por defecto.
- **claves propias**: access key, secret key y, si hace falta, session token.

Y, como opciones de cualquier conexión:

- **host (endpoint)**: un servidor compatible con S3 (el emulador oficial de AWS, MinIO, R2…). Vacío = AWS.
- **región**: si no se indica, la del perfil o `us-east-1`.
- **rutas path-style** (`host/bucket/clave`): necesarias en casi todos los servidores compatibles y cuando el host es una IP.
- **certificado CA**: un PEM adicional en el que confiar, para servidores con certificado propio.

Se guardan en `~/.config/fex/s3` (Linux) o `~/Library/Application Support/fex/s3` (macOS), con permisos `0600` porque pueden llevar claves. También se puede editar a mano:

```text
[AWS]
profile = default

[Emulador]
endpoint = https://127.0.0.1:9297
region = us-east-1
access_key = AKIA…
secret_key = …
path_style = true
ca_file = ~/ruta/al/ca.pem
```

### Operaciones

Dentro de un bucket funcionan las mismas acciones que en local:

- **copiar, cortar y pegar** entre local y S3 en cualquier sentido, y entre buckets del mismo perfil (copia servidor a servidor, sin pasar por tu máquina). Las carpetas se copian enteras.
- **renombrar, nueva carpeta y borrar.** S3 no tiene papelera: `Supr` pide confirmación y borra de forma permanente.
- **descargar** (`Ctrl+D` / `⌘D`) ficheros o carpetas a una carpeta local elegida.
- **deshacer** copias, movimientos dentro de S3, renombrados y carpetas nuevas. Mover entre local y S3 no se puede deshacer.
- **cancelar** desde el pie de la ventana, como en local.

Garantías, iguales que en local:

- **Nunca sobrescribe.** Las escrituras usan `If-None-Match: *`: si el destino ya existe, S3 las rechaza y fex elige otro nombre ("x (2).txt").
- **Nunca borra ni mueve algo que haya cambiado.** Los borrados y las copias de origen usan el ETag leído (`If-Match`). Si otro proceso modificó el objeto entretanto, la operación se detiene y lo avisa.
- **Sin carpetas a medias.** Si falla a mitad de una carpeta, se borra lo que ya se había copiado de ella. Las descargas se montan en un temporal y se publican enteras.
- Los ficheros grandes se suben por partes (subida multiparte) y las copias de más de 5 GB también.

Permisos IAM necesarios: `s3:ListBucket`, `s3:GetObject`, `s3:PutObject`, `s3:DeleteObject` y `s3:AbortMultipartUpload`. Los servicios compatibles con S3 (MinIO, R2…) deben admitir escrituras condicionales (`If-None-Match`); si no, las escrituras fallan en vez de sobrescribir.

## Atajos personalizados

Los atajos se cambian en un fichero de texto: `~/.config/fex/keymap` en Linux (respeta `XDG_CONFIG_HOME`) y `~/Library/Application Support/fex/keymap` en macOS. Lo más cómodo es abrirlo desde la paleta con **Editar atajos de teclado**: la primera vez se crea con todos los comandos comentados. También se puede ver la plantilla con `fex --list-commands`.

```text
# comando = atajo, atajo     (`mod` es Ctrl en Linux y ⌘ en macOS)
file.new_folder = mod-n
view.hidden     = linux:mod-h, mac:mod-shift-.
app.quit        =            # sin atajo
```

Tus atajos sustituyen los del comando y ganan si chocan con los de otro. Las líneas con errores se ignoran y se avisa en la barra de estado. Tras guardar, usa **Recargar atajos de teclado** en la paleta.

## Extender fex

Cada acción es un comando declarado una vez en `src/app/commands.rs`: de ahí salen el atajo, el menú contextual, la barra de herramientas y la paleta. Cada backend declara sus capacidades en `src/backend.rs`, y lo que no soporta desaparece solo de la interfaz. Los detalles están en [ARCHITECTURE.md](ARCHITECTURE.md).

## Requisitos

- Rust 1.98.1.
- Linux (Debian/Ubuntu):

  ```sh
  sudo apt install libxkbcommon-dev libwayland-dev libvulkan-dev \
    libfontconfig-dev libzstd-dev pkg-config clang cmake
  ```

- En Linux, un driver Vulkan funcional (`vulkaninfo --summary`).
- En macOS, Xcode seleccionado y el compilador de Metal disponible mediante `xcrun -f metal`.

## Compilar

```sh
cargo build --release
```

En un Mac con Apple Silicon puedes compilar y arrancar el binario arm64 con:

```sh
cargo build --release --locked --target aarch64-apple-darwin
./target/aarch64-apple-darwin/release/fex ~
```

## Instalar la beta

Desde el repositorio, ejecuta `make install` para compilar e instalar Fex en `~/.local/bin/fex`. El instalador detecta Linux o macOS con Apple Silicon y compila en modo release para la CPU de la máquina (`target-cpu=native`, ThinLTO y una unidad de generación de código). En macOS necesitas un toolchain de Rust arm64. El binario puede requerir instrucciones que no estén disponibles en otros procesadores. Puedes cambiar el destino con `make install BINDIR=/ruta/al/bin`.

El workflow de cada plataforma genera un paquete descargable como artefacto de la ejecución. También puedes generarlo con:

```sh
./scripts/package.sh
```

- Linux genera `dist/fex-linux-x86_64.tar.gz` (o la arquitectura de la máquina). Extrae el archivo y ejecuta `./fex/install.sh`. Instala el binario en `~/.local/bin` y el acceso del lanzador en el directorio de aplicaciones del usuario. El instalador necesita Python 3.
- macOS genera `dist/Fex-macos-arm64.zip`. Extrae `Fex.app` y cópiala a Aplicaciones. El paquete de esta beta se genera sin firma Developer ID ni notarización.

## Validación de la beta

La suite incluye pruebas de copias concurrentes, destinos ocupados y enlaces rotos, errores en copias recursivas, nombres no UTF-8, atributos extendidos, reintentos y deshacer sobre archivos modificados.

Para validar el uso diario en una máquina real, comprueba navegación y ordenación durante los refrescos, portapapeles con el explorador del sistema, papelera y deshacer, y copia de una carpeta a un disco externo. Comprueba también un fallo de espacio o desconexión y revisa las rutas que muestra el resumen antes de reintentar.

Las comprobaciones de metadatos detectan cambios entre pasos de una operación. La validación pendiente incluye modificaciones externas durante la eliminación recursiva, pérdida de alimentación y sistemas de archivos de red. Un cierre forzado puede dejar un directorio temporal oculto `.fex-copy-*` en el destino. Las copias de enlaces simbólicos conservan el enlace, mientras que sus atributos extendidos siguen pendientes.

## Medir en Linux

```sh
# Tiempos internos (desde main): app lista, ventana creada, primer frame
./target/release/fex ~

# Arranque real en frío, incluyendo exec + carga de .so + teardown
hyperfine --warmup 3 './target/release/fex --exit-after-first-frame ~'

# Solo I/O, sin GUI (para comparar tamaños de directorio y FS)
./target/release/fex --bench /usr/lib
```

`--exit-after-first-frame` (o `FEX_EXIT_AFTER_FIRST_FRAME=1`) cierra la app tras el primer frame, así que hyperfine mide el arranque completo.

Lo que debes mirar:

- **`app lista`** incluye la inicialización de la plataforma (conexión Wayland y fuentes). Si pasa de ~40 ms, sospecha de fontconfig. La primera vez que se construye su caché es lenta.
- **`ventana creada` → `primer frame`** es sobre todo la creación del device wgpu/Vulkan y la compilación de shaders. Es el sospechoso nº 1 si no bajas de 200 ms.
- El tiempo del listado aparece en la barra inferior. No debería influir en el primer frame.

## Siguiente paso si no llega a 200 ms

1. Comprueba la caché de pipelines de Vulkan del driver (Mesa la guarda en `~/.cache/mesa_shader_cache`). Mide en frío borrándola y en caliente sin borrarla.
2. Prueba `WGPU_BACKEND=gl` para comparar con el backend Vulkan.
3. Si sigue lento, usa el modo daemon: un proceso residente que abre ventanas bajo demanda.

## Tests

```sh
cargo test --release
```

Los tests unitarios cubren las operaciones de ficheros, el deshacer, los formatos de portapapeles, la edición de texto UTF-8, i18n, la detección de discos y la persistencia de carpetas fijadas.

## Límites del POC

No incluye todavía:

- iconos por MIME
- miniaturas
- rehacer (Ctrl+Shift+Z) y vista de la papelera
- diálogo de conflictos: ahora siempre se renombra automáticamente
- porcentaje total de las copias (ya se muestran bytes copiados y se pueden cancelar)
- copiar entre conexiones S3 distintas
- el campo de renombrado no soporta IME ni selección con ratón
- DnD
- pestañas
- búsqueda

## Licencia

MIT. Ver [LICENSE](LICENSE).
