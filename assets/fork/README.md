# Herdr Fleet brand assets

The fork's product name is **Herdr Fleet** (short name **Fleet**). The binary,
crate, config directories and protocol stay `herdr`; this identity is used only
on fork-owned surfaces: `docs/fork/`, the phone app manifest and icons (E4), the
gateway pages, and the GitHub repository description.

- `logo.svg` — source mark: one console (hollow amber ring) linked to many
  hosts (three nodes) on a slate rounded square.
- `logo-512.png`, `logo-192.png` — rasters for the PWA manifest and README.

Colours: ground `#1f2327`, nodes `#e8e9e7`, links `#5a616a`, accent `#f2a03d`.
Regenerate the PNGs with `rsvg-convert -w 512 -h 512 logo.svg -o logo-512.png`.
