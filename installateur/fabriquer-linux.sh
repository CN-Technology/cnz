#!/bin/sh
# Builds dist/linux/CNz-VERSION-linux-x86_64.tar.gz: programs compiled on
# Debian 11 (ci/Dockerfile.linux-diffusion) so they also run on older
# systems, with the installation scripts and the licences.
set -eu
cd "$(dirname "$0")/.."
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
docker build -q -t cnz-linux-diffusion -f ci/Dockerfile.linux-diffusion ci >/dev/null
docker run --rm --user "$(id -u):$(id -g)" -e CARGO_HOME=/cargo -e HOME=/tmp/h \
  -v "$HOME/.cache/cnz-cargo:/cargo" -v "$PWD:/src" -w /src cnz-linux-diffusion sh -c '
    CARGO_TARGET_DIR=/src/target/diffusion cargo build --release
    cd fenetre && CARGO_TARGET_DIR=/src/target/diffusion-fenetre cargo build --release'
nom="CNz-$VERSION-linux-x86_64"
dossier="dist/linux/$nom"
test ! -e "$dossier" || { echo "$dossier existe déjà" >&2; exit 1; }
mkdir -p "$dossier/licences"
cp target/diffusion/release/cnz target/diffusion-fenetre/release/cnz-fenetre "$dossier/"
strip "$dossier/cnz" "$dossier/cnz-fenetre"
cp installateur/linux/installer.sh installateur/linux/desinstaller.sh installateur/linux/LISEZMOI.txt NOTICE "$dossier/"
cp licences/* LICENSE-MIT LICENSE-APACHE "$dossier/licences/"
tar -czf "dist/linux/$nom.tar.gz" -C dist/linux "$nom"
ls -l "dist/linux/$nom.tar.gz"
