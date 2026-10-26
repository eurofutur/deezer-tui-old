---
title: Self-update from Codeberg releases
author: Pyratt
date: 2026-10-26
branch: main
touches:
  - src/update.rs
  - release.sh
  - Cargo.toml
---

## Decided
- In-app updater in src/update.rs: GET codeberg API releases/latest, compare tag (vX.Y.Z) to CARGO_PKG_VERSION, download asset `deezer-tui-x86_64-linux`, write <exe>.new then rename over current_exe.
- Startup check is silent unless a newer release exists; Settings row 6 = manual check / install (settings max index bumped to 6 in app.rs).
- release.sh <version>: bumps Cargo.toml, builds, tags, pushes, creates release + uploads asset via API (needs CODEBERG_TOKEN write:repository, jq).

## Tried and ruled out
- Unauthenticated API access: repo pyratt/deezer-tui is private, API returns 404 → token via config `codeberg_token` or env CODEBERG_TOKEN.

## Watch out
- Download of private-repo asset with token header via browser_download_url is untested; ELF magic check prevents installing an HTML login page.
- Old tags v1 / v1-alpha come from upstream (Minuga-RC); they are not releases.
- Self-replace fails if the binary lives in a non-writable path (AUR /usr/bin).
