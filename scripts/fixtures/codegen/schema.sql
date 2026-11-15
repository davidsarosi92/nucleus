-- The schema scripts/driver-tests.sh's codegen step generates types from
-- (PLAN.md §M14.16 M5): every type, nullable and not, names that are no
-- identifier, a partitioned table (its partitions get no type), tables
-- named as what the generated code uses (Value, Date, List, String).
DROP TABLE IF EXISTS cg_every;
DROP TABLE IF EXISTS cg_events;
DROP TABLE IF EXISTS value;
DROP TABLE IF EXISTS date;
DROP TABLE IF EXISTS list;
DROP TABLE IF EXISTS string;
DROP TYPE IF EXISTS cg_mood;
CREATE TYPE cg_mood AS ENUM ('sad', 'ok', 'happy');
CREATE TABLE cg_every (id bigint PRIMARY KEY, b boolean, s smallint, i integer NOT NULL, r real, d double precision, n numeric(10,2), t text, v varchar(20), c char(3), m cg_mood, bin bytea, dt date, ts timestamp, tz timestamptz, iv interval, tm time, ip inet, net cidr, u uuid, j json, vec vector(3), sv sparsevec(5), ia integer[], ta text[], "select" text, "Two Words" text);
INSERT INTO cg_every VALUES (1, true, 2, 3, 1.5, 2.5, 12.34, 'x', 'y', 'abc', 'happy', '\x0102', '2024-01-02', '2024-01-02 03:04:05', '2024-01-02 03:04:05+00', '1 day 2 hours', '10:11:12', '10.0.0.1', '10.0.0.0/8', 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11', '{"a": [1, 2]}', '[1,2,3]', '{1:1.5,3:2}/5', ARRAY[1,NULL,3], ARRAY['a','b'], 'sel', 'tw');
INSERT INTO cg_every (id, i) VALUES (2, 7);
CREATE TABLE cg_events (at date NOT NULL, id int NOT NULL, kind text, PRIMARY KEY (at, id)) PARTITION BY RANGE (at);
CREATE TABLE cg_events_2025 PARTITION OF cg_events FOR VALUES FROM ('2025-01-01') TO ('2026-01-01');
INSERT INTO cg_events VALUES ('2025-03-01', 1, 'start');
CREATE TABLE value (v integer);
CREATE TABLE date (d date);
CREATE TABLE list (l text);
CREATE TABLE string (s text);
