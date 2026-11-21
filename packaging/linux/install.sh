#!/bin/sh
set -eu
source_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
data_dir=${XDG_DATA_HOME:-"$HOME/.local/share"}
case "$data_dir" in
    /*) ;;
    *) printf '%s\n' "XDG_DATA_HOME must be absolute" >&2; exit 1 ;;
esac
install -Dm755 "$source_dir/fex" "$HOME/.local/bin/fex"
install -Dm644 "$source_dir/fex.svg" "$data_dir/icons/hicolor/scalable/apps/fex.svg"
# La ruta absoluta permite arrancar desde el lanzador aunque .local/bin no esté en PATH.
python3 - "$source_dir/fex.desktop" "$data_dir/applications/fex.desktop" "$HOME/.local/bin/fex" <<'PY'
from pathlib import Path
import sys
source, target, executable = sys.argv[1:]
escaped = executable.replace('\\', '\\\\').replace('"', '\\"').replace('`', '\\`').replace('$', '\\$').replace('%', '%%')
target = Path(target)
target.parent.mkdir(parents=True, exist_ok=True)
target.write_text(Path(source).read_text().replace('Exec=fex %f', 'Exec="' + escaped + '" %f'))
PY
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$data_dir/applications"
fi
