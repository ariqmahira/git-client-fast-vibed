#!/usr/bin/env bash
# Runs the GPUI app headlessly on Linux: Xvfb + a window manager + Mesa's
# software Vulkan (lavapipe), for CI and containers.
#
#   scripts/ui-headless.sh target/release/fgc <repo> [fgc args...]
#
# Needs: Xvfb, openbox, mesa-vulkan-drivers (x11-utils + xdotool only for the
# no-window-manager fallback).
#
# GPUI 0.2.2 starts its frame clock when X tells it the window is mapped
# and visible. Without a window manager, Xvfb can deliver MapNotify before
# GPUI has registered the window, and no frame is ever drawn. Real desktops
# always run a window manager, so we run one too (openbox: it maps windows
# immediately). Without one we fall back to unmapping and remapping the
# window once, which works most of the time but is timing-dependent.
#
# Frame timings recorded this way are CPU-side (render + layout + paint)
# timings; llvmpipe rasterizes on the CPU, so GPU time isn't representative.
set -euo pipefail

bin="$1"; shift
display="${FGC_DISPLAY:-:97}"
Xvfb "$display" -screen 0 1600x1000x24 -nolisten tcp >/dev/null 2>&1 &
pids=($!)
trap 'kill "${pids[@]}" 2>/dev/null || true' EXIT
for _ in $(seq 50); do DISPLAY="$display" xdpyinfo >/dev/null 2>&1 && break; sleep 0.1; done

export DISPLAY="$display"
export VK_ICD_FILENAMES="${VK_ICD_FILENAMES:-/usr/share/vulkan/icd.d/lvp_icd.json}"

wm=0
if command -v openbox >/dev/null 2>&1; then
  openbox >/dev/null 2>&1 &
  pids+=($!)
  wm=1
  sleep 0.5
fi

"$bin" "$@" &
app_pid=$!

if [ "$wm" = 0 ]; then
  echo "ui-headless: no window manager found (install openbox); using the remap fallback" >&2
  win=""
  for _ in $(seq 100); do
    win=$(xwininfo -root -children 2>/dev/null | awk '{ for (i = 1; i <= NF; i++) if ($i ~ /^[0-9]+x[0-9]+\+/) { split($i, d, "x"); if (d[1] > 100) { print $1; exit } } }')
    [ -n "$win" ] && break
    kill -0 $app_pid 2>/dev/null || break
    sleep 0.05
  done
  if [ -n "$win" ]; then
    for _ in $(seq 50); do xwininfo -id "$win" 2>/dev/null | grep -q IsViewable && break; sleep 0.02; done
    sleep 0.3
    xdotool windowunmap --sync "$win" windowmap --sync "$win" 2>/dev/null || true
  fi
fi
wait $app_pid
