#!/bin/sh
# Rebuilds the embedded fonts, reduced to the characters actually used
# (run again when the translations in src/langue.rs change):
#   - Inter (body text): Latin (Vietnamese included), Cyrillic;
#   - Poppins Bold (titles and logo): Latin (Russian, Ukrainian and
#     Vietnamese titles use Inter: Poppins has no Cyrillic);
#   - Noto Sans CJK, Chinese, Japanese and Korean versions: only the
#     characters present in src/langue.rs;
#   - Noto Sans Arabic: Arabic, Persian and Urdu letters and their
#     presentation forms.
# Sources: Debian 13 packages (Inter, Noto) and the google/fonts repository.
# Usage (from fenetre/): sh polices/reduire.sh
set -e
cd "$(dirname "$0")/.."
docker run --rm -v "$PWD:/f" debian:13-slim sh -c '
set -e
apt-get update -qq >/dev/null
apt-get install -y -qq fonttools curl ca-certificates >/dev/null 2>&1
cd /tmp
apt-get download -qq fonts-inter fonts-noto-cjk fonts-noto-core 2>/dev/null
for d in *.deb; do dpkg -x $d x; done
G=https://github.com/google/fonts/raw/main/ofl
curl -sSLf -o Poppins.ttf $G/poppins/Poppins-Bold.ttf
I=x/usr/share/fonts/opentype/inter
C=x/usr/share/fonts/opentype/noto
A=x/usr/share/fonts/truetype/noto
P="U+0020-007E,U+00A0-00FF,U+2013-2014,U+2018-201E,U+2022,U+2026,U+2039-203A,U+20AC,U+2190-2193,U+2212,U+2248,U+202F"
LAT="$P,U+0100-017F"
VI="U+0180-024F,U+0300-0323,U+1EA0-1EF9"
CYR="U+0400-045F,U+0490-0491"
o="--layout-features=kern --output-file"
for w in Regular SemiBold; do pyftsubset $I/Inter-$w.otf --unicodes="$LAT,$VI,$CYR" $o=/f/polices/Inter-$w.otf; done
pyftsubset Poppins.ttf --unicodes="$LAT" $o=/f/polices/Poppins-Bold.ttf
# Faces in the .ttc files: 0 Japanese, 1 Korean, 2 Simplified Chinese.
for w in Regular Bold; do
  pyftsubset $C/NotoSansCJK-$w.ttc --font-number=0 --text-file=/f/src/langue.rs --unicodes="U+3000-303F,U+FF01-FF1F" $o=/f/polices/NotoSansJP-$w.otf
  pyftsubset $C/NotoSansCJK-$w.ttc --font-number=2 --text-file=/f/src/langue.rs --unicodes="U+3000-303F,U+FF01-FF1F" $o=/f/polices/NotoSansSC-$w.otf
  pyftsubset $C/NotoSansCJK-$w.ttc --font-number=1 --text-file=/f/src/langue.rs --unicodes="U+3000-303F,U+FF01-FF1F" $o=/f/polices/NotoSansKR-$w.otf
  pyftsubset $A/NotoSansArabic-$w.ttf --unicodes="U+0600-06FF,U+FB50-FDFF,U+FE70-FEFF" $o=/f/polices/NotoSansArabic-$w.ttf
done
chown -R '"$(id -u):$(id -g)"' /f/polices
' 2>&1 | grep -v "WARNING" || true
ls -l polices
