-- PostGIS for the map view's tests (ADR-045), in a database of its own: the extension adds
-- many functions and a table, and the `spelaion` database's catalog must stay plain.
CREATE DATABASE spelaion_gis;
\connect spelaion_gis
CREATE EXTENSION postgis;
