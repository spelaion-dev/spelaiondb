# ADR-085: Map tiles are fetched by the core under the tile servers' policy, and OpenStreetMap is offered, never defaulted

- **Status:** Accepted
- **Date:** 2026-11-25
- **Supersedes:** ADR-045's alternative "Fetching tiles through the core", and the part of its
  point 6 that has the web view request tiles itself
- **Builds on:** ADR-045 (geometry on a map), ADR-055 (a transport that only moves bytes)

## Context
ADR-045 shows tiles only from a server the user names, and lets the web view request them as
images without a referrer. That keeps the privacy decision with the user, but the requests go
out with WebView2's (or WebKit's) own User-Agent, are cached however the web view likes, and
are not bounded by anything but the viewport. People expect a map out of the box, as other
clients give it, and OpenStreetMap's tile policy (operations.osmfoundation.org/policies/tiles)
asks of an application:

- a distinct, stable User-Agent that names it (generic or library ones may be blocked);
- honouring `Cache-Control`, `Expires` and `ETag`, keeping a tile at least 7 days when the
  headers cannot be read, and revalidating with conditional requests;
- attribution that is visible, never behind a toggle, while its tiles show;
- no bulk or offline download, and few parallel requests.

A web view cannot set its User-Agent per request, and its image cache is not ours to bound.

## Decision
1. **The core fetches every tile** (`spelaion-core::tiles`), whatever the server: OpenStreetMap
   and any server the user types go the same way. The page asks only for a tile of a map view
   (`view`, `z`, `x`, `y`) and receives the image; it never names an address. The core builds the
   address from the stored, checked tile server (`check_tile_server`, ADR-045) and refuses
   before anything else when tiles are off, when no server is stored, or when the tile is out of
   range (zoom 0 to 19). The web view's content security policy no longer lets images come from
   the network at all: tiles reach it as `data:` images.
2. **A transport that only moves bytes** (`spelaion-tile-http`, like ADR-055's
   `spelaion-https`): `https://`, or `http://` only to this computer, as the stored server allows;
   a redirect only to another `https://` address; the system's proxies; no cookies, no
   credentials, **no `Referer`**; and one fixed, stable **User-Agent that names the product, its
   version and the project's address** (`SpelaionDB/<version> (+https://github.com/spelaion-dev/spelaiondb)`),
   with nothing of the user's in it. The core adds only `If-None-Match` and `If-Modified-Since`.
3. **A disk cache that honours the headers**, in the user's folder (`tile-cache`, next to
   `state.db`): a tile is served from it without a request while it is fresh by `Cache-Control`
   (`max-age`; `no-cache` stores it but revalidates every time, `no-store` keeps nothing) or
   else `Expires`, and for 7 days when neither can be read. Once stale, the next use sends a
   conditional request with its `ETag` and `Last-Modified`; a `304` keeps the bytes and takes
   the new lifetime. The cache is bounded (256 MiB), the least recently used tiles leave first,
   and a tile larger than 2 MiB or that is not a PNG, JPEG or WebP image is refused and not kept.
4. **Requests are bounded:** at most two at a time for the whole app, at most 300 network
   requests per map view (a map opened in the cell panel), and only the tiles of the viewport,
   asked for once the view has stopped moving. Nothing is fetched ahead of the viewport and
   there is no way to download an area. A view past its limit says so and shows what it has.
5. **OpenStreetMap is offered, never defaulted.** Settings › Map gains "Use OpenStreetMap", which
   fills the server (`https://tile.openstreetmap.org/{z}/{x}/{y}.png`), its attribution ("©
   OpenStreetMap contributors") and the attribution's link (its copyright page), and turns tiles
   on: that click is the user's opt-in. Tiles stay off for everyone else, and the setting says
   what each tile tells the server (the address the request comes from, which square of the map
   and when, and the app's name and version). The preset lives in the core
   (`tiles::OPENSTREETMAP`), not in React.
6. **Attribution is visible and linked while tiles show:** the map shows the server's name and
   attribution whenever its tiles are drawn, and the attribution opens its link (an `https://`
   address checked when it is saved, as the tile address is) in the system's browser, from the
   stored setting, never from the page. Hiding a map's tiles hides their attribution with them;
   nothing hides the attribution of tiles on screen.

## Consequences
- The smoke test's tile server is local, as before; it now also checks the User-Agent and that
  no referrer arrives. No test ever asks tile.openstreetmap.org or any other outside host.
- Tiles are cached across maps and restarts; a server that changes its tiles is seen when each
  tile's lifetime ends, as the policy wants.
- Another server's terms are still the user's to read: the app offers only OpenStreetMap, whose
  policy this ADR follows.

## Alternatives
- **The web view keeps fetching tiles, with `referrerpolicy` and the browser cache:** no User-Agent
  of our own and a cache we cannot bound or inspect; it fails the policy's first rule.
- **A custom URI scheme the web view loads tiles from:** the browser would cache and parallelise
  as it likes again, and the content security policy would need the scheme on every platform;
  `data:` images from a command keep every request in the core.
- **Extending `spelaion-https` for tiles:** it is HTTPS at every hop by design (ADR-055) and under
  CODEOWNERS for the plugin catalogue; a tile server may be `http://localhost`, so tiles get a
  transport of their own with the same rules otherwise.
- **OpenStreetMap on by default:** the nicest first look, but it would send every user's view to
  a third party unasked; ADR-045's reason stands.

## Revisit if
OpenStreetMap's policy changes what it asks of applications, or a second public server is to be
offered.
