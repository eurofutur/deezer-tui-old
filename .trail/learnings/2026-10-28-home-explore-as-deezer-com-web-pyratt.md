---
title: Home/Explore as deezer.com web pages (page.get)
author: Pyratt
date: 2026-10-28
branch: main
touches:
  - README.md
  - src/api.rs
  - src/app.rs
  - src/main.rs
  - src/ui.rs
---

## Decided
- Home and Explore use gateway `page.get` (the web site's call): sections of cards. Generic BrowsePage view (app.browse) also serves genre channels ("channels/<slug>") and "See all" ("channels/module/<id>").
- Item kinds kept: album, playlist, artist, smarttracklist (tracks via `smartTracklist.getSongs`, source "__stl__:<id>"), flow (`radio.getUserRadio` + config_id unless "default", source "__flow__:<id>", played at once), channel, track. Podcasts/videos/external links are dropped.
- Main panel = one framed panel; headers drawn borderless inside it (split_header_body draws the frame, `plain()` for inner blocks).
- Old LoadHome/LoadExplore and fetch_home/explore_tracks + fallbacks removed; resume maps legacy "__home__"/"__explore__" to LoadPage.

## Tried and ruled out
- Passing `gateway_input` in the JSON body of page.get: Deezer answers MISSING_PARAMETER_PAGE. It must be in the query string (gw_post_query).

## Watch out
- page.get section titles/subtitles come back in the account language even with LANG "en".
- Explicit badge = EXPLICIT_LYRICS_STATUS 1 or 4.
- Browse rows cut at the top/bottom are not drawn (half-block covers can't be clipped); scroll snaps to row starts.
- Test a second instance with HOME pointed at a scratch dir holding a copy of ~/.deezer-tui-config.json (resume/discord off) so the real state file isn't overwritten.
- Flow and mouse clicks on cards were not tested end-to-end.
