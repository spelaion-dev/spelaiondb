# ADR-045: Geometry values on a map: GeoJSON from the core, drawn as SVG, tiles only when the user names a server

- **Status:** Accepted (implemented)
- **Date:** 2026-11-10
- **Builds on:** ADR-010 (results live in the core), ADR-040 (plain SVG, no chart library),
  ADR-041 / ADR-042 (anything a driver adds travels as owned data)

## Context
A geometry in the grid is unreadable: PostGIS shows hexadecimal EWKB, MySQL and SpatiaLite
binary blobs. People want to see the shape, and often all the shapes of a column together.
Each engine stores geometries differently (PostGIS's EWKB, MySQL's SRID plus WKB, SpatiaLite's
own blob, GeoPackage's header plus WKB, DuckDB's WKB through Arrow), and those bytes come from
the database: they are untrusted input like any other value.

A map usually sits on tiles from an online server, and every tile requested tells that server
which part of the world the user is looking at, from which address, and when. For a database
client that may show customers' addresses or a company's assets, that is a leak nobody should
get by opening a cell.

## Decision
1. **The core reads every format into one: GeoJSON** (`spelaion-core::geometry`). `read_geometry`
   takes a cell and its column's type name: bytes are recognised by format (SpatiaLite's markers,
   GeoPackage's `GP` header, then WKB and MySQL's SRID + WKB, which must read cleanly to the last
   byte) in any column; text only in a spatial column (PostGIS's hexadecimal EWKB). Points, lines,
   polygons, their multi forms and collections are read; curves, surfaces and TINs are not drawn,
   and Z and M are read and left out (the viewer says so). The view receives GeoJSON, never the
   database's bytes, and never parses them itself.
2. **Untrusted bytes stay bounded.** Every count is checked against the bytes left before
   anything is allocated, nesting stops at 32 levels, a value must be read to its end, and a
   coordinate that is not finite refuses the whole value (WKB's all-NaN empty point is the one
   exception). A value past the viewer's limit (`VIEW_LIMIT`) is not read, like any withheld
   value.
3. **The cell panel draws the map.** A cell the core read as a geometry gets "Map" next to
   "Value" in the cell panel: the value alone, or the whole column of the result's current view
   (the grid's sort and filters), at most 10,000 shapes and 500,000 points, read by the core
   (`column_map`), which counts NULLs, values that are not geometries, values too big to draw and
   whether it stopped early, and the panel says each of them. Clicking a shape of the column's map
   selects its row.
4. **Plain SVG, no map library**, as charts are (ADR-040): longitude and latitude are projected
   to Web Mercator, anything else is drawn as plane coordinates; pan, zoom and fit by pointer and
   keyboard. Colors are theme tokens (`--map-*`).
5. **What the coordinates are** is decided in the core (`MapCrs`): longitude and latitude from a
   geographic SRID (4326 and the usual national datums) when the coordinates fit; Web Mercator
   from 3857; with no SRID (DuckDB, MySQL's 0) coordinates that fit the degrees are read as
   longitude and latitude and the panel says "read as"; anything else, and a column whose shapes
   have different SRIDs, is plane coordinates. MySQL stores longitude as X whatever its WKT's axis
   order, so X is always the longitude drawn.
6. **Tiles are off by default and come only from a server the user names.** Settings › Map holds
   one tile server: a name, an address with `{z}`, `{x}` and `{y}`, and its attribution, checked by
   the core before it is saved (`check_tile_server`: `https://`, or `http://` only to this
   computer; no user name in the address, no other fields, no quotes, spaces or control
   characters). The app suggests no server: the user picks one whose terms allow it. Tiles show
   only when the user turned them on, only behind longitude and latitude or Web Mercator
   coordinates, with the attribution on the map and a toggle to hide them for that map; they are
   requested as images without a referrer. With tiles off the map asks nothing of the network,
   which the smoke test checks.
7. **The DuckDB driver names its geometries:** a column that DuckDB's spatial extension hands to
   Arrow as `geoarrow.wkb` reports `GEOMETRY` as its type (a string in the column's metadata, as
   ADR-041 wants), so the panel and `read_geometry` know it.

## Consequences
- PostGIS, MySQL and MariaDB, SpatiaLite and GeoPackage files, and DuckDB's spatial extension
  are drawn the same way, and a blob of plain WKB in any engine is too. SQL Server's `geometry`
  and `geography` (its own serialization) and well-known text are not read yet.
- The test PostgreSQL is the PostGIS image, with the extension only in a `spelaion_gis`
  database, so `spelaion`'s catalog is unchanged; SpatiaLite's blobs are fixtures written by a
  real SpatiaLite, and DuckDB's spatial test installs the extension when it can (it downloads
  once).
- A map of a table data tab shows its page only, like charts.

## Alternatives
- **A map library (Leaflet, MapLibre, OpenLayers):** tiles, projections and gestures for free,
  but 40 kB to 800 kB more JavaScript, its own styling, and in MapLibre's case WebGL; its default
  examples point at public tile servers. Our needs (draw shapes, pan, zoom, optional raster tiles)
  are a few hundred lines.
- **Tiles on by default from a public server (OpenStreetMap):** the nicest first look, but it
  sends every viewer's position to a third party without asking, and OpenStreetMap's tile policy
  does not welcome apps using its servers by default.
- **Fetching tiles through the core:** the core could proxy tile requests and strip headers, but
  it would add an HTTP client to the core for images the web view can fetch itself without a
  referrer; the privacy decision is the user's server choice, not the transport.
- **Parsing in the view (wkx, wellknown in JavaScript):** one less step, but untrusted bytes
  would be parsed outside the core, and each engine's quirks (SpatiaLite, MySQL's prefix) would
  live in React.
