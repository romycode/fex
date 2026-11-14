# fex — POC explorador de archivos (Rust + GPUI)

> [!WARNING]
> **Proyecto de prueba desarrollado al 100 % por IA.** Todo el código, los iconos, los tests y esta documentación los ha escrito un asistente de IA (Claude, de Anthropic) a partir de las indicaciones de una persona. Es un experimento: no está auditado ni probado en uso real. Revísalo antes de usarlo con ficheros que te importen.

Objetivo: **< 200 ms desde exec hasta el primer frame** en Linux. **Solo Wayland**: el backend X11 de GPUI no se compila (13 crates menos, sin `libxcb`). En una sesión X11 no arranca.

GPUI viene de Zed `main`, fijado al commit `d2a4fb6` (renderer wgpu en Linux).

## Qué hace

- Lista virtualizada (`uniform_list`): solo se pintan las filas visibles, aguanta 100k+ entradas.
- La ventana pinta primero y el listado llega después (`background_spawn`).
- El listado no hace `stat`: usa `d_type` vía `file_type()`. Tamaño y fecha llegan en una segunda pasada en background.
- Descarta resultados de navegaciones obsoletas mediante un contador de generación.
- Al subir de nivel, mantiene seleccionado el directorio del que vienes.
- Abre los ficheros con `xdg-open`.
- Tema claro y oscuro. En modo Auto sigue el `color-scheme` del portal XDG y cambia en caliente. Se puede forzar con `--light`/`--dark` o con el botón.
- Barra lateral con los directorios XDG (`user-dirs.dirs`) y migas de pan clicables.
- Idiomas: español, catalán e inglés. Se detecta de `LC_ALL`/`LC_MESSAGES`/`LANG`/`LANGUAGE`; se fuerza con `--lang=ca|es|en` o se cambia en caliente con `i` o el botón del pie. Son tablas estáticas en `src/i18n.rs`, sin coste de arranque.
- **Refresco automático** con inotify (`notify`), debounce de 150 ms y como mucho ~2,5 refrescos/s. Conserva selección y scroll. Si borran el directorio actual, sube al primer ancestro que exista.
- **Operaciones de ficheros**, todas en background y sin bloquear la UI:
  - copiar, cortar y pegar, también en recursivo. Los symlinks se copian como symlinks. Entre sistemas de ficheros distintos, mover = copiar + borrar.
  - **nunca sobrescribe**: si el destino existe genera `x (copia).txt`, `x (copia 2).txt` o `x (2).txt`.
  - papelera freedesktop (`~/.local/share/Trash` y `.Trash-$UID` en cada punto de montaje, con `.trashinfo`).
  - borrado permanente con confirmación. No sigue symlinks.
  - renombrado en línea: selecciona el nombre sin extensión, como Nautilus.
  - nueva carpeta, que entra directamente en modo renombrar.
- **Portapapeles del sistema**: al copiar deja las rutas como texto. Al pegar entiende rutas, `file://` URIs y el formato de Nautilus/GNOME (`copy`/`cut`), así que se puede copiar desde Nautilus y pegar en fex.
- **Deshacer (Ctrl+Z)**, con una pila de 50 pasos:
  - papelera → restaura desde la papelera.
  - mover/cortar → lo devuelve a su sitio.
  - renombrar → recupera el nombre anterior.
  - copiar → manda las copias a la papelera (no las borra).
  - nueva carpeta → la quita (si ya tiene contenido, va a la papelera). Crear y nombrar cuentan como un solo paso.
  - Nunca sobrescribe: si el sitio original está ocupado, avisa y no hace nada. El borrado permanente no se puede deshacer.
- Selección múltiple (Ctrl/Shift + clic, Shift + flechas, Ctrl+A) y menú contextual.
- Iconos SVG propios en `assets/icons/`, embebidos en el binario con `include_bytes!` y teñidos con el color del tema.

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
| clic derecho | menú contextual |
| `Esc` | cerrar menú / quitar selección |
| `q` / `Ctrl+Q` | salir |

## Requisitos

- Rust ≥ 1.97 (verificado con 1.97.0; Zed fija 1.98.1).
- Debian/Ubuntu:

  ```sh
  sudo apt install libxkbcommon-dev libwayland-dev libvulkan-dev \
    libfontconfig-dev libzstd-dev pkg-config clang cmake
  ```

- Un driver Vulkan funcional (`vulkaninfo --summary`).

## Compilar

```sh
cargo build --release
```

El primer build tarda: descarga el repo de Zed y compila wgpu. Cuenta con 5–15 min en una máquina decente.

## Medir

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

Hay 15 tests unitarios: nombres únicos, copia recursiva con symlinks, rechazo de copiar una carpeta dentro de sí misma, mover sin sobrescribir, renombrar, borrar sin seguir symlinks, formatos de portapapeles, editor de texto UTF-8, i18n y el deshacer de mover, renombrar, nueva carpeta y papelera (con una papelera aislada).

## Límites del POC

No incluye todavía:

- iconos por MIME
- miniaturas
- rehacer (Ctrl+Shift+Z) y vista de la papelera
- diálogo de conflictos: ahora siempre se renombra automáticamente
- barra de progreso en copias grandes (ahora solo "Copiando N elementos…")
- el campo de renombrado no soporta IME ni selección con ratón
- DnD
- pestañas
- búsqueda

## Licencia

MIT. Ver [LICENSE](LICENSE).
