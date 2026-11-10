//! Geometry values on a map (ADR-045). Each engine stores a geometry its own way: PostGIS as
//! EWKB (its text is that in hexadecimal), MySQL and MariaDB as a four-byte SRID followed by
//! WKB, SpatiaLite in its own blob, GeoPackage files in theirs, DuckDB's spatial extension as
//! WKB. Here every one of them becomes the same GeoJSON geometry, which is the only thing the
//! map view receives: it never parses database bytes itself.
//!
//! The bytes are untrusted: every count is checked against the bytes left before anything is
//! allocated, nesting is bounded, a value must be read to its last byte, and a coordinate that
//! is not a finite number refuses the whole value (an empty point is the one exception). A
//! value that does not read cleanly is simply not a geometry: the viewer shows its text.

use serde::Serialize;
use spelaion_driver_api::Value;

use crate::results::StoredResult;
use crate::value_view::VIEW_LIMIT;

/// The most shapes a column's map holds; the rest of the column is counted, not drawn.
pub const MAP_MAX_SHAPES: usize = 10_000;
/// The most points (vertices) a column's map holds, across all its shapes.
pub const MAP_MAX_POINTS: usize = 500_000;
/// Deepest nesting of collections read (a collection inside a collection…).
const MAX_DEPTH: usize = 32;

/// A position: x (longitude) and y (latitude). Z and M are left out: the map is flat.
pub type Position = [f64; 2];

/// A geometry as GeoJSON writes it (RFC 7946, section 3.1): `serde_json` turns it into that
/// text, and the desktop app hands it to the map as it is.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "bindings", derive(specta::Type))]
#[serde(tag = "type")]
pub enum Geometry {
    /// Empty (`POINT EMPTY`) when `coordinates` is empty; otherwise x and y.
    Point {
        coordinates: Vec<f64>,
    },
    LineString {
        coordinates: Vec<Position>,
    },
    Polygon {
        coordinates: Vec<Vec<Position>>,
    },
    MultiPoint {
        coordinates: Vec<Position>,
    },
    MultiLineString {
        coordinates: Vec<Vec<Position>>,
    },
    MultiPolygon {
        coordinates: Vec<Vec<Vec<Position>>>,
    },
    GeometryCollection {
        geometries: Vec<Geometry>,
    },
}

impl Geometry {
    /// The number of positions it holds.
    pub fn points(&self) -> usize {
        match self {
            Self::Point { coordinates } => usize::from(!coordinates.is_empty()),
            Self::LineString { coordinates } | Self::MultiPoint { coordinates } => {
                coordinates.len()
            }
            Self::Polygon { coordinates } | Self::MultiLineString { coordinates } => {
                coordinates.iter().map(Vec::len).sum()
            }
            Self::MultiPolygon { coordinates } => coordinates
                .iter()
                .flat_map(|polygon| polygon.iter().map(Vec::len))
                .sum(),
            Self::GeometryCollection { geometries } => geometries.iter().map(Self::points).sum(),
        }
    }

    /// The smallest rectangle around every position; `None` when it is empty.
    pub fn bounds(&self) -> Option<Bounds> {
        let mut bounds: Option<Bounds> = None;
        self.each_position(&mut |[x, y]| {
            bounds = Some(match bounds {
                None => Bounds {
                    min_x: x,
                    min_y: y,
                    max_x: x,
                    max_y: y,
                },
                Some(b) => b.with([x, y]),
            });
        });
        bounds
    }

    fn each_position(&self, f: &mut impl FnMut(Position)) {
        match self {
            Self::Point { coordinates } => {
                if let [x, y, ..] = coordinates[..] {
                    f([x, y]);
                }
            }
            Self::LineString { coordinates } | Self::MultiPoint { coordinates } => {
                coordinates.iter().copied().for_each(f);
            }
            Self::Polygon { coordinates } | Self::MultiLineString { coordinates } => {
                coordinates.iter().flatten().copied().for_each(f);
            }
            Self::MultiPolygon { coordinates } => {
                coordinates.iter().flatten().flatten().copied().for_each(f);
            }
            Self::GeometryCollection { geometries } => {
                for g in geometries {
                    g.each_position(f);
                }
            }
        }
    }

    /// The GeoJSON text of this geometry.
    pub fn to_geojson(&self) -> String {
        // Every coordinate is finite (the readers refuse others), so this cannot fail.
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// A rectangle in the geometry's own units.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[cfg_attr(feature = "bindings", derive(specta::Type))]
#[serde(rename_all = "camelCase")]
pub struct Bounds {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

impl Bounds {
    fn with(self, [x, y]: Position) -> Self {
        Self {
            min_x: self.min_x.min(x),
            min_y: self.min_y.min(y),
            max_x: self.max_x.max(x),
            max_y: self.max_y.max(y),
        }
    }

    fn union(self, other: Self) -> Self {
        self.with([other.min_x, other.min_y])
            .with([other.max_x, other.max_y])
    }

    fn fits_lon_lat(self) -> bool {
        self.min_x >= -180.0 && self.max_x <= 180.0 && self.min_y >= -90.0 && self.max_y <= 90.0
    }
}

/// How the stored value was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "bindings", derive(specta::Type))]
#[serde(rename_all = "camelCase")]
pub enum GeoFormat {
    /// OGC well-known binary (DuckDB, and any blob of plain WKB).
    Wkb,
    /// PostGIS's extended WKB, with the SRID inside.
    Ewkb,
    /// MySQL's and MariaDB's: a four-byte SRID, then WKB.
    Mysql,
    /// SpatiaLite's own blob.
    Spatialite,
    /// A GeoPackage geometry: its header, then WKB.
    GeoPackage,
}

/// The coordinates a value had before Z and M were left out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "bindings", derive(specta::Type))]
#[serde(rename_all = "camelCase")]
pub enum Dimensions {
    Xy,
    Xyz,
    Xym,
    Xyzm,
}

impl Dimensions {
    fn new(z: bool, m: bool) -> Self {
        match (z, m) {
            (false, false) => Self::Xy,
            (true, false) => Self::Xyz,
            (false, true) => Self::Xym,
            (true, true) => Self::Xyzm,
        }
    }

    fn doubles(self) -> usize {
        match self {
            Self::Xy => 2,
            Self::Xyz | Self::Xym => 3,
            Self::Xyzm => 4,
        }
    }
}

/// What the coordinates are, which decides whether a web map's tiles can go behind them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "bindings", derive(specta::Type))]
#[serde(rename_all = "camelCase")]
pub enum MapCrs {
    /// Longitude and latitude in degrees, from the SRID (4326 and other geographic ones).
    LonLat,
    /// No SRID (or 0), but every coordinate fits longitude and latitude: read as them.
    AssumedLonLat,
    /// Web Mercator metres (3857), the tiles' own projection.
    WebMercator,
    /// Anything else (a projected system, or no SRID with coordinates past the degrees):
    /// drawn as they are on a plain background, never over tiles.
    Plane,
}

/// SRIDs whose coordinates are longitude and latitude in degrees: WGS 84 and the usual
/// national datums that differ from it by less than a map shows.
const LON_LAT_SRIDS: &[u32] = &[
    4326, 4258, 4269, 4267, 4283, 7844, 4674, 4617, 4612, 6668, 4490, 4171, 4148,
];
const WEB_MERCATOR_SRIDS: &[u32] = &[3857, 900_913, 3785, 102_100, 102_113];

/// What coordinates with this SRID and these bounds are.
pub fn map_crs(srid: Option<u32>, bounds: Option<Bounds>) -> MapCrs {
    let fits = bounds.is_none_or(Bounds::fits_lon_lat);
    match srid {
        Some(srid) if LON_LAT_SRIDS.contains(&srid) && fits => MapCrs::LonLat,
        Some(srid) if WEB_MERCATOR_SRIDS.contains(&srid) => MapCrs::WebMercator,
        Some(_) => MapCrs::Plane,
        None if fits => MapCrs::AssumedLonLat,
        None => MapCrs::Plane,
    }
}

/// A cell read as a geometry.
#[derive(Debug, Clone, PartialEq)]
pub struct GeoValue {
    pub geometry: Geometry,
    /// `None` when the value has none or says 0.
    pub srid: Option<u32>,
    pub format: GeoFormat,
    pub dimensions: Dimensions,
}

impl GeoValue {
    pub fn crs(&self) -> MapCrs {
        map_crs(self.srid, self.geometry.bounds())
    }
}

/// Whether an engine's type name is a spatial type (`geometry`, `geography`, `GEOMETRY`,
/// `POINT`, `MULTIPOLYGON`…), as PostGIS, MySQL, DuckDB and SpatiaLite's columns name them.
pub fn is_geometry_type(native_type: &str) -> bool {
    let name = native_type.trim().to_ascii_lowercase();
    let name = name.rsplit('.').next().unwrap_or(&name);
    let name = name.split(['(', ' ']).next().unwrap_or(name);
    matches!(
        name,
        "geometry"
            | "geography"
            | "point"
            | "linestring"
            | "polygon"
            | "multipoint"
            | "multilinestring"
            | "multipolygon"
            | "geometrycollection"
            | "geomcollection"
    )
}

/// The cell as a geometry, if it is one. Bytes are recognised by their format (SpatiaLite's
/// and GeoPackage's blobs by their markers, WKB and MySQL's by reading cleanly to the end), in
/// any column; text only in a spatial column (PostGIS's hexadecimal EWKB).
pub fn read_geometry(value: &Value, native_type: &str) -> Option<GeoValue> {
    let spatial = is_geometry_type(native_type);
    match value {
        Value::Bytes(bytes) => read_bytes(bytes, spatial),
        Value::Text(text) | Value::Native(text) if spatial => {
            let bytes = decode_hex(text.trim())?;
            let (value, ewkb) = read_wkb(&bytes)?;
            Some(GeoValue {
                format: if ewkb {
                    GeoFormat::Ewkb
                } else {
                    GeoFormat::Wkb
                },
                ..value
            })
        }
        Value::Null
        | Value::Bool(_)
        | Value::Int(_)
        | Value::Float(_)
        | Value::Decimal(_)
        | Value::Text(_)
        | Value::Date(_)
        | Value::Time(_)
        | Value::Timestamp(_)
        | Value::TimestampTz(_)
        | Value::Uuid(_)
        | Value::Json(_)
        | Value::Interval(_)
        | Value::Array(_)
        | Value::Native(_) => None,
    }
}

fn read_bytes(bytes: &[u8], spatial: bool) -> Option<GeoValue> {
    if let Some(value) = read_geopackage(bytes) {
        return Some(value);
    }
    if let Some(value) = read_spatialite(bytes) {
        return Some(value);
    }
    let plain = read_wkb(bytes)
        .map(|(value, ewkb)| GeoValue {
            format: if ewkb {
                GeoFormat::Ewkb
            } else {
                GeoFormat::Wkb
            },
            ..value
        })
        .or_else(|| read_mysql(bytes));
    // Outside a spatial column a blob that happens to read as an empty geometry is more
    // likely something else.
    plain.filter(|value| spatial || value.geometry.points() > 0)
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    let text = text.strip_prefix("\\x").unwrap_or(text);
    if text.is_empty() || !text.len().is_multiple_of(2) {
        return None;
    }
    let digit = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    text.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|[high, low]| Some(digit(*high)? << 4 | digit(*low)?))
        .collect()
}

/// A reader over untrusted bytes. Every read checks what is left.
struct Bytes<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Bytes<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }

    fn left(&self) -> usize {
        self.data.len() - self.at
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(n)?;
        let slice = self.data.get(self.at..end)?;
        self.at = end;
        Some(slice)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }

    fn u32(&mut self, little: bool) -> Option<u32> {
        let b: [u8; 4] = self.take(4)?.try_into().ok()?;
        Some(if little {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    }

    fn f64(&mut self, little: bool) -> Option<f64> {
        let b: [u8; 8] = self.take(8)?.try_into().ok()?;
        Some(if little {
            f64::from_le_bytes(b)
        } else {
            f64::from_be_bytes(b)
        })
    }

    fn f32(&mut self, little: bool) -> Option<f32> {
        let b: [u8; 4] = self.take(4)?.try_into().ok()?;
        Some(if little {
            f32::from_le_bytes(b)
        } else {
            f32::from_be_bytes(b)
        })
    }

    /// A count of items of at least `min_size` bytes each: refused when the bytes left
    /// cannot hold them, so a forged count never allocates.
    fn count(&mut self, little: bool, min_size: usize) -> Option<usize> {
        let n = self.u32(little)? as usize;
        (n.checked_mul(min_size)? <= self.left()).then_some(n)
    }

    /// A position of `dims`, keeping x and y.
    fn position(&mut self, little: bool, dims: Dimensions) -> Option<Position> {
        let x = self.f64(little)?;
        let y = self.f64(little)?;
        for _ in 2..dims.doubles() {
            self.f64(little)?;
        }
        finite([x, y])
    }

    fn positions(&mut self, little: bool, dims: Dimensions) -> Option<Vec<Position>> {
        let n = self.count(little, 8 * dims.doubles())?;
        (0..n).map(|_| self.position(little, dims)).collect()
    }

    fn rings(&mut self, little: bool, dims: Dimensions) -> Option<Vec<Vec<Position>>> {
        let n = self.count(little, 4)?;
        (0..n).map(|_| self.positions(little, dims)).collect()
    }
}

fn finite(position: Position) -> Option<Position> {
    position.iter().all(|c| c.is_finite()).then_some(position)
}

/// The seven simple feature types of WKB, SpatiaLite and GeoPackage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Point,
    LineString,
    Polygon,
    MultiPoint,
    MultiLineString,
    MultiPolygon,
    Collection,
}

impl Kind {
    fn from_code(code: u32) -> Option<Self> {
        Some(match code {
            1 => Self::Point,
            2 => Self::LineString,
            3 => Self::Polygon,
            4 => Self::MultiPoint,
            5 => Self::MultiLineString,
            6 => Self::MultiPolygon,
            7 => Self::Collection,
            // Curves, surfaces, TINs and triangles are not drawn.
            _ => return None,
        })
    }
}

/// Plain WKB (ISO, with Z and M as +1000, +2000, +3000) or PostGIS's EWKB (flags in the
/// high bits, SRID inside). The value and whether it was EWKB.
fn read_wkb(data: &[u8]) -> Option<(GeoValue, bool)> {
    let mut bytes = Bytes::new(data);
    let mut header = WkbHeader::default();
    let geometry = wkb_geometry(&mut bytes, 0, &mut header)?;
    (bytes.left() == 0).then_some((
        GeoValue {
            geometry,
            srid: header.srid.filter(|&s| s != 0),
            format: GeoFormat::Wkb,
            dimensions: header.dimensions.unwrap_or(Dimensions::Xy),
        },
        header.ewkb,
    ))
}

#[derive(Default)]
struct WkbHeader {
    srid: Option<u32>,
    ewkb: bool,
    /// The outermost geometry's.
    dimensions: Option<Dimensions>,
}

fn wkb_geometry(bytes: &mut Bytes, depth: usize, header: &mut WkbHeader) -> Option<Geometry> {
    if depth > MAX_DEPTH {
        return None;
    }
    let little = match bytes.u8()? {
        0 => false,
        1 => true,
        _ => return None,
    };
    let code = bytes.u32(little)?;
    const EWKB_Z: u32 = 0x8000_0000;
    const EWKB_M: u32 = 0x4000_0000;
    const EWKB_SRID: u32 = 0x2000_0000;
    let flags = code & (EWKB_Z | EWKB_M | EWKB_SRID);
    let iso = code & !(EWKB_Z | EWKB_M | EWKB_SRID);
    if flags != 0 {
        header.ewkb = true;
    }
    if code & EWKB_SRID != 0 {
        // Only the outermost geometry carries one.
        if depth > 0 {
            return None;
        }
        header.srid = Some(bytes.u32(little)?);
    }
    let (iso_z, iso_m) = match iso / 1000 {
        0 => (false, false),
        1 => (true, false),
        2 => (false, true),
        3 => (true, true),
        _ => return None,
    };
    // One way or the other, never both.
    if flags & (EWKB_Z | EWKB_M) != 0 && (iso_z || iso_m) {
        return None;
    }
    let dims = Dimensions::new(iso_z || code & EWKB_Z != 0, iso_m || code & EWKB_M != 0);
    header.dimensions.get_or_insert(dims);
    let kind = Kind::from_code(iso % 1000)?;
    Some(match kind {
        Kind::Point => {
            let x = bytes.f64(little)?;
            let y = bytes.f64(little)?;
            let mut rest = Vec::new();
            for _ in 2..dims.doubles() {
                rest.push(bytes.f64(little)?);
            }
            point([x, y], &rest)?
        }
        Kind::LineString => Geometry::LineString {
            coordinates: bytes.positions(little, dims)?,
        },
        Kind::Polygon => Geometry::Polygon {
            coordinates: bytes.rings(little, dims)?,
        },
        Kind::MultiPoint | Kind::MultiLineString | Kind::MultiPolygon | Kind::Collection => {
            // Each part is at least a byte order and a type.
            let n = bytes.count(little, 5)?;
            let parts = (0..n)
                .map(|_| wkb_geometry(bytes, depth + 1, header))
                .collect::<Option<Vec<_>>>()?;
            multi(kind, parts)?
        }
    })
}

/// A point, empty when every coordinate is NaN (how WKB writes `POINT EMPTY`).
fn point([x, y]: Position, rest: &[f64]) -> Option<Geometry> {
    if x.is_nan() && y.is_nan() && rest.iter().all(|c| c.is_nan()) {
        return Some(Geometry::Point {
            coordinates: Vec::new(),
        });
    }
    let [x, y] = finite([x, y])?;
    Some(Geometry::Point {
        coordinates: vec![x, y],
    })
}

/// A multi-geometry from its parts, which must be of the matching kind (any kind in a
/// collection). Empty points inside a multipoint are left out.
fn multi(kind: Kind, parts: Vec<Geometry>) -> Option<Geometry> {
    Some(match kind {
        Kind::MultiPoint => Geometry::MultiPoint {
            coordinates: parts
                .into_iter()
                .map(|p| match p {
                    Geometry::Point { coordinates } => Some(match coordinates[..] {
                        [x, y] => Some([x, y]),
                        _ => None,
                    }),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()?
                .into_iter()
                .flatten()
                .collect(),
        },
        Kind::MultiLineString => Geometry::MultiLineString {
            coordinates: parts
                .into_iter()
                .map(|p| match p {
                    Geometry::LineString { coordinates } => Some(coordinates),
                    _ => None,
                })
                .collect::<Option<_>>()?,
        },
        Kind::MultiPolygon => Geometry::MultiPolygon {
            coordinates: parts
                .into_iter()
                .map(|p| match p {
                    Geometry::Polygon { coordinates } => Some(coordinates),
                    _ => None,
                })
                .collect::<Option<_>>()?,
        },
        Kind::Collection => Geometry::GeometryCollection { geometries: parts },
        Kind::Point | Kind::LineString | Kind::Polygon => return None,
    })
}

/// MySQL's and MariaDB's storage: the SRID in four little-endian bytes, then WKB.
fn read_mysql(data: &[u8]) -> Option<GeoValue> {
    let srid = u32::from_le_bytes(data.get(..4)?.try_into().ok()?);
    let (value, ewkb) = read_wkb(&data[4..])?;
    // MySQL's WKB never has PostGIS's flags.
    (!ewkb).then_some(GeoValue {
        srid: (srid != 0).then_some(srid),
        format: GeoFormat::Mysql,
        ..value
    })
}

/// A GeoPackage geometry (GeoPackage 1.3, section 2.1.3): `GP`, version 0, flags, the SRS id,
/// an optional envelope, then WKB. Extended geometries are not drawn.
fn read_geopackage(data: &[u8]) -> Option<GeoValue> {
    let mut bytes = Bytes::new(data);
    if bytes.take(2)? != b"GP" || bytes.u8()? != 0 {
        return None;
    }
    let flags = bytes.u8()?;
    if flags & 0b1100_0000 != 0 || flags & 0b0010_0000 != 0 {
        // Reserved bits, or an extended geometry.
        return None;
    }
    let little = flags & 1 == 1;
    let envelope = match (flags >> 1) & 0b111 {
        0 => 0,
        1 => 32,
        2 | 3 => 48,
        4 => 64,
        _ => return None,
    };
    let srs = bytes.u32(little)? as i32;
    bytes.take(envelope)?;
    let (value, ewkb) = read_wkb(&data[bytes.at..])?;
    (!ewkb).then_some(GeoValue {
        // -1 and 0 are GeoPackage's "undefined" systems.
        srid: u32::try_from(srs).ok().filter(|&s| s != 0),
        format: GeoFormat::GeoPackage,
        ..value
    })
}

/// SpatiaLite's blob ("BLOB-Geometry" in SpatiaLite's documentation): `0x00`, the byte order,
/// the SRID, the bounding rectangle, `0x7C`, the class and its data, `0xFE`. Compressed
/// lines and polygons (their middle points as float offsets) are read too.
fn read_spatialite(data: &[u8]) -> Option<GeoValue> {
    if data.len() < 44 || data[0] != 0x00 || data[38] != 0x7C || data.last() != Some(&0xFE) {
        return None;
    }
    let little = match data[1] {
        0 => false,
        1 => true,
        _ => return None,
    };
    let mut bytes = Bytes::new(&data[..data.len() - 1]);
    bytes.take(2)?;
    let srid = bytes.u32(little)?;
    bytes.take(32)?;
    bytes.u8()?;
    let class = bytes.u32(little)?;
    let mut dimensions = None;
    let geometry = spatialite_geometry(&mut bytes, little, class, 0, &mut dimensions)?;
    (bytes.left() == 0).then_some(GeoValue {
        geometry,
        srid: (srid != 0).then_some(srid),
        format: GeoFormat::Spatialite,
        dimensions: dimensions.unwrap_or(Dimensions::Xy),
    })
}

fn spatialite_geometry(
    bytes: &mut Bytes,
    little: bool,
    class: u32,
    depth: usize,
    dimensions: &mut Option<Dimensions>,
) -> Option<Geometry> {
    if depth > MAX_DEPTH {
        return None;
    }
    let compressed = class >= 1_000_000;
    let code = class % 1_000_000;
    let dims = match code / 1000 {
        0 => Dimensions::Xy,
        1 => Dimensions::Xyz,
        2 => Dimensions::Xym,
        3 => Dimensions::Xyzm,
        _ => return None,
    };
    dimensions.get_or_insert(dims);
    let kind = Kind::from_code(code % 1000)?;
    let line = |bytes: &mut Bytes| {
        if compressed {
            compressed_positions(bytes, little, dims)
        } else {
            bytes.positions(little, dims)
        }
    };
    Some(match kind {
        Kind::Point if !compressed => {
            let x = bytes.f64(little)?;
            let y = bytes.f64(little)?;
            let mut rest = Vec::new();
            for _ in 2..dims.doubles() {
                rest.push(bytes.f64(little)?);
            }
            point([x, y], &rest)?
        }
        Kind::LineString => Geometry::LineString {
            coordinates: line(bytes)?,
        },
        Kind::Polygon => {
            let n = bytes.count(little, 4)?;
            Geometry::Polygon {
                coordinates: (0..n).map(|_| line(bytes)).collect::<Option<_>>()?,
            }
        }
        Kind::MultiPoint | Kind::MultiLineString | Kind::MultiPolygon | Kind::Collection
            if !compressed =>
        {
            // Each entity is at least its marker and its class.
            let n = bytes.count(little, 5)?;
            let parts = (0..n)
                .map(|_| {
                    if bytes.u8()? != 0x69 {
                        return None;
                    }
                    let class = bytes.u32(little)?;
                    spatialite_geometry(bytes, little, class, depth + 1, dimensions)
                })
                .collect::<Option<Vec<_>>>()?;
            multi(kind, parts)?
        }
        // Only lines and polygons have a compressed form.
        Kind::Point
        | Kind::MultiPoint
        | Kind::MultiLineString
        | Kind::MultiPolygon
        | Kind::Collection => return None,
    })
}

/// A compressed SpatiaLite line or ring: its first and last points whole, the ones between
/// as float offsets from the previous point (X, Y and Z; M stays a whole double).
fn compressed_positions(
    bytes: &mut Bytes,
    little: bool,
    dims: Dimensions,
) -> Option<Vec<Position>> {
    // Intermediate points are 8 bytes for X and Y, plus 4 for Z and 8 for M.
    let middle = match dims {
        Dimensions::Xy => 8,
        Dimensions::Xyz => 12,
        Dimensions::Xym => 16,
        Dimensions::Xyzm => 20,
    };
    let n = bytes.count(little, middle)?;
    let mut out = Vec::with_capacity(n);
    let mut last = [0.0, 0.0];
    for i in 0..n {
        let position = if i == 0 || i == n - 1 {
            bytes.position(little, dims)?
        } else {
            let dx = f64::from(bytes.f32(little)?);
            let dy = f64::from(bytes.f32(little)?);
            match dims {
                Dimensions::Xy => {}
                Dimensions::Xyz => {
                    bytes.f32(little)?;
                }
                Dimensions::Xym => {
                    bytes.f64(little)?;
                }
                Dimensions::Xyzm => {
                    bytes.f32(little)?;
                    bytes.f64(little)?;
                }
            }
            finite([last[0] + dx, last[1] + dy])?
        };
        last = position;
        out.push(position);
    }
    Some(out)
}

/// One shape of a column's map: the row it is on (a position of the current view) and its
/// geometry.
#[derive(Debug, Clone, PartialEq)]
pub struct MapShape {
    pub row: usize,
    pub geometry: Geometry,
}

/// A column of a result on one map.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnMap {
    pub shapes: Vec<MapShape>,
    /// One SRID when every shape that has one shares it.
    pub srid: Option<u32>,
    /// Shapes do not share one SRID.
    pub mixed_srids: bool,
    pub crs: MapCrs,
    pub bounds: Option<Bounds>,
    /// Rows of the current view read.
    pub rows_read: usize,
    pub nulls: usize,
    /// Values that are not a geometry this map can draw.
    pub unreadable: usize,
    /// Values past [`VIEW_LIMIT`], left out like the cell viewer withholds them.
    pub too_big: usize,
    /// The map stopped at [`MAP_MAX_SHAPES`] or [`MAP_MAX_POINTS`] before the last row.
    pub truncated: bool,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MapError {
    #[error("there is no column {0} in this result")]
    NoSuchColumn(usize),
}

/// Every geometry of `column` in the current view of `result` (the grid's sort and filters),
/// in row order, up to [`MAP_MAX_SHAPES`] shapes and [`MAP_MAX_POINTS`] points.
pub fn column_map(result: &StoredResult, column: usize) -> Result<ColumnMap, MapError> {
    let meta = result
        .schema()
        .columns
        .get(column)
        .ok_or(MapError::NoSuchColumn(column))?;
    let mut map = ColumnMap {
        shapes: Vec::new(),
        srid: None,
        mixed_srids: false,
        crs: MapCrs::Plane,
        bounds: None,
        rows_read: 0,
        nulls: 0,
        unreadable: 0,
        too_big: 0,
        truncated: false,
    };
    let mut points = 0usize;
    let mut srids: Option<Option<u32>> = None;
    for row in 0..result.rows() {
        let Some(value) = result.value_at(row, column) else {
            break;
        };
        let size = match &value {
            Value::Null => {
                map.rows_read += 1;
                map.nulls += 1;
                continue;
            }
            Value::Bytes(bytes) => bytes.len(),
            other => other.to_string().len(),
        };
        if size > VIEW_LIMIT {
            map.rows_read += 1;
            map.too_big += 1;
            continue;
        }
        let Some(geo) = read_geometry(&value, &meta.native_type) else {
            map.rows_read += 1;
            map.unreadable += 1;
            continue;
        };
        let count = geo.geometry.points();
        if map.shapes.len() == MAP_MAX_SHAPES || points + count > MAP_MAX_POINTS {
            map.truncated = true;
            break;
        }
        map.rows_read += 1;
        points += count;
        match srids {
            None => srids = Some(geo.srid),
            Some(srid) if srid != geo.srid => map.mixed_srids = true,
            Some(_) => {}
        }
        if let Some(b) = geo.geometry.bounds() {
            map.bounds = Some(map.bounds.map_or(b, |all| all.union(b)));
        }
        map.shapes.push(MapShape {
            row,
            geometry: geo.geometry,
        });
    }
    map.srid = if map.mixed_srids {
        None
    } else {
        srids.flatten()
    };
    map.crs = if map.mixed_srids {
        MapCrs::Plane
    } else {
        map_crs(map.srid, map.bounds)
    };
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        decode_hex(text).unwrap()
    }

    #[test]
    fn spatial_type_names() {
        for name in [
            "geometry",
            "GEOMETRY",
            "geography",
            "public.geometry",
            "POINT",
            "MultiPolygon",
            "geometry(Point,4326)",
            "GEOMCOLLECTION",
        ] {
            assert!(is_geometry_type(name), "{name}");
        }
        for name in ["BLOB", "bytea", "text", "varchar(10)", "points", "geo"] {
            assert!(!is_geometry_type(name), "{name}");
        }
    }

    #[test]
    fn wkb_in_both_byte_orders_and_dimensions() {
        // POINT(1 2), little and big endian.
        let little = hex("0101000000000000000000F03F0000000000000040");
        let big = hex("00000000013FF00000000000004000000000000000");
        for bytes in [little, big] {
            let (value, ewkb) = read_wkb(&bytes).unwrap();
            assert!(!ewkb);
            assert_eq!(
                value.geometry.to_geojson(),
                r#"{"type":"Point","coordinates":[1.0,2.0]}"#
            );
            assert_eq!(value.dimensions, Dimensions::Xy);
        }
        // ISO POINT Z (1 2 3): Z is read and left out.
        let (z, _) = read_wkb(&hex(
            "01E9030000000000000000F03F00000000000000400000000000000840",
        ))
        .unwrap();
        assert_eq!(z.dimensions, Dimensions::Xyz);
        assert_eq!(
            z.geometry.to_geojson(),
            r#"{"type":"Point","coordinates":[1.0,2.0]}"#
        );
        // EWKB POINT(1 2) with SRID 4326 and Z flag.
        let (e, ewkb) = read_wkb(&hex(
            "01010000A0E6100000000000000000F03F00000000000000400000000000000840",
        ))
        .unwrap();
        assert!(ewkb);
        assert_eq!(e.srid, Some(4326));
        assert_eq!(e.dimensions, Dimensions::Xyz);
        // POINT EMPTY (NaN coordinates).
        let (empty, _) = read_wkb(&hex("0101000000000000000000F87F000000000000F87F")).unwrap();
        assert_eq!(
            empty.geometry.to_geojson(),
            r#"{"type":"Point","coordinates":[]}"#
        );
        assert_eq!(empty.geometry.bounds(), None);
    }

    #[test]
    fn broken_or_hostile_bytes_are_not_geometries() {
        for text in [
            "",
            // Cut short.
            "0101000000000000000000F03F00000000000000",
            // A trailing byte.
            "0101000000000000000000F03F000000000000004000",
            // Bad byte order, unknown type, a curve.
            "0201000000000000000000F03F0000000000000040",
            "0163000000000000000000F03F0000000000000040",
            "0108000000",
            // A line claiming four billion points.
            "0102000000FFFFFFFF",
            // An infinite coordinate.
            "0101000000000000000000F07F0000000000000040",
            // ISO and EWKB dimensions at once.
            "01E9030080000000000000F03F00000000000000400000000000000840",
            // A multipolygon holding a point.
            "0106000000010000000101000000000000000000F03F0000000000000040",
        ] {
            assert_eq!(read_bytes(&hex_or_empty(text), true), None, "{text}");
        }
        // Collections nested past the limit.
        let mut deep = Vec::new();
        for _ in 0..=MAX_DEPTH + 1 {
            deep.extend_from_slice(&hex("010700000001000000"));
        }
        deep.extend_from_slice(&hex("0101000000000000000000F03F0000000000000040"));
        assert_eq!(read_wkb(&deep), None);
        // Text is read only in a spatial column.
        let text = Value::Text("0101000000000000000000F03F0000000000000040".into());
        assert!(read_geometry(&text, "geometry").is_some());
        assert_eq!(read_geometry(&text, "text"), None);
        assert_eq!(
            read_geometry(&Value::Text("not hex".into()), "geometry"),
            None
        );
        // An empty geometry in a blob column is not taken for one.
        let empty_collection = Value::Bytes(hex("010700000000000000"));
        assert_eq!(read_geometry(&empty_collection, "BLOB"), None);
        assert!(read_geometry(&empty_collection, "GEOMETRY").is_some());
    }

    fn hex_or_empty(text: &str) -> Vec<u8> {
        if text.is_empty() {
            Vec::new()
        } else {
            hex(text)
        }
    }

    #[test]
    fn lon_lat_is_decided_by_the_srid_and_the_coordinates() {
        let small = Some(Bounds {
            min_x: -3.7,
            min_y: 40.4,
            max_x: 2.1,
            max_y: 41.4,
        });
        let big = Some(Bounds {
            min_x: 440_000.0,
            min_y: 4_470_000.0,
            max_x: 450_000.0,
            max_y: 4_480_000.0,
        });
        assert_eq!(map_crs(Some(4326), small), MapCrs::LonLat);
        assert_eq!(map_crs(Some(4326), big), MapCrs::Plane);
        assert_eq!(map_crs(Some(3857), big), MapCrs::WebMercator);
        assert_eq!(map_crs(Some(25830), big), MapCrs::Plane);
        assert_eq!(map_crs(Some(25830), small), MapCrs::Plane);
        assert_eq!(map_crs(None, small), MapCrs::AssumedLonLat);
        assert_eq!(map_crs(None, big), MapCrs::Plane);
        assert_eq!(map_crs(None, None), MapCrs::AssumedLonLat);
    }
}
