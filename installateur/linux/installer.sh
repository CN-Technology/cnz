#!/bin/sh
# Installs CNz for the user, without administrator rights: programs in
# ~/.local/bin, then applications menu and right-click menu (Nautilus,
# Dolphin, Nemo) through "cnz menu installer".
set -eu
ici=$(cd "$(dirname "$0")" && pwd)
dest="$HOME/.local/bin"
mkdir -p "$dest"
install -m 755 "$ici/cnz" "$ici/cnz-fenetre" "$dest/"
"$dest/cnz" menu installer
echo
echo "CNz est installé dans $dest."
case ":$PATH:" in
    *":$dest:"*) ;;
    *) echo "Pour taper « cnz » dans un terminal, ajoutez $dest à votre PATH." ;;
esac
echo "Pour désinstaller : sh desinstaller.sh"
