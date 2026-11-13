//! Geometry values on a map (ADR-045), from every engine with spatial types to one GeoJSON:
//! SQLite (SpatiaLite's and GeoPackage's blobs) and DuckDB always; DuckDB's spatial extension
//! when it can be installed (it downloads once; `SPELAION_TEST_DUCKDB_SPATIAL=1` makes a
//! failed install fail the test); PostGIS, MySQL and MariaDB with their `SPELAION_TEST_*`
//! variables (without them they are skipped, which does not count as tested). PostGIS is in the
//! test PostgreSQL's `spelaion_gis` database (`testing/docker/postgres-init/02-postgis.sql`).
//!
//! Each value goes the way the app takes it: the engine runs a query, the core stores the
//! result, and the cell viewer (`view_cell`) and the column's map (`column_map`) read it.
//!
//! The SQLite blobs were written by a real SpatiaLite 5.0.1 (Debian bookworm's
//! `spatialite-bin`): `GeomFromText`, `CompressGeometry` and `AsGPB` over the WKT next to each
//! one. To write them again:
//! `docker run --rm debian:bookworm-slim sh -c "apt-get update && apt-get install -y spatialite-bin && echo \"SELECT hex(GeomFromText('POINT(1 2)', 4326));\" | spatialite /tmp/x.db"`.

use std::sync::Arc;

use futures_util::StreamExt;
use spelaion_core::{
    CoreEvent, Dimensions, GeoFormat, MapCrs, ResultId, ResultStore, Session, column_map, view_cell,
};
use spelaion_driver_api::{ConnectParams, Driver, Secret};

async fn session(driver: &dyn Driver, params: ConnectParams) -> Session {
    let conn = driver
        .connect(&params)
        .await
        .unwrap_or_else(|e| panic!("connecting: {e}"));
    Session::new(
        conn,
        spelaion_dialect::for_engine(driver.engine()).expect("a dialect"),
        Arc::new(ResultStore::new()),
    )
}

fn server(var: &str) -> Option<(String, u16)> {
    let Ok(addr) = std::env::var(var) else {
        eprintln!("{var} not set: skipped");
        return None;
    };
    let (host, port) = addr.rsplit_once(':').expect("host:port");
    Some((host.to_string(), port.parse().expect("port")))
}

fn login(host: String, port: u16, database: &str) -> ConnectParams {
    ConnectParams {
        host,
        port,
        user: "spelaion".into(),
        password: Some(Secret::new("spelaion")),
        database: database.into(),
        ..Default::default()
    }
}

fn memory() -> ConnectParams {
    ConnectParams {
        database: ":memory:".into(),
        ..Default::default()
    }
}

/// Runs `sql`; panics if it fails or gives no result.
async fn query(label: &str, session: &Session, sql: &str) -> ResultId {
    let events: Vec<CoreEvent> = session.run_script(sql).collect().await;
    let mut result = None;
    for event in &events {
        match event {
            CoreEvent::StatementFailed { error, .. } => {
                panic!("{label}: {sql}\n→ {}", error.message)
            }
            CoreEvent::ResultStarted { result: id, .. } => result = Some(*id),
            _ => {}
        }
    }
    result.unwrap_or_else(|| panic!("{label}: no result for {sql}"))
}

async fn statement(label: &str, session: &Session, sql: &str) {
    let events: Vec<CoreEvent> = session.run_script(sql).collect().await;
    for event in &events {
        if let CoreEvent::StatementFailed { error, .. } = event {
            panic!("{label}: {sql}\n→ {}", error.message)
        }
    }
}

const POINT: &str = r#"{"type":"Point","coordinates":[1.0,2.0]}"#;
const LINE: &str = r#"{"type":"LineString","coordinates":[[0.0,0.0],[1.0,1.0],[2.0,0.0]]}"#;
const POLYGON: &str = r#"{"type":"Polygon","coordinates":[[[0.0,0.0],[4.0,0.0],[4.0,4.0],[0.0,4.0],[0.0,0.0]],[[1.0,1.0],[1.0,2.0],[2.0,2.0],[1.0,1.0]]]}"#;
const MULTIPOLYGON: &str = r#"{"type":"MultiPolygon","coordinates":[[[[0.0,0.0],[1.0,0.0],[1.0,1.0],[0.0,0.0]]],[[[2.0,2.0],[3.0,2.0],[3.0,3.0],[2.0,2.0]]]]}"#;
const COLLECTION: &str = r#"{"type":"GeometryCollection","geometries":[{"type":"Point","coordinates":[1.0,2.0]},{"type":"LineString","coordinates":[[0.0,0.0],[1.0,1.0]]}]}"#;

/// The well-known text each engine builds its values from, and the GeoJSON every one of them
/// must give.
const SHAPES: [(&str, &str); 5] = [
    ("POINT(1 2)", POINT),
    ("LINESTRING(0 0, 1 1, 2 0)", LINE),
    (
        "POLYGON((0 0, 4 0, 4 4, 0 4, 0 0), (1 1, 1 2, 2 2, 1 1))",
        POLYGON,
    ),
    (
        "MULTIPOLYGON(((0 0, 1 0, 1 1, 0 0)), ((2 2, 3 2, 3 3, 2 2)))",
        MULTIPOLYGON,
    ),
    (
        "GEOMETRYCOLLECTION(POINT(1 2), LINESTRING(0 0, 1 1))",
        COLLECTION,
    ),
];

/// The same shapes as OGC WKB (little endian), as PostGIS's `ST_AsBinary` writes them.
const WKB: [&str; 5] = [
    "0101000000000000000000f03f0000000000000040",
    "01020000000300000000000000000000000000000000000000000000000000f03f000000000000f03f00000000000000400000000000000000",
    "01030000000200000005000000000000000000000000000000000000000000000000001040000000000000000000000000000010400000000000001040000000000000000000000000000010400000000000000000000000000000000004000000000000000000f03f000000000000f03f000000000000f03f000000000000004000000000000000400000000000000040000000000000f03f000000000000f03f",
    "0106000000020000000103000000010000000400000000000000000000000000000000000000000000000000f03f0000000000000000000000000000f03f000000000000f03f000000000000000000000000000000000103000000010000000400000000000000000000400000000000000040000000000000084000000000000000400000000000000840000000000000084000000000000000400000000000000040",
    "0107000000020000000101000000000000000000f03f000000000000004001020000000200000000000000000000000000000000000000000000000000f03f000000000000f03f",
];

/// One expected cell: the GeoJSON, the SRID, the stored format and the dimensions read.
struct Expected {
    geojson: &'static str,
    srid: Option<u32>,
    format: GeoFormat,
    dimensions: Dimensions,
}

fn expect(geojson: &'static str, srid: Option<u32>, format: GeoFormat) -> Expected {
    Expected {
        geojson,
        srid,
        format,
        dimensions: Dimensions::Xy,
    }
}

/// `SELECT g FROM (… UNION ALL …) ORDER BY n`, one row per expression, then a NULL.
fn rows_sql(expressions: &[String]) -> String {
    let mut parts: Vec<String> = expressions
        .iter()
        .enumerate()
        .map(|(i, e)| format!("SELECT {} AS n, {e} AS g", i + 1))
        .collect();
    parts.push(format!("SELECT {} AS n, NULL AS g", expressions.len() + 1));
    format!(
        "SELECT g FROM ({}) AS t ORDER BY n",
        parts.join(" UNION ALL ")
    )
}

/// Reads every row of the result's only column as the cell viewer does, checks it against
/// `expected` (the last row is NULL), then checks the column's map. `native` is the column's
/// type name as the engine reports it.
fn check(
    label: &str,
    session: &Session,
    result: ResultId,
    native: &str,
    expected: &[Expected],
    crs: MapCrs,
) {
    let stored = session.results().get(result).expect("stored result");
    let stored = stored.read().unwrap();
    assert_eq!(
        stored.schema().columns[0].native_type.to_ascii_lowercase(),
        native.to_ascii_lowercase(),
        "{label}: column type"
    );
    assert_eq!(stored.rows(), expected.len() + 1, "{label}: rows");
    for (row, want) in expected.iter().enumerate() {
        let cell = view_cell(&stored, row, 0, false).unwrap();
        let geo = cell
            .geometry
            .unwrap_or_else(|| panic!("{label}: row {row} is not read as a geometry"));
        assert_eq!(
            geo.geometry.to_geojson(),
            want.geojson,
            "{label}: row {row}"
        );
        assert_eq!(geo.srid, want.srid, "{label}: row {row} SRID");
        assert_eq!(geo.format, want.format, "{label}: row {row} format");
        assert_eq!(
            geo.dimensions, want.dimensions,
            "{label}: row {row} dimensions"
        );
    }
    let null = view_cell(&stored, expected.len(), 0, false).unwrap();
    assert_eq!(null.geometry, None, "{label}: NULL");

    let map = column_map(&stored, 0).unwrap();
    assert_eq!(map.rows_read, expected.len() + 1, "{label}: map rows");
    assert_eq!(map.nulls, 1, "{label}: map NULLs");
    assert_eq!(map.unreadable, 0, "{label}: map unreadable");
    assert!(!map.truncated, "{label}: map truncated");
    let shapes: Vec<(usize, String)> = map
        .shapes
        .iter()
        .map(|s| (s.row, s.geometry.to_geojson()))
        .collect();
    let want: Vec<(usize, String)> = expected
        .iter()
        .enumerate()
        .map(|(row, e)| (row, e.geojson.to_string()))
        .collect();
    assert_eq!(shapes, want, "{label}: map shapes");
    assert_eq!(map.crs, crs, "{label}: map CRS");
}

#[tokio::test]
async fn spatialite_and_geopackage_blobs_in_sqlite() {
    let label = "sqlite";
    let session = session(&spelaion_driver_sqlite::SqliteDriver::default(), memory()).await;
    // As SpatiaLite's `AddGeometryColumn` declares them.
    statement(
        label,
        &session,
        "CREATE TABLE places (n INTEGER PRIMARY KEY, g GEOMETRY)",
    )
    .await;
    let blobs: [(&str, Expected); 13] = [
        (
            // POINT(1 2), 4326
            "0001E6100000000000000000F03F0000000000000040000000000000F03F00000000000000407C01000000000000000000F03F0000000000000040FE",
            expect(POINT, Some(4326), GeoFormat::Spatialite),
        ),
        (
            // LINESTRING(0 0, 1 1, 2 0), 4326
            "0001E6100000000000000000000000000000000000000000000000000040000000000000F03F7C020000000300000000000000000000000000000000000000000000000000F03F000000000000F03F00000000000000400000000000000000FE",
            expect(LINE, Some(4326), GeoFormat::Spatialite),
        ),
        (
            // POLYGON((0 0, 4 0, 4 4, 0 4, 0 0), (1 1, 1 2, 2 2, 1 1)), 4326
            "0001E610000000000000000000000000000000000000000000000000104000000000000010407C030000000200000005000000000000000000000000000000000000000000000000001040000000000000000000000000000010400000000000001040000000000000000000000000000010400000000000000000000000000000000004000000000000000000F03F000000000000F03F000000000000F03F000000000000004000000000000000400000000000000040000000000000F03F000000000000F03FFE",
            expect(POLYGON, Some(4326), GeoFormat::Spatialite),
        ),
        (
            // MULTIPOLYGON(((0 0, 1 0, 1 1, 0 0)), ((2 2, 3 2, 3 3, 2 2))), 3857
            "0001110F000000000000000000000000000000000000000000000000084000000000000008407C06000000020000006903000000010000000400000000000000000000000000000000000000000000000000F03F0000000000000000000000000000F03F000000000000F03F000000000000000000000000000000006903000000010000000400000000000000000000400000000000000040000000000000084000000000000000400000000000000840000000000000084000000000000000400000000000000040FE",
            expect(MULTIPOLYGON, Some(3857), GeoFormat::Spatialite),
        ),
        (
            // GEOMETRYCOLLECTION(POINT(1 2), LINESTRING(0 0, 1 1)), SRID 0
            "00010000000000000000000000000000000000000000000000000000F03F00000000000000407C07000000020000006901000000000000000000F03F000000000000004069020000000200000000000000000000000000000000000000000000000000F03F000000000000F03FFE",
            expect(COLLECTION, None, GeoFormat::Spatialite),
        ),
        (
            // POINTZ(1 2 3), 4326
            "0001E6100000000000000000F03F0000000000000040000000000000F03F00000000000000407CE9030000000000000000F03F00000000000000400000000000000840FE",
            Expected {
                dimensions: Dimensions::Xyz,
                ..expect(POINT, Some(4326), GeoFormat::Spatialite)
            },
        ),
        (
            // LINESTRINGZM(0 0 1 5, 1 1 2 6, 2 0 3 7), 4326
            "0001E6100000000000000000000000000000000000000000000000000040000000000000F03F7CBA0B00000300000000000000000000000000000000000000000000000000F03F0000000000001440000000000000F03F000000000000F03F000000000000004000000000000018400000000000000040000000000000000000000000000008400000000000001C40FE",
            Expected {
                dimensions: Dimensions::Xyzm,
                ..expect(LINE, Some(4326), GeoFormat::Spatialite)
            },
        ),
        (
            // CompressGeometry(LINESTRING(0 0, 1 1, 2 0, 3 3)), 4326
            "0001E610000000000000000000000000000000000000000000000000084000000000000008407C42420F0004000000000000000000000000000000000000000000803F0000803F0000803F000080BF00000000000008400000000000000840FE",
            expect(
                r#"{"type":"LineString","coordinates":[[0.0,0.0],[1.0,1.0],[2.0,0.0],[3.0,3.0]]}"#,
                Some(4326),
                GeoFormat::Spatialite,
            ),
        ),
        (
            // CompressGeometry(POLYGONZ((0 0 1, 4 0 2, 4 4 3, 0 4 4, 0 0 1))), 4326
            "0001E610000000000000000000000000000000000000000000000000104000000000000010407C2B460F00010000000500000000000000000000000000000000000000000000000000F03F00008040000000000000803F00000000000080400000803F000080C0000000000000803F00000000000000000000000000000000000000000000F03FFE",
            Expected {
                dimensions: Dimensions::Xyz,
                ..expect(
                    r#"{"type":"Polygon","coordinates":[[[0.0,0.0],[4.0,0.0],[4.0,4.0],[0.0,4.0],[0.0,0.0]]]}"#,
                    Some(4326),
                    GeoFormat::Spatialite,
                )
            },
        ),
        (
            // CompressGeometry(LINESTRINGM(0 0 5, 1 1 6, 2 0 7)), 4326
            "0001E6100000000000000000000000000000000000000000000000000040000000000000F03F7C124A0F00030000000000000000000000000000000000000000000000000014400000803F0000803F0000000000001840000000000000004000000000000000000000000000001C40FE",
            Expected {
                dimensions: Dimensions::Xym,
                ..expect(LINE, Some(4326), GeoFormat::Spatialite)
            },
        ),
        (
            // AsGPB(POINT(1 2)), 4326
            "47500003E6100000000000000000F03F000000000000F03F000000000000004000000000000000400101000000000000000000F03F0000000000000040",
            expect(POINT, Some(4326), GeoFormat::GeoPackage),
        ),
        (
            // AsGPB(LINESTRING(0 0, 1 1, 2 0)), 4326
            "47500003E6100000000000000000000000000000000000400000000000000000000000000000F03F01020000000300000000000000000000000000000000000000000000000000F03F000000000000F03F00000000000000400000000000000000",
            expect(LINE, Some(4326), GeoFormat::GeoPackage),
        ),
        (
            // AsGPB(POLYGON((0 0, 4 0, 4 4, 0 4, 0 0))), 4326
            "47500003E61000000000000000000000000000000000104000000000000000000000000000001040010300000001000000050000000000000000000000000000000000000000000000000010400000000000000000000000000000104000000000000010400000000000000000000000000000104000000000000000000000000000000000",
            expect(
                r#"{"type":"Polygon","coordinates":[[[0.0,0.0],[4.0,0.0],[4.0,4.0],[0.0,4.0],[0.0,0.0]]]}"#,
                Some(4326),
                GeoFormat::GeoPackage,
            ),
        ),
    ];
    for (i, (hex, _)) in blobs.iter().enumerate() {
        statement(
            label,
            &session,
            &format!("INSERT INTO places VALUES ({}, X'{hex}')", i + 1),
        )
        .await;
    }
    statement(label, &session, "INSERT INTO places VALUES (100, NULL)").await;
    let result = query(label, &session, "SELECT g FROM places ORDER BY n").await;
    let expected: Vec<Expected> = blobs.into_iter().map(|(_, e)| e).collect();
    // SRIDs differ (4326, 3857, none): drawn as they are.
    check(
        label,
        &session,
        result,
        "GEOMETRY",
        &expected,
        MapCrs::Plane,
    );

    // Only the 4326 ones: longitude and latitude.
    let result = query(
        label,
        &session,
        "SELECT g FROM places WHERE n IN (1, 2, 3, 100) ORDER BY n",
    )
    .await;
    check(
        label,
        &session,
        result,
        "GEOMETRY",
        &[
            expect(POINT, Some(4326), GeoFormat::Spatialite),
            expect(LINE, Some(4326), GeoFormat::Spatialite),
            expect(POLYGON, Some(4326), GeoFormat::Spatialite),
        ],
        MapCrs::LonLat,
    );
}

#[tokio::test]
async fn wkb_blobs_in_duckdb() {
    let label = "duckdb";
    let session = session(&spelaion_driver_duckdb::DuckdbDriver::default(), memory()).await;
    let expressions: Vec<String> = WKB.iter().map(|h| format!("from_hex('{h}')")).collect();
    let result = query(label, &session, &rows_sql(&expressions)).await;
    let expected: Vec<Expected> = SHAPES
        .iter()
        .map(|(_, geojson)| expect(geojson, None, GeoFormat::Wkb))
        .collect();
    check(
        label,
        &session,
        result,
        "BLOB",
        &expected,
        MapCrs::AssumedLonLat,
    );
}

#[tokio::test]
async fn duckdb_spatial() {
    let label = "duckdb spatial";
    let required = std::env::var("SPELAION_TEST_DUCKDB_SPATIAL").is_ok();
    let session = session(&spelaion_driver_duckdb::DuckdbDriver::default(), memory()).await;
    let events: Vec<CoreEvent> = session
        .run_script("INSTALL spatial; LOAD spatial")
        .collect()
        .await;
    if let Some(error) = events.iter().find_map(|e| match e {
        CoreEvent::StatementFailed { error, .. } => Some(error.message.clone()),
        _ => None,
    }) {
        assert!(!required, "{label}: installing the extension: {error}");
        eprintln!("{label}: the extension could not be installed ({error}): skipped");
        return;
    }
    let expressions: Vec<String> = SHAPES
        .iter()
        .map(|(wkt, _)| format!("ST_GeomFromText('{wkt}')"))
        .collect();
    let result = query(label, &session, &rows_sql(&expressions)).await;
    // DuckDB's geometries have no SRID.
    let expected: Vec<Expected> = SHAPES
        .iter()
        .map(|(_, geojson)| expect(geojson, None, GeoFormat::Wkb))
        .collect();
    check(
        label,
        &session,
        result,
        "GEOMETRY",
        &expected,
        MapCrs::AssumedLonLat,
    );

    // Z is read and left out; coordinates past the degrees are drawn as they are.
    let result = query(
        label,
        &session,
        &rows_sql(&[
            "ST_GeomFromText('POINT Z (1 2 3)')".into(),
            "ST_GeomFromText('POINT (500000 4400000)')".into(),
        ]),
    )
    .await;
    check(
        label,
        &session,
        result,
        "GEOMETRY",
        &[
            Expected {
                dimensions: Dimensions::Xyz,
                ..expect(POINT, None, GeoFormat::Wkb)
            },
            expect(
                r#"{"type":"Point","coordinates":[500000.0,4400000.0]}"#,
                None,
                GeoFormat::Wkb,
            ),
        ],
        MapCrs::Plane,
    );
}

#[tokio::test]
async fn postgis() {
    let Some((host, port)) = server("SPELAION_TEST_PG") else {
        return;
    };
    let label = "postgis";
    let session = session(
        &spelaion_driver_postgres::PostgresDriver,
        login(host, port, "spelaion_gis"),
    )
    .await;
    let expressions: Vec<String> = SHAPES
        .iter()
        .map(|(wkt, _)| format!("ST_GeomFromText('{wkt}', 4326)"))
        .collect();
    let result = query(label, &session, &rows_sql(&expressions)).await;
    let expected: Vec<Expected> = SHAPES
        .iter()
        .map(|(_, geojson)| expect(geojson, Some(4326), GeoFormat::Ewkb))
        .collect();
    check(
        label,
        &session,
        result,
        "geometry",
        &expected,
        MapCrs::LonLat,
    );

    // Geography (always 4326).
    let expressions: Vec<String> = SHAPES[..3]
        .iter()
        .map(|(wkt, _)| format!("ST_GeogFromText('{wkt}')"))
        .collect();
    let result = query(label, &session, &rows_sql(&expressions)).await;
    let expected: Vec<Expected> = SHAPES[..3]
        .iter()
        .map(|(_, geojson)| expect(geojson, Some(4326), GeoFormat::Ewkb))
        .collect();
    check(
        label,
        &session,
        result,
        "geography",
        &expected,
        MapCrs::LonLat,
    );

    // Web Mercator, and a point with Z and M.
    let result = query(
        label,
        &session,
        &rows_sql(&[
            "ST_GeomFromText('LINESTRING(0 0, 1 1, 2 0)', 3857)".into(),
            "ST_GeomFromText('POINT ZM (1 2 3 4)', 3857)".into(),
        ]),
    )
    .await;
    check(
        label,
        &session,
        result,
        "geometry",
        &[
            expect(LINE, Some(3857), GeoFormat::Ewkb),
            Expected {
                dimensions: Dimensions::Xyzm,
                ..expect(POINT, Some(3857), GeoFormat::Ewkb)
            },
        ],
        MapCrs::WebMercator,
    );
}

#[tokio::test]
async fn mysql_and_mariadb() {
    for (label, var, driver) in [
        (
            "mysql",
            "SPELAION_TEST_MYSQL",
            spelaion_driver_mysql::MysqlDriver::mysql(),
        ),
        (
            "mariadb",
            "SPELAION_TEST_MARIADB",
            spelaion_driver_mysql::MysqlDriver::mariadb(),
        ),
    ] {
        let Some((host, port)) = server(var) else {
            continue;
        };
        let session = session(&driver, login(host, port, "spelaion")).await;
        statement(label, &session, "DROP TABLE IF EXISTS geo_places").await;
        statement(
            label,
            &session,
            "CREATE TABLE geo_places (n INT PRIMARY KEY, g GEOMETRY)",
        )
        .await;
        for (i, (wkt, _)) in SHAPES.iter().enumerate() {
            statement(
                label,
                &session,
                &format!(
                    "INSERT INTO geo_places VALUES ({}, ST_GeomFromText('{wkt}'))",
                    i + 1
                ),
            )
            .await;
        }
        statement(label, &session, "INSERT INTO geo_places VALUES (100, NULL)").await;
        let result = query(label, &session, "SELECT g FROM geo_places ORDER BY n").await;
        let expected: Vec<Expected> = SHAPES
            .iter()
            .map(|(_, geojson)| expect(geojson, None, GeoFormat::Mysql))
            .collect();
        check(
            label,
            &session,
            result,
            "geometry",
            &expected,
            MapCrs::AssumedLonLat,
        );

        // SRID 4326: MySQL reads its well-known text as latitude first and stores longitude as
        // X; MariaDB stores X as written. Either way X is the longitude the map draws.
        let result = query(
            label,
            &session,
            &rows_sql(&["ST_GeomFromText('POINT(40 -3)', 4326)".into()]),
        )
        .await;
        let point = if label == "mysql" {
            r#"{"type":"Point","coordinates":[-3.0,40.0]}"#
        } else {
            r#"{"type":"Point","coordinates":[40.0,-3.0]}"#
        };
        check(
            label,
            &session,
            result,
            "geometry",
            &[expect(point, Some(4326), GeoFormat::Mysql)],
            MapCrs::LonLat,
        );
        statement(label, &session, "DROP TABLE geo_places").await;
    }
}
