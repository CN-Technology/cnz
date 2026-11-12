#!/bin/sh
# Builds the Windows installer: compiles cnz.exe and cnz-fenetre.exe
# (cnz-windows image), then assembles CNz-installation-VERSION.exe with NSIS.
# Result in dist/windows/.
#
# Usage (from the repository root): sh installateur/fabriquer.sh
set -e
cd "$(dirname "$0")/.."
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)

docker run --rm --user "$(id -u):$(id -g)" -e CARGO_HOME=/cargo -e HOME=/tmp/h \
  -v "$HOME/.cache/cnz-cargo:/cargo" -v "$PWD:/src" -w /src cnz-windows sh -c '
    mkdir -p /tmp/h
    cargo build --release --target x86_64-pc-windows-gnu
    cd fenetre && cargo build --release --target x86_64-pc-windows-gnu'

mkdir -p dist/windows/assemblage
cp target/x86_64-pc-windows-gnu/release/cnz.exe \
   fenetre/target/x86_64-pc-windows-gnu/release/cnz-fenetre.exe \
   installateur/cnz.nsi installateur/icone.ico dist/windows/assemblage/
# NOTICE with Windows line endings, readable in Notepad.
sed 's/$/\r/' NOTICE > dist/windows/assemblage/NOTICE.txt

docker run --rm -v "$PWD/dist/windows/assemblage:/a" -w /a debian:13-slim sh -c "
    apt-get -qq update >/dev/null && apt-get -qq install -y nsis >/dev/null 2>&1
    makensis -V2 -DVERSION=$VERSION cnz.nsi
    chown $(id -u):$(id -g) CNz-installation-$VERSION.exe"
mv "dist/windows/assemblage/CNz-installation-$VERSION.exe" dist/windows/
rm -rf dist/windows/assemblage
ls -l "dist/windows/CNz-installation-$VERSION.exe"
