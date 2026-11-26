#!/usr/bin/env bash
# Runs the GPUI app headlessly on Linux: Xvfb + Mesa's software Vulkan
# (lavapipe), for CI and containers.
#
#   scripts/ui-headless.sh target/release/fgc <repo> [fgc args...]
#
# Needs: Xvfb, xdotool, x11-utils (xwininfo), mesa-vulkan-drivers.
#
# Without a window manager, Xvfb delivers the window's MapNotify before
# GPUI 0.2.2 has registered the window, so GPUI never starts its frame
# clock. Real desktops always run a window manager; here we unmap and
# remap the window once it exists, which re-sends MapNotify.
#
# Frame timings recorded this way are CPU-side (render + layout + paint)
# timings; llvmpipe rasterizes on the CPU, so GPU time isn't representative.
set -euo pipefail

bin="$1"; shift
display="${FGC_DISPLAY:-:97}"
Xvfb "$display" -screen 0 1600x1000x24 -nolisten tcp >/dev/null 2>&1 &
xvfb_pid=$!
trap 'kill $xvfb_pid 2>/dev/null || true' EXIT
for _ in $(seq 50); do DISPLAY="$display" xwininfo -root >/dev/null 2>&1 && break; sleep 0.1; done

export DISPLAY="$display"
export VK_ICD_FILENAMES="${VK_ICD_FILENAMES:-/usr/share/vulkan/icd.d/lvp_icd.json}"
"$bin" "$@" &
app_pid=$!

# Wait for the app's main window (the only large top-level window), then remap it.
win=""
for _ in $(seq 100); do
  win=$(xwininfo -root -children 2>/dev/null | awk '$0 ~ /[0-9]+x[0-9]+\+/ { split($0, a, " "); for (i in a) if (a[i] ~ /^[0-9]+x[0-9]+\+/) { split(a[i], d, "x"); if (d[1] > 100) print $1 } }' | head -1)
  [ -n "$win" ] && break
  kill -0 $app_pid 2>/dev/null || break
  sleep 0.05
done
echo "ui-headless: window ${win:-not found}" >&2
if [ -n "$win" ]; then
  # Give GPUI a moment to register the window before re-sending MapNotify.
  for _ in $(seq 50); do xwininfo -id "$win" 2>/dev/null | grep -q IsViewable && break; sleep 0.02; done
  sleep "${FGC_REMAP_DELAY:-0.3}"
  xdotool windowunmap --sync "$win" windowmap --sync "$win" || true
fi
wait $app_pid
