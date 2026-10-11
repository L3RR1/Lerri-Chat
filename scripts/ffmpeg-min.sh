#!/usr/bin/env bash
# FFmpeg mínimo que vai no pacote do Lerri Chat (D-03, SPEC-01 §1).
#
# Só o que o app usa: avcodec + avutil, decoders H.264/HEVC/AV1 e os
# hwaccels D3D11VA. Sem programas, sem formatos, sem filtros, sem bibliotecas
# externas. Licença: LGPL 2.1 ou posterior (sem --enable-gpl/--enable-version3).
#
# Roda num Linux com mingw-w64 (é o que a CI usa):
#   sudo apt-get install mingw-w64 nasm
#   scripts/ffmpeg-min.sh saida/
# Gera saida/avcodec-63.dll, saida/avutil-61.dll e saida/LICENCA-FFmpeg.txt.
set -euo pipefail

VERSION=${FFMPEG_VERSION:-n9.0.2}
OUT=$(mkdir -p "${1:-ffmpeg-min}" && cd "${1:-ffmpeg-min}" && pwd)
WORK=$(mktemp -d)

git clone --quiet --depth 1 --branch "$VERSION" https://github.com/FFmpeg/FFmpeg "$WORK/src"
cd "$WORK/src"

CONFIG=(
  --target-os=mingw32 --arch=x86_64 --cross-prefix=x86_64-w64-mingw32-
  --enable-shared --disable-static
  --disable-programs --disable-doc --disable-debug
  --disable-everything --disable-autodetect --disable-network
  --disable-avdevice --disable-avformat --disable-avfilter --disable-swscale --disable-swresample
  --enable-w32threads --enable-d3d11va
  --enable-decoder=h264,hevc,av1
  --enable-hwaccel=h264_d3d11va,h264_d3d11va2,hevc_d3d11va,hevc_d3d11va2,av1_d3d11va,av1_d3d11va2
  # Sem libgcc_s_seh-1.dll ao lado: as DLLs só dependem do Windows.
  --extra-ldflags=-static-libgcc
)
./configure --prefix="$WORK/prefix" "${CONFIG[@]}"

# O configure desliga em silêncio o que o mingw não suporta; o app usa o
# formato D3D11 (os "*_d3d11va2").
for c in H264_DECODER HEVC_DECODER AV1_DECODER H264_D3D11VA2_HWACCEL HEVC_D3D11VA2_HWACCEL AV1_D3D11VA2_HWACCEL; do
  grep -q "^#define CONFIG_$c 1" config_components.h || { echo "faltou $c" >&2; exit 1; }
done

make -j"$(nproc)"
make install

cp "$WORK"/prefix/bin/avcodec-63.dll "$WORK"/prefix/bin/avutil-61.dll "$OUT/"
deps=$(x86_64-w64-mingw32-objdump -p "$OUT"/*.dll | sed -n 's/.*DLL Name: //p' | sort -u)
echo "dependências: $deps"
if echo "$deps" | grep -qiE '^lib'; then
  echo "as DLLs dependem de uma biblioteca do mingw" >&2
  exit 1
fi

{
  echo "As DLLs avcodec-63.dll e avutil-61.dll são do FFmpeg (https://ffmpeg.org),"
  echo "licenciado sob a LGPL 2.1 ou posterior, reproduzida abaixo."
  echo
  echo "Código-fonte: https://github.com/FFmpeg/FFmpeg/tree/$VERSION"
  echo "Compiladas com scripts/ffmpeg-min.sh do Lerri Chat"
  echo "(https://github.com/L3RR1/Lerri-Chat), configure:"
  echo "  ${CONFIG[*]}"
  echo
  cat COPYING.LGPLv2.1
} > "$OUT/LICENCA-FFmpeg.txt"

rm -rf "$WORK"
ls -l "$OUT"
