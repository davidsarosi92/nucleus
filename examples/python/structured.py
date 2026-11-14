"""A model's tool calls and structured outputs (PLAN.md section M14.15):
stored as JSON, checked against the tool's JSON Schema as they are
written — a call that does not fit is refused, and the model is told
where it does not —, then queried: by containment, by an expression
index, and as a table (JSON_TABLE).

    python3 examples/python/structured.py                    # the local server
    python3 examples/python/structured.py file:calls.db      # in this process
    python3 examples/python/structured.py nucleus://nucleus:nucleus@127.0.0.1:20035/nucleus

The "model" here is a list of made-up calls, one of them wrong, so it
runs anywhere; a real application stores what its model returns.  The
schema is the one the model was given (the tool's `input_schema`, or a
structured output's `json_schema`): the same document checks its answer.
"""

import json
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "..", "drivers", "python"))
import nucleus  # noqa: E402

port = int(os.environ.get("NUCLEUS_PORT", nucleus.DEFAULT_PORT))
url = sys.argv[1] if len(sys.argv) > 1 else f"nucleus://nucleus:nucleus@127.0.0.1:{port}/nucleus"

# The tool the model calls, as it was described to the model.
GET_WEATHER = {
    "type": "object",
    "properties": {
        "city": {"type": "string", "minLength": 1},
        "unit": {"enum": ["celsius", "fahrenheit"]},
        "days": {"type": "integer", "minimum": 1, "maximum": 14},
        "hours": {"type": "array", "items": {"type": "integer", "minimum": 0, "maximum": 23}},
    },
    "required": ["city", "unit"],
    "additionalProperties": False,
}

# What the model answered, turn by turn.
CALLS = [
    {"city": "Budapest", "unit": "celsius", "days": 3},
    {"city": "Oslo", "unit": "fahrenheit", "hours": [8, 12, 18]},
    {"city": "Lima", "unit": "kelvin", "days": 30, "lang": "es"},
    {"city": "Tokyo", "unit": "celsius", "days": 7, "hours": [9]},
]


def setup(db):
    db.execute("DROP TABLE IF EXISTS tool_calls")
    db.execute("DROP TABLE IF EXISTS tool_schemas")
    # The tools' schemas, kept with the calls: one place says what fits.
    db.execute("CREATE TABLE tool_schemas (tool TEXT PRIMARY KEY, schema JSON NOT NULL)")
    db.execute("INSERT INTO tool_schemas VALUES ('get_weather', $1)", [GET_WEATHER])
    # A call is stored only if it fits.  (A CHECK reads its own row: the
    # schema is written into it, the same document as above.)
    db.execute(
        f"""CREATE TABLE tool_calls (
              id INT PRIMARY KEY,
              tool TEXT NOT NULL,
              args JSON NOT NULL
                CHECK (tool <> 'get_weather' OR json_matches_schema(args, '{json.dumps(GET_WEATHER)}')))"""
    )
    # Found by what they contain, and by a city's name.
    db.execute("CREATE INDEX tool_calls_args ON tool_calls USING json (args)")
    db.execute("CREATE INDEX tool_calls_city ON tool_calls ((args->>'city'))")


def store(db, i, args):
    """Stores the model's call; where it does not fit, what to tell the
    model — each problem with the place it is at."""
    try:
        db.execute("INSERT INTO tool_calls VALUES ($1, 'get_weather', $2)", [i, args])
        return None
    except nucleus.Error as e:
        if e.code != "23514":  # check_violation
            raise
        return db.execute(
            "SELECT json_schema_errors($1, schema) FROM tool_schemas WHERE tool = 'get_weather'",
            [args],
        ).rows[0][0]


with nucleus.connect(url) as db:
    print("connected:", db.server_version)
    setup(db)

    for i, args in enumerate(CALLS, 1):
        problems = store(db, i, args)
        if problems:
            # Handed back to the model as the tool's error, so it tries again.
            print(f"call {i} refused:")
            for p in problems:
                print("  ", p)
            assert problems == [
                '$.days: 30 is greater than the maximum 14',
                '$: the property "lang" is not allowed',
                '$.unit: "kelvin" is not one of "celsius", "fahrenheit"',
            ], problems
        else:
            print(f"call {i} stored")

    # Containment, served by the json index (and checked again on its rows).
    rows = db.execute(
        "SELECT id FROM tool_calls WHERE args @> $1 ORDER BY id", [{"unit": "celsius"}]
    ).rows
    print("in celsius:", [r[0] for r in rows])
    assert [r[0] for r in rows] == [1, 4]
    plan = "\n".join(r[0] for r in db.execute(
        "EXPLAIN SELECT id FROM tool_calls WHERE args @> '{\"unit\": \"celsius\"}'").rows)
    assert "tool_calls_args" in plan, plan

    # The expression index, by a city's name.
    plan = "\n".join(r[0] for r in db.execute(
        "EXPLAIN SELECT id FROM tool_calls WHERE args->>'city' = 'Oslo'").rows)
    assert "tool_calls_city" in plan, plan

    # A JSON value comes back as Python's.
    args = db.execute("SELECT args FROM tool_calls WHERE id = 2").rows[0][0]
    assert args == CALLS[1], args

    # The calls as a table: one row per hour asked for.
    rows = db.execute(
        """SELECT c.id, h.city, h.hour
             FROM tool_calls c,
                  JSON_TABLE(c.args, '$' COLUMNS (
                      city TEXT PATH '$.city',
                      NESTED PATH '$.hours[*]' COLUMNS (hour INT PATH '$'))) AS h
            WHERE h.hour IS NOT NULL
            ORDER BY c.id, h.hour"""
    ).rows
    for r in rows:
        print("  hour asked:", r)
    assert rows == [(2, "Oslo", 8), (2, "Oslo", 12), (2, "Oslo", 18), (4, "Tokyo", 9)], rows

    db.execute("DROP TABLE tool_calls")
    db.execute("DROP TABLE tool_schemas")
    print("ok")
