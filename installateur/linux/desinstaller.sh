#!/bin/sh
# Removes CNz: right-click and applications menu, then the programs.
set -eu
dest="$HOME/.local/bin"
[ -x "$dest/cnz" ] && "$dest/cnz" menu desinstaller || true
rm -f "$dest/cnz" "$dest/cnz-fenetre"
echo "CNz est désinstallé."
