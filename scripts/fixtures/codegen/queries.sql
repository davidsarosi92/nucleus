-- The named queries scripts/driver-tests.sh's codegen step generates
-- functions from (PLAN.md §M14.16 M6), over schema.sql.

-- name: every_by_id :one
-- A row of every kind of value, by its id.
SELECT id, b, n, m, bin, tz, u, j, vec, ia, ta, "Two Words" FROM cg_every WHERE id = $1;

-- name: every_ids :many
SELECT id, i FROM cg_every WHERE i >= $1 ORDER BY id LIMIT $2;

-- name: every_by_mood :many
SELECT id, m FROM cg_every WHERE m = $1;

-- name: kinds_after :many
-- How many events of each kind after a day, the rows of cg_every they
-- share an id with beside them (a LEFT JOIN: its column may be NULL).
SELECT e.kind, count(*) AS n, max(v.t) AS t FROM cg_events e LEFT JOIN cg_every v ON v.id = e.id
WHERE e.at > $1 GROUP BY e.kind ORDER BY e.kind;

-- name: add_event :exec
INSERT INTO cg_events (at, id, kind) VALUES ($1, $2, $3);

-- name: rename_event :one
-- params: id, kind?
UPDATE cg_events SET kind = $2 WHERE id = $1 RETURNING id, kind;
