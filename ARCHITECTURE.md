# Arquitectura de fex

fex tiene tres objetivos: **rápido**, **bonito** y **fácil de extender**. Este documento explica cómo está organizado el código y cómo añadir cosas sin pelearse con él.

## Capas

```
main.rs            arranque: argumentos, ventana, --bench
app/               aplicación (GPUI)
  mod.rs           estado (Explorer), Modal, teclado
  commands.rs      registro de comandos  ← punto de extensión principal
  bindings.rs      atajos efectivos: por defecto + keymap del usuario
  selection.rs     cursor y selección múltiple (puro, testeado)
  navigation.rs    listar, refrescar, ordenar, rutas, S3
  file_ops.rs      copiar, pegar, papelera, renombrar, carpetas
  jobs.rs          trabajos en segundo plano, avisos, deshacer
  view/            solo render: sidebar, toolbar, list, overlays, status
keymap.rs          atajos como datos ("mod-shift-n") y diferencias Linux/macOS
config.rs          directorio de configuración por plataforma
backend.rs         Location + Capabilities  ← punto de extensión de backends
fs.rs ops.rs safe_fs.rs undo.rs watch.rs sidebar.rs s3.rs
                   dominio puro, sin GPUI, con tests propios
theme.rs icons.rs i18n.rs input.rs
                   presentación compartida
```

Reglas:

1. **El dominio no conoce la UI.** Nada fuera de `app/` y `main.rs` importa GPUI (salvo `icons`/`theme`, que son tokens visuales). Si algo se puede testear sin ventana, va fuera de `app/`.
2. **`view/` no decide nada.** Lee el estado y conecta clics a métodos o comandos. Si un `if` en `view/` decide si una acción está permitida, ese `if` debería ser el `when` de un comando.
3. **El hilo de UI no hace I/O.** Los listados van a `background_spawn`; las operaciones de ficheros, a un hilo propio (`jobs.rs`). La única excepción es un `rename(2)` suelto.
4. **Un estado modal a la vez.** `Modal` es un `enum`: renombrar, escribir una ruta, la paleta, confirmar un borrado o ver errores son mutuamente excluyentes por tipo.

## Añadir un comando

Todo lo que el usuario puede hacer es un `Command` en `app/commands.rs`:

```rust
cmd("file.duplicate", |s| s.duplicate, |e, cx| e.duplicate(cx))
    .icon(Icon::Copy)
    .keys(&["mod-shift-d"])
    .when(|e| e.caps().write && !e.entries.is_empty()),
```

Con eso el comando:

- responde al atajo (`keymap.rs` resuelve `mod` = Ctrl en Linux, ⌘ en macOS),
- aparece en la paleta (`Ctrl+K` / `⌘K`) con su atajo,
- se puede poner en un menú añadiendo `Cmd("file.duplicate")` a `ITEM_MENU` o `BACKGROUND_MENU`,
- se puede poner en la barra añadiendo su id a `TOOLBAR` en `view/toolbar.rs`.

Los usuarios pueden cambiar cualquier atajo sin recompilar con el fichero `keymap` (ver `app/bindings.rs` y el README); por eso los ids de comando son estables y forman parte de la API pública.

Las etiquetas de atajo en menús, tooltips y paleta salen de la misma cadena que casa la tecla, así que no se desincronizan.

Si dos comandos comparten atajo, gana el primero de la lista cuyo `when` sea cierto. Así funciona `mod-d`: fija la carpeta en local y descarga en S3.

Los tests de `commands.rs` comprueban que los ids no se repiten, que los atajos se pueden parsear, que los menús solo usan comandos que existen y que todo comando de la paleta tiene etiqueta en los tres idiomas.

## Añadir un backend

Un backend es una variante de `backend::Location` (local y S3 hoy). Para añadir uno (SFTP, WebDAV, un zip…):

1. Crea un módulo con su propia `Location` (como `s3::Location`) y una función `list` async que devuelva `Vec<fs::Entry>`.
2. Añade la variante a `backend::Location` y completa los `match`: `list`, `child`, `parent`, `entry_label`.
3. Declara qué soporta en `capabilities()`. Los comandos consultan `Capabilities` (escribir, abrir ficheros, descargar, vigilar cambios…), así que todo lo que el backend no soporta desaparece solo de los menús, la barra y la paleta.

Lo que todavía no está abstraído: las operaciones de escritura (`ops.rs`) trabajan con rutas locales. Un backend que escriba necesitará una operación propia lanzada con `run_job`, igual que la descarga de S3.

## Trabajos en segundo plano

`Explorer::run_job(job, n, work, make_undo, cx)`:

- corre `work` (un `Future` que devuelve `ops::Report`) en un hilo propio,
- muestra el progreso a partir de `jobs.progress` (bytes),
- al terminar, apila el deshacer que devuelva `make_undo`, informa de los errores y refresca el listado seleccionando lo creado.

Solo hay un trabajo a la vez (`can_start_job`). La ventana no se cierra mientras haya uno en curso.

## Rendimiento

- Lista virtualizada (`uniform_list`): solo se pintan las filas visibles.
- El listado no hace `stat`; tamaño y fecha llegan en una segunda pasada (`Capabilities::stat_pass`).
- Cada carga sube `generation`; los resultados de cargas anteriores se descartan.
- `fex --bench DIR` mide listado y `stat` sin GUI. `FEX_EXIT_AFTER_FIRST_FRAME=1` mide el arranque.

## Tests

```sh
cargo test                       # dominio, selección, keymap, comandos
cargo test --features ui-tests   # UI headless (Linux, Vulkan por software en CI)
```

Para lógica nueva con estado, prefiere un módulo puro con tests (como `selection.rs`) frente a meterla en `Explorer`.
