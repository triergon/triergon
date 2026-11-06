#!/bin/sh
# FFmpeg single-threaded AAC decode cost per pass (ms), development aid (not evidence):
# best-of-5 utime of `-stream_loop N` decoding to the null muxer minus best-of-5 utime of the
# same command with `-c:a copy` (demux / parse / mux overhead), divided by N+1 passes.
# usage: ffbench.sh FILE [N]
F=$1; N=${2:-10}
ut() { /opt/homebrew/bin/ffmpeg -nostdin -hide_banner -threads 1 -benchmark -stream_loop "$N" -f aac -i "$F" $1 -f null - 2>&1 | sed -n 's/.*utime=\([0-9.]*\)s.*/\1/p'; }
bd=999; bc=999
for i in 1 2 3 4 5; do
  a=$(ut ""); b=$(ut "-c:a copy")
  bd=$(echo "if ($a < $bd) $a else $bd" | bc -l)
  bc=$(echo "if ($b < $bc) $b else $bc" | bc -l)
done
echo "$(basename "$F") ffmpeg decode ms/pass: $(echo "($bd - $bc) * 1000 / ($N + 1)" | bc -l | cut -c1-6) (decode run $bd s, copy run $bc s, $((N+1)) passes)"
