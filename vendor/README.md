# Vendored crates

## avif-decode

`avif-decode/` is the crates.io release of avif-decode 3.0.0 with one
change: `Decoder::from_avif_limited` decodes on the calling thread, so a
rav1d panic can't escape from a worker thread, and holds rav1d to a frame
size limit. Its CLI, tests and lockfile are left out. The workspace `Cargo.toml` points `[patch.crates-io]` at it.

`avif-decode.patch` is the whole difference from the published crate. CI
runs `scripts/check-vendor.sh`, which downloads the crate, checks its
sha256, applies the patch and fails if the result isn't exactly
`avif-decode/`.

To change the vendored code, edit `avif-decode/` and run
`scripts/check-vendor.sh --update` to rewrite the patch. To move to a new
upstream release, put the new crate's files in `avif-decode/`, reapply the
change, set `version` and `sha256` in the script (the `cksum` for that
version at `https://index.crates.io/av/if/avif-decode`), and run
`--update`.
