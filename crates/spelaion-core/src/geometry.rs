//! Geometry values on a map (ADR-045). Each engine stores a geometry its own way: PostGIS as
//! EWKB (its text is that in hexadecimal), MySQL and MariaDB as a four-byte SRID followed by
//! WKB, SpatiaLite in its own blob, GeoPackage files in theirs, DuckDB's spatial extension as
//! WKB, SQL Server in its CLR serialization (MS-SSCLRT). Here every one of them becomes the same
//! GeoJSON geometry, which is the only thing the map view receives: it never parses database
//! bytes itself.
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
    /// SQL Server's `geometry` or `geography`, in its CLR serialization (MS-SSCLRT).
    SqlServer,
    /// Well-known text, or PostGIS's extended form with `SRID=…;` before it.
    Wkt,
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
/// any column, and SQL Server's in a column of a CLR type. Text is a geometry when the whole
/// value is well-known text (SDB-214), in any column, or in a spatial column PostGIS's
/// hexadecimal EWKB.
pub fn read_geometry(value: &Value, native_type: &str) -> Option<GeoValue> {
    read_cell_geometry(value, native_type).ok().flatten()
}

/// [`read_geometry`], and why a value that is a geometry is not drawn ([`MapNote`]).
pub fn read_cell_geometry(value: &Value, native_type: &str) -> Result<Option<GeoValue>, MapNote> {
    let spatial = is_geometry_type(native_type);
    Ok(match value {
        Value::Bytes(bytes) => {
            let clr = match clr_type(native_type) {
                Some(clr) => read_clr(bytes, clr)?,
                None => None,
            };
            clr.filter(|value| spatial || value.geometry.points() > 0)
                .or_else(|| read_bytes(bytes, spatial))
        }
        Value::Text(text) | Value::Native(text) => {
            let text = text.trim();
            spatial
                .then(|| read_hex_wkb(text))
                .flatten()
                .or_else(|| read_wkt(text))
                // Outside a spatial column, empty text geometries are more likely something else.
                .filter(|value| spatial || value.geometry.points() > 0)
        }
        Value::Null
        | Value::Bool(_)
        | Value::Int(_)
        | Value::Float(_)
        | Value::Decimal(_)
        | Value::Date(_)
        | Value::Time(_)
        | Value::Timestamp(_)
        | Value::TimestampTz(_)
        | Value::Uuid(_)
        | Value::Json(_)
        | Value::Interval(_)
        | Value::Array(_) => None,
    })
}

/// PostGIS's text for a geometry: its EWKB in hexadecimal.
fn read_hex_wkb(text: &str) -> Option<GeoValue> {
    let bytes = decode_hex(text)?;
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

/// The CLR type of a SQL Server column, as far as its native type tells: its two spatial
/// types are serialized alike, but geography keeps latitude before longitude.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClrType {
    Geometry,
    Geography,
    /// A CLR type the driver did not name (`udt`): see [`read_clr`].
    Unnamed,
}

/// Why a value that is a geometry is not drawn on the map.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "bindings", derive(specta::Type))]
#[serde(rename_all = "camelCase")]
pub enum MapNote {
    /// A SQL Server spatial value with an SRID in a column the driver did not name (`udt`):
    /// it may be geometry (x first) or geography (latitude first), and is not guessed. A
    /// SQL Server plugin that names the column's type draws it.
    SqlServerTypeUnknown,
}

/// A SQL Server spatial value as its column's CLR type says. A value of a type the driver did
/// not name is geometry when it has no SRID (geography always has one); with an SRID it could
/// be either, and drawing a geometry with its axes swapped would mislead: it is not drawn,
/// and says why.
fn read_clr(bytes: &[u8], clr: ClrType) -> Result<Option<GeoValue>, MapNote> {
    match clr {
        ClrType::Geometry => Ok(read_sqlserver(bytes, false)),
        ClrType::Geography => Ok(read_sqlserver(bytes, true)),
        ClrType::Unnamed => match read_sqlserver(bytes, false) {
            Some(value) if value.srid.is_some() => Err(MapNote::SqlServerTypeUnknown),
            other => Ok(other),
        },
    }
}

/// Whether a column's values may be SQL Server's spatial serialization, and of which type.
fn clr_type(native_type: &str) -> Option<ClrType> {
    let name = native_type.trim().to_ascii_lowercase();
    match name.rsplit('.').next().unwrap_or(&name) {
        "geometry" => Some(ClrType::Geometry),
        "geography" => Some(ClrType::Geography),
        "udt" => Some(ClrType::Unnamed),
        _ => None,
    }
}

/// One figure of a SQL Server value: a run of its points (a ring, a line or a point).
struct Figure {
    first_point: usize,
}

/// One shape: its parent shape (none for the root), its first figure (none when empty) and its
/// OpenGIS type.
struct Shape {
    parent: Option<usize>,
    first_figure: Option<usize>,
    kind: u8,
}

/// SQL Server's `geometry` and `geography` (MS-SSCLRT, versions 1 and 2), little-endian: the
/// SRID, the version, the properties (Z, M, valid, a single point, a single line segment,
/// larger than a hemisphere), then either that point or segment, or every point, its Z and M
/// values, the figures (runs of points) and the shapes (a tree of OpenGIS types over the
/// figures). Arcs, curves and the full globe are not drawn. `geography` reads each point
/// latitude first, as geography stores it.
fn read_sqlserver(data: &[u8], geography: bool) -> Option<GeoValue> {
    let mut bytes = Bytes::new(data);
    let srid = bytes.u32(true)? as i32;
    let version = bytes.u8()?;
    let known_flags = match version {
        1 => 0b1_1111,
        2 => 0b11_1111,
        _ => return None,
    };
    let flags = bytes.u8()?;
    if flags & !known_flags != 0 {
        return None;
    }
    let (z, m) = (flags & 0x01 != 0, flags & 0x02 != 0);
    let (single_point, single_segment) = (flags & 0x08 != 0, flags & 0x10 != 0);
    if single_point && single_segment {
        return None;
    }
    let dimensions = Dimensions::new(z, m);
    // Z and M follow all the points, a double per point each.
    let extra = usize::from(z) + usize::from(m);
    let point = |bytes: &mut Bytes| {
        let a = bytes.f64(true)?;
        let b = bytes.f64(true)?;
        finite(if geography { [b, a] } else { [a, b] })
    };
    let geometry = if single_point || single_segment {
        let n = if single_point { 1 } else { 2 };
        let points = (0..n)
            .map(|_| point(&mut bytes))
            .collect::<Option<Vec<_>>>()?;
        bytes.take(8 * extra * n)?;
        if single_point {
            Geometry::Point {
                coordinates: points[0].to_vec(),
            }
        } else {
            Geometry::LineString {
                coordinates: points,
            }
        }
    } else {
        let n = bytes.count(true, 16 + 8 * extra)?;
        let points = (0..n)
            .map(|_| point(&mut bytes))
            .collect::<Option<Vec<_>>>()?;
        bytes.take(8 * extra * n)?;
        let figures = sqlserver_figures(&mut bytes, version, points.len())?;
        let shapes = sqlserver_shapes(&mut bytes, figures.len())?;
        sqlserver_geometry(&points, &figures, &shapes)?
    };
    (bytes.left() == 0).then_some(GeoValue {
        geometry,
        srid: u32::try_from(srid).ok().filter(|&s| s != 0),
        format: GeoFormat::SqlServer,
        dimensions,
    })
}

/// The figures, each the offset of its first point; offsets never go back. Version 1 marks a
/// figure as an interior ring, a stroke or an exterior ring; version 2 as a point, a line, an
/// arc or a composite curve, and arcs and composite curves are not drawn.
fn sqlserver_figures(bytes: &mut Bytes, version: u8, points: usize) -> Option<Vec<Figure>> {
    let n = bytes.count(true, 5)?;
    let mut figures = Vec::with_capacity(n);
    let mut last = 0;
    for _ in 0..n {
        let attribute = bytes.u8()?;
        let drawn = match version {
            1 => attribute <= 2,
            _ => attribute <= 1,
        };
        let first_point = bytes.u32(true)? as usize;
        if !drawn || first_point < last || first_point > points {
            return None;
        }
        last = first_point;
        figures.push(Figure { first_point });
    }
    Some(figures)
}

/// The shapes: the first is the root, with no parent; every other's parent comes before it.
/// Figure offsets never go back.
fn sqlserver_shapes(bytes: &mut Bytes, figures: usize) -> Option<Vec<Shape>> {
    let n = bytes.count(true, 9)?;
    let mut shapes: Vec<Shape> = Vec::with_capacity(n);
    let mut last_figure = 0;
    for index in 0..n {
        let parent = bytes.u32(true)?;
        let figure = bytes.u32(true)?;
        let kind = bytes.u8()?;
        // -1 is "none" for both.
        let parent = (parent != u32::MAX).then_some(parent as usize);
        let first_figure = (figure != u32::MAX).then_some(figure as usize);
        match parent {
            None if index == 0 => {}
            Some(parent) if parent < index => {}
            _ => return None,
        }
        if let Some(figure) = first_figure {
            if figure < last_figure || figure >= figures {
                return None;
            }
            last_figure = figure;
        }
        shapes.push(Shape {
            parent,
            first_figure,
            kind,
        });
    }
    (!shapes.is_empty()).then_some(shapes)
}

/// A SQL Server value's points, figures and shapes, read into geometries.
struct ClrShapes<'a> {
    points: &'a [Position],
    figures: &'a [Figure],
    shapes: &'a [Shape],
    /// Each shape's children, in order.
    children: Vec<Vec<usize>>,
}

impl ClrShapes<'_> {
    /// A figure's points: up to the next figure's first point.
    fn figure_points(&self, figure: usize) -> &[Position] {
        let end = self
            .figures
            .get(figure + 1)
            .map_or(self.points.len(), |next| next.first_point);
        &self.points[self.figures[figure].first_point..end]
    }

    /// A shape's own figures: up to the next shape's first figure.
    fn shape_figures(&self, index: usize) -> std::ops::Range<usize> {
        let Some(first) = self.shapes[index].first_figure else {
            return 0..0;
        };
        let end = self.shapes[index + 1..]
            .iter()
            .find_map(|shape| shape.first_figure)
            .unwrap_or(self.figures.len());
        first..end
    }

    /// A shape as a geometry: points, lines and polygons from their figures, multi forms and
    /// collections from their children.
    fn geometry(&self, index: usize, depth: usize) -> Option<Geometry> {
        if depth > MAX_DEPTH {
            return None;
        }
        let kind = Kind::from_code(u32::from(self.shapes[index].kind))?;
        let own = self.shape_figures(index);
        Some(match kind {
            Kind::Point => match own.len() {
                0 => Geometry::Point {
                    coordinates: Vec::new(),
                },
                1 => match self.figure_points(own.start) {
                    [position] => Geometry::Point {
                        coordinates: position.to_vec(),
                    },
                    _ => return None,
                },
                _ => return None,
            },
            Kind::LineString => match own.len() {
                0 => Geometry::LineString {
                    coordinates: Vec::new(),
                },
                1 => Geometry::LineString {
                    coordinates: self.figure_points(own.start).to_vec(),
                },
                _ => return None,
            },
            Kind::Polygon => Geometry::Polygon {
                coordinates: own
                    .map(|figure| self.figure_points(figure).to_vec())
                    .collect(),
            },
            Kind::MultiPoint | Kind::MultiLineString | Kind::MultiPolygon | Kind::Collection => {
                let parts = self.children[index]
                    .iter()
                    .map(|&child| self.geometry(child, depth + 1))
                    .collect::<Option<Vec<_>>>()?;
                multi(kind, parts)?
            }
        })
    }
}

/// The root shape as a geometry. Only multi forms and collections have children.
fn sqlserver_geometry(
    points: &[Position],
    figures: &[Figure],
    shapes: &[Shape],
) -> Option<Geometry> {
    let mut children = vec![Vec::new(); shapes.len()];
    for (index, shape) in shapes.iter().enumerate() {
        if let Some(parent) = shape.parent {
            if !matches!(shapes[parent].kind, 4..=7) {
                return None;
            }
            children[parent].push(index);
        }
    }
    ClrShapes {
        points,
        figures,
        shapes,
        children,
    }
    .geometry(0, 0)
}

/// Well-known text (OGC, with ISO's `Z`, `M` and `ZM`), or PostGIS's extended form: `SRID=…;`
/// before it, `POINTM` for a point with M, and Z read from the number of coordinates. As
/// `ST_AsText` and `ST_AsEWKT` write it, in any case and spacing. The whole value must be
/// one geometry; nesting is bounded and every coordinate must be a finite number. Curves,
/// surfaces, TINs and triangles are not drawn.
fn read_wkt(text: &str) -> Option<GeoValue> {
    let (srid, body) = match text.get(..5) {
        Some(prefix) if prefix.eq_ignore_ascii_case("srid=") => {
            let (srid, body) = text[5..].split_once(';')?;
            (Some(srid.trim().parse::<i32>().ok()?), body)
        }
        _ => (None, text),
    };
    let mut wkt = Wkt {
        text: body.as_bytes(),
        at: 0,
        dimensions: None,
    };
    let geometry = wkt.geometry(0)?;
    wkt.space();
    (wkt.at == wkt.text.len()).then_some(GeoValue {
        geometry,
        srid: srid.and_then(|s| u32::try_from(s).ok()).filter(|&s| s != 0),
        format: GeoFormat::Wkt,
        dimensions: wkt.dimensions.unwrap_or(Dimensions::Xy),
    })
}

/// A reader over well-known text. The value's dimensions are decided once, by a `Z`, `M` or
/// `ZM` or by the first coordinate, and every coordinate after must have them.
struct Wkt<'a> {
    text: &'a [u8],
    at: usize,
    dimensions: Option<Dimensions>,
}

impl Wkt<'_> {
    fn space(&mut self) {
        while self.text.get(self.at).is_some_and(u8::is_ascii_whitespace) {
            self.at += 1;
        }
    }

    /// The next character after white space, without taking it.
    fn peek(&mut self) -> Option<u8> {
        self.space();
        self.text.get(self.at).copied()
    }

    /// Takes `c` after white space, or fails.
    fn expect(&mut self, c: u8) -> Option<()> {
        (self.peek()? == c).then(|| self.at += 1)
    }

    /// A word of letters, upper-cased (at most a keyword's length).
    fn word(&mut self) -> Option<String> {
        self.space();
        let start = self.at;
        while self.text.get(self.at).is_some_and(u8::is_ascii_alphabetic) {
            self.at += 1;
        }
        let word = &self.text[start..self.at];
        (!word.is_empty() && word.len() <= 20)
            .then(|| word.to_ascii_uppercase())
            .and_then(|word| String::from_utf8(word).ok())
    }

    /// Whether `EMPTY` follows; if it does, it is taken.
    fn empty(&mut self) -> bool {
        let at = self.at;
        if self.word().as_deref() == Some("EMPTY") {
            return true;
        }
        self.at = at;
        false
    }

    /// Sets the value's dimensions, or checks them against the ones already set.
    fn dimensions(&mut self, dimensions: Dimensions) -> Option<()> {
        match self.dimensions {
            None => {
                self.dimensions = Some(dimensions);
                Some(())
            }
            Some(set) => (set == dimensions).then_some(()),
        }
    }

    /// A finite number.
    fn number(&mut self) -> Option<f64> {
        self.space();
        let start = self.at;
        while self
            .text
            .get(self.at)
            .is_some_and(|c| c.is_ascii_digit() || matches!(c, b'+' | b'-' | b'.' | b'e' | b'E'))
        {
            self.at += 1;
        }
        let number: f64 = std::str::from_utf8(&self.text[start..self.at])
            .ok()?
            .parse()
            .ok()?;
        number.is_finite().then_some(number)
    }

    /// A coordinate: two to four numbers, as many as the value's dimensions say.
    fn position(&mut self) -> Option<Position> {
        let mut numbers = Vec::with_capacity(4);
        while numbers.len() < 4 && !matches!(self.peek()?, b',' | b')') {
            numbers.push(self.number()?);
        }
        let dimensions = match numbers.len() {
            2 => Dimensions::Xy,
            // Z unless an M was declared: PostGIS's `POINT(1 2 3)` is a Z point.
            3 => match self.dimensions {
                Some(Dimensions::Xym) => Dimensions::Xym,
                _ => Dimensions::Xyz,
            },
            4 => Dimensions::Xyzm,
            _ => return None,
        };
        self.dimensions(dimensions)?;
        Some([numbers[0], numbers[1]])
    }

    /// `(x y, x y, …)`, or `EMPTY`.
    fn positions(&mut self) -> Option<Vec<Position>> {
        if self.empty() {
            return Some(Vec::new());
        }
        self.list(Self::position)
    }

    /// `(item, item, …)`: at least one item.
    fn list<T>(&mut self, mut item: impl FnMut(&mut Self) -> Option<T>) -> Option<Vec<T>> {
        self.expect(b'(')?;
        let mut items = vec![item(self)?];
        while self.peek()? == b',' {
            self.at += 1;
            items.push(item(self)?);
        }
        self.expect(b')')?;
        Some(items)
    }

    /// `(ring, ring, …)`, or `EMPTY`.
    fn rings(&mut self) -> Option<Vec<Vec<Position>>> {
        if self.empty() {
            return Some(Vec::new());
        }
        self.list(Self::positions)
    }

    fn geometry(&mut self, depth: usize) -> Option<Geometry> {
        if depth > MAX_DEPTH {
            return None;
        }
        let word = self.word()?;
        // PostGIS writes a point with M as `POINTM`; ISO as `POINT M`.
        let (name, m_suffix) = match word.strip_suffix('M') {
            Some(name) if Self::kind(name).is_some() => (name, true),
            _ => (word.as_str(), false),
        };
        let kind = Self::kind(name)?;
        if m_suffix {
            self.dimensions(Dimensions::Xym)?;
        } else {
            let at = self.at;
            match self.word().as_deref() {
                Some("Z") => self.dimensions(Dimensions::Xyz)?,
                Some("M") => self.dimensions(Dimensions::Xym)?,
                Some("ZM") => self.dimensions(Dimensions::Xyzm)?,
                // `EMPTY`, read below.
                _ => self.at = at,
            }
        }
        Some(match kind {
            Kind::Point => {
                if self.empty() {
                    Geometry::Point {
                        coordinates: Vec::new(),
                    }
                } else {
                    let [position] = <[Position; 1]>::try_from(self.list(Self::position)?).ok()?;
                    Geometry::Point {
                        coordinates: position.to_vec(),
                    }
                }
            }
            Kind::LineString => Geometry::LineString {
                coordinates: self.positions()?,
            },
            Kind::Polygon => Geometry::Polygon {
                coordinates: self.rings()?,
            },
            Kind::MultiPoint => Geometry::MultiPoint {
                coordinates: if self.empty() {
                    Vec::new()
                } else {
                    // Each point bare (`1 2`) or in parentheses (`(1 2)`); an empty one is left
                    // out.
                    self.list(|wkt| {
                        if wkt.empty() {
                            Some(None)
                        } else if wkt.peek()? == b'(' {
                            let [position] =
                                <[Position; 1]>::try_from(wkt.list(Self::position)?).ok()?;
                            Some(Some(position))
                        } else {
                            wkt.position().map(Some)
                        }
                    })?
                    .into_iter()
                    .flatten()
                    .collect()
                },
            },
            Kind::MultiLineString => Geometry::MultiLineString {
                coordinates: self.rings()?,
            },
            Kind::MultiPolygon => Geometry::MultiPolygon {
                coordinates: if self.empty() {
                    Vec::new()
                } else {
                    self.list(Self::rings)?
                },
            },
            Kind::Collection => Geometry::GeometryCollection {
                geometries: if self.empty() {
                    Vec::new()
                } else {
                    self.list(|wkt| wkt.geometry(depth + 1))?
                },
            },
        })
    }

    fn kind(name: &str) -> Option<Kind> {
        Some(match name {
            "POINT" => Kind::Point,
            "LINESTRING" => Kind::LineString,
            "POLYGON" => Kind::Polygon,
            "MULTIPOINT" => Kind::MultiPoint,
            "MULTILINESTRING" => Kind::MultiLineString,
            "MULTIPOLYGON" => Kind::MultiPolygon,
            "GEOMETRYCOLLECTION" => Kind::Collection,
            _ => return None,
        })
    }
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
    /// SQL Server values in a column whose type the driver did not name, with an SRID: geometry
    /// or geography, not drawn ([`MapNote::SqlServerTypeUnknown`]).
    pub type_unknown: usize,
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
        type_unknown: 0,
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
        let geo = match read_cell_geometry(&value, &meta.native_type) {
            Ok(Some(geo)) => geo,
            Ok(None) => {
                map.rows_read += 1;
                map.unreadable += 1;
                continue;
            }
            Err(MapNote::SqlServerTypeUnknown) => {
                map.rows_read += 1;
                map.type_unknown += 1;
                continue;
            }
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

    /// Reads SQL Server bytes (written by SQL Server 2022 with `CAST(… AS varbinary(max))`) as
    /// a value of `native` type, and returns its GeoJSON and SRID.
    fn sqlserver(native: &str, text: &str) -> Option<(String, Option<u32>, Dimensions)> {
        let value = read_geometry(&Value::Bytes(hex(text)), native)?;
        assert_eq!(value.format, GeoFormat::SqlServer);
        Some((value.geometry.to_geojson(), value.srid, value.dimensions))
    }

    #[test]
    fn sql_server_geometry_and_geography() {
        let point = r#"{"type":"Point","coordinates":[20.0,10.0]}"#;
        // geometry::Point(20, 10, 4326): x then y.
        let geometry_point = "E6100000010C00000000000034400000000000002440";
        // geography::Point(10, 20, 4326): latitude 10, longitude 20, stored latitude first.
        let geography_point = "E6100000010C00000000000024400000000000003440";
        assert_eq!(
            sqlserver("geometry", geometry_point),
            Some((point.into(), Some(4326), Dimensions::Xy))
        );
        assert_eq!(
            sqlserver("geography", geography_point),
            Some((point.into(), Some(4326), Dimensions::Xy))
        );
        // A CLR type the driver did not name: with an SRID it may be either, and is not drawn
        // (nor guessed); at SRID 0 it can only be geometry.
        for bytes in [geometry_point, geography_point] {
            assert_eq!(
                read_cell_geometry(&Value::Bytes(hex(bytes)), "udt"),
                Err(MapNote::SqlServerTypeUnknown)
            );
            assert_eq!(read_geometry(&Value::Bytes(hex(bytes)), "udt"), None);
        }
        let line = r#"{"type":"LineString","coordinates":[[1.0,2.0],[3.0,4.0]]}"#;
        assert_eq!(
            sqlserver(
                "udt",
                "000000000114000000000000F03F000000000000004000000000000008400000000000001040"
            ),
            Some((line.into(), None, Dimensions::Xy))
        );
        // geography::STGeomFromText('LINESTRING(1 2, 3 4)', 4326): WKT is longitude first.
        assert_eq!(
            sqlserver(
                "geography",
                "E610000001140000000000000040000000000000F03F00000000000010400000000000000840"
            )
            .map(|v| v.0),
            Some(line.into())
        );
        // POLYGON((0 0, 4 0, 4 4, 0 0)).
        assert_eq!(
            sqlserver(
                "geometry",
                "000000000104040000000000000000000000000000000000000000000000000010400000000000000000000000000000104000000000000010400000000000000000000000000000000001000000020000000001000000FFFFFFFF0000000003"
            )
            .map(|v| v.0),
            Some(r#"{"type":"Polygon","coordinates":[[[0.0,0.0],[4.0,0.0],[4.0,4.0],[0.0,0.0]]]}"#.into())
        );
        // GEOMETRYCOLLECTION(POINT(1 2), LINESTRING(0 0, 1 1)).
        assert_eq!(
            sqlserver(
                "udt",
                "00000000010403000000000000000000F03F000000000000004000000000000000000000000000000000000000000000F03F000000000000F03F020000000100000000010100000003000000FFFFFFFF0000000007000000000000000001000000000100000002"
            )
            .map(|v| v.0),
            Some(r#"{"type":"GeometryCollection","geometries":[{"type":"Point","coordinates":[1.0,2.0]},{"type":"LineString","coordinates":[[0.0,0.0],[1.0,1.0]]}]}"#.into())
        );
        // POINT(1 2 3): Z is read and left out.
        assert_eq!(
            sqlserver(
                "geometry",
                "00000000010D000000000000F03F00000000000000400000000000000840"
            ),
            Some((
                r#"{"type":"Point","coordinates":[1.0,2.0]}"#.into(),
                None,
                Dimensions::Xyz
            ))
        );
        // POINT EMPTY: a geometry in a spatial column; outside one, more likely something else.
        let empty = "000000000104000000000000000001000000FFFFFFFFFFFFFFFF01";
        assert_eq!(
            sqlserver("geometry", empty).map(|v| v.0),
            Some(r#"{"type":"Point","coordinates":[]}"#.into())
        );
        assert_eq!(read_geometry(&Value::Bytes(hex(empty)), "udt"), None);
        // CIRCULARSTRING(0 0, 1 1, 2 0): curves are not drawn.
        let arc = "0000000002040300000000000000000000000000000000000000000000000000F03F000000000000F03F0000000000000040000000000000000001000000020000000001000000FFFFFFFF0000000008";
        assert_eq!(read_geometry(&Value::Bytes(hex(arc)), "geometry"), None);
        // Only a column of a CLR type is read this way.
        assert_eq!(
            read_geometry(&Value::Bytes(hex(geometry_point)), "varbinary"),
            None
        );
    }

    /// Reads `text` as a text cell of `native` type: its GeoJSON, SRID and dimensions.
    fn wkt(native: &str, text: &str) -> Option<(String, Option<u32>, Dimensions)> {
        let value = read_geometry(&Value::Text(text.into()), native)?;
        assert_eq!(value.format, GeoFormat::Wkt, "{text}");
        Some((value.geometry.to_geojson(), value.srid, value.dimensions))
    }

    fn wkt_geojson(text: &str) -> Option<String> {
        wkt("text", text).map(|v| v.0)
    }

    #[test]
    fn well_known_text_of_each_type() {
        let cases = [
            ("POINT(1 2)", r#"{"type":"Point","coordinates":[1.0,2.0]}"#),
            (
                "point ( -1.5e1  2 )",
                r#"{"type":"Point","coordinates":[-15.0,2.0]}"#,
            ),
            (
                "LINESTRING(0 0, 1 1, 2 0)",
                r#"{"type":"LineString","coordinates":[[0.0,0.0],[1.0,1.0],[2.0,0.0]]}"#,
            ),
            (
                "POLYGON((0 0,4 0,4 4,0 0),(1 1,1 2,2 2,1 1))",
                r#"{"type":"Polygon","coordinates":[[[0.0,0.0],[4.0,0.0],[4.0,4.0],[0.0,0.0]],[[1.0,1.0],[1.0,2.0],[2.0,2.0],[1.0,1.0]]]}"#,
            ),
            // Both ways MySQL and PostGIS write a multipoint.
            (
                "MULTIPOINT((1 2),(3 4))",
                r#"{"type":"MultiPoint","coordinates":[[1.0,2.0],[3.0,4.0]]}"#,
            ),
            (
                "MULTIPOINT(1 2, 3 4)",
                r#"{"type":"MultiPoint","coordinates":[[1.0,2.0],[3.0,4.0]]}"#,
            ),
            (
                "MULTILINESTRING((0 0,1 1),(2 2,3 3))",
                r#"{"type":"MultiLineString","coordinates":[[[0.0,0.0],[1.0,1.0]],[[2.0,2.0],[3.0,3.0]]]}"#,
            ),
            (
                "MULTIPOLYGON(((0 0,1 0,1 1,0 0)),((2 2,3 2,3 3,2 2)))",
                r#"{"type":"MultiPolygon","coordinates":[[[[0.0,0.0],[1.0,0.0],[1.0,1.0],[0.0,0.0]]],[[[2.0,2.0],[3.0,2.0],[3.0,3.0],[2.0,2.0]]]]}"#,
            ),
            (
                "GEOMETRYCOLLECTION(POINT(1 2),LINESTRING(0 0,1 1))",
                r#"{"type":"GeometryCollection","geometries":[{"type":"Point","coordinates":[1.0,2.0]},{"type":"LineString","coordinates":[[0.0,0.0],[1.0,1.0]]}]}"#,
            ),
        ];
        for (text, geojson) in cases {
            assert_eq!(
                wkt("text", text),
                Some((geojson.into(), None, Dimensions::Xy)),
                "{text}"
            );
        }
    }

    #[test]
    fn well_known_text_srid_dimensions_and_empty() {
        let point = r#"{"type":"Point","coordinates":[1.0,2.0]}"#.to_string();
        // PostGIS's extended form.
        assert_eq!(
            wkt("text", "SRID=4326;POINT(1 2)"),
            Some((point.clone(), Some(4326), Dimensions::Xy))
        );
        assert_eq!(wkt("text", "srid=0;POINT(1 2)").unwrap().1, None);
        // Z and M, the ISO way and PostGIS's.
        for (text, dimensions) in [
            ("POINT Z (1 2 3)", Dimensions::Xyz),
            ("POINT(1 2 3)", Dimensions::Xyz),
            ("POINT M (1 2 3)", Dimensions::Xym),
            ("POINTM(1 2 3)", Dimensions::Xym),
            ("POINT ZM (1 2 3 4)", Dimensions::Xyzm),
            ("POINT(1 2 3 4)", Dimensions::Xyzm),
        ] {
            assert_eq!(
                wkt("text", text),
                Some((point.clone(), None, dimensions)),
                "{text}"
            );
        }
        assert_eq!(
            wkt("text", "MULTIPOINT Z ((1 2 3), (3 4 5))").unwrap().2,
            Dimensions::Xyz
        );
        // Empty geometries: in a spatial column only.
        for (text, geojson) in [
            ("POINT EMPTY", r#"{"type":"Point","coordinates":[]}"#),
            (
                "LINESTRING EMPTY",
                r#"{"type":"LineString","coordinates":[]}"#,
            ),
            ("POLYGON Z EMPTY", r#"{"type":"Polygon","coordinates":[]}"#),
            (
                "MULTIPOLYGON EMPTY",
                r#"{"type":"MultiPolygon","coordinates":[]}"#,
            ),
            (
                "GEOMETRYCOLLECTION EMPTY",
                r#"{"type":"GeometryCollection","geometries":[]}"#,
            ),
        ] {
            assert_eq!(
                wkt("geometry", text).map(|v| v.0),
                Some(geojson.into()),
                "{text}"
            );
            assert_eq!(
                read_geometry(&Value::Text(text.into()), "text"),
                None,
                "{text}"
            );
        }
        assert_eq!(
            wkt_geojson("GEOMETRYCOLLECTION(POINT EMPTY, POINT(1 2))"),
            Some(r#"{"type":"GeometryCollection","geometries":[{"type":"Point","coordinates":[]},{"type":"Point","coordinates":[1.0,2.0]}]}"#.into())
        );
        assert_eq!(
            wkt_geojson("MULTIPOINT(EMPTY, (1 2))"),
            Some(r#"{"type":"MultiPoint","coordinates":[[1.0,2.0]]}"#.into())
        );
    }

    #[test]
    fn text_that_is_not_one_whole_geometry() {
        for text in [
            "",
            "POINT",
            "POINT()",
            "POINT(1)",
            "POINT(1 2 3 4 5)",
            "POINT(1 2",
            "POINT(1 2))",
            "POINT(1 2) and more",
            "the POINT(1 2)",
            "POINT(1 2),POINT(3 4)",
            "POINT(1, 2)",
            "POINT(1 2, 3 4)",
            "POINT(NaN 2)",
            "POINT(inf 2)",
            "POINT(1e999 2)",
            "POINT(1-2 3)",
            "LINESTRING(0 0, 1 1 1)",
            "POINT Z (1 2)",
            "POINT ZM (1 2 3)",
            "GEOMETRYCOLLECTION Z (POINT(1 2))",
            "CIRCULARSTRING(0 0, 1 1, 2 0)",
            "CURVEPOLYGON((0 0, 1 1, 0 0))",
            "TRIANGLE((0 0, 1 0, 0 1, 0 0))",
            "SRID=4326POINT(1 2)",
            "SRID=x;POINT(1 2)",
            "Paris",
        ] {
            assert_eq!(
                read_geometry(&Value::Text(text.into()), "geometry"),
                None,
                "{text:?}"
            );
        }
        // Collections nested deeper than the bound, and a long one within it.
        let deep = MAX_DEPTH + 2;
        let nested = format!(
            "{}POINT(1 2){}",
            "GEOMETRYCOLLECTION(".repeat(deep),
            ")".repeat(deep)
        );
        assert_eq!(wkt_geojson(&nested), None);
        let shallow = format!(
            "{}POINT(1 2){}",
            "GEOMETRYCOLLECTION(".repeat(3),
            ")".repeat(3)
        );
        assert!(wkt_geojson(&shallow).is_some());
        let many = format!("LINESTRING({})", vec!["1 2"; 10_000].join(","));
        assert_eq!(
            read_geometry(&Value::Text(many), "text").map(|v| v.geometry.points()),
            Some(10_000)
        );
    }

    #[test]
    fn hostile_sql_server_bytes_are_not_geometries() {
        let polygon = hex(
            "000000000104040000000000000000000000000000000000000000000000000010400000000000000000000000000000104000000000000010400000000000000000000000000000000001000000020000000001000000FFFFFFFF0000000003",
        );
        let read = |bytes: &[u8]| read_sqlserver(bytes, false);
        assert!(read(&polygon).is_some());
        // Cut anywhere, or with a byte too many.
        for end in 0..polygon.len() {
            assert!(read(&polygon[..end]).is_none(), "cut at {end}");
        }
        let mut longer = polygon.clone();
        longer.push(0);
        assert!(read(&longer).is_none());
        let with = |at: usize, value: &[u8]| {
            let mut bytes = polygon.clone();
            bytes[at..at + value.len()].copy_from_slice(value);
            bytes
        };
        // An unknown version, unknown flags, a single point and segment at once.
        assert!(read(&with(4, &[3])).is_none());
        assert!(read(&with(5, &[0x44])).is_none());
        assert!(read(&with(5, &[0x18])).is_none());
        // A point count the bytes cannot hold.
        assert!(read(&with(6, &[0xFF, 0xFF, 0xFF, 0x7F])).is_none());
        // A figure past the points, an arc in version 1's places, a shape past the figures, a
        // parent after its child, a curve type.
        let figure = 6 + 4 + 4 * 16 + 4;
        assert!(read(&with(figure + 1, &[5, 0, 0, 0])).is_none());
        assert!(read(&with(figure, &[3])).is_none());
        let shape = figure + 5 + 4;
        assert!(read(&with(shape + 4, &[1, 0, 0, 0])).is_none());
        assert!(read(&with(shape, &[0, 0, 0, 0])).is_none());
        assert!(read(&with(shape + 8, &[8])).is_none());
        // A coordinate that is not a finite number.
        assert!(read(&with(10, &f64::NAN.to_le_bytes())).is_none());
        // Shapes nested deeper than the bound.
        // The header, no points, no figures, then the shape count and the shapes.
        let deep = MAX_DEPTH + 2;
        let mut nested = vec![0, 0, 0, 0, 1, 0x04, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        nested.splice(14..18, (deep as u32).to_le_bytes());
        for index in 0..deep {
            let parent = if index == 0 {
                u32::MAX
            } else {
                index as u32 - 1
            };
            nested.extend(parent.to_le_bytes());
            nested.extend(u32::MAX.to_le_bytes());
            nested.push(7);
        }
        assert!(read(&nested).is_none());
        nested.truncate(18 + 9 * 3);
        nested.splice(14..18, 3u32.to_le_bytes());
        assert_eq!(
            read(&nested).map(|v| v.geometry.to_geojson()),
            Some(r#"{"type":"GeometryCollection","geometries":[{"type":"GeometryCollection","geometries":[{"type":"GeometryCollection","geometries":[]}]}]}"#.into())
        );
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
