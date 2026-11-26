# Patched third-party crates

- `xattr-0.2.3`: pulled in by `gpui 0.2.2` (via `gpui_http_client` → `zed-async-tar`).
  libc ≥ 0.2.18x no longer exports `ENOATTR` on Linux, so the published crate fails to
  compile. The patch maps it to `ENODATA` (its value on Linux/Android). Remove once gpui
  moves off `xattr 0.2`.
