"""The Python nucleus codegen makes (scripts/driver-tests.sh codegen): its
dataclasses take the rows, and the values are what the annotations say."""

import array
import dataclasses
import datetime
import decimal
import ipaddress
import os
import sys
import typing
import uuid

here = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(here, "..", "..", "..", "drivers", "python"))
sys.path.insert(0, sys.argv[1])
import nucleus  # noqa: E402
import schema  # noqa: E402
import queries  # noqa: E402

conn = nucleus.connect(port=int(os.environ["NUCLEUS_PORT"]))
a, b = [schema.CgEvery.from_row(r) for r in conn.execute(schema.CgEvery.SELECT + " ORDER BY id").rows]
hints = typing.get_type_hints(schema.CgEvery, vars(schema))


def conforms(value, hint):
    """Whether value is of the annotation hint."""
    origin, args = typing.get_origin(hint), typing.get_args(hint)
    if hint is typing.Any:
        return True
    if origin is typing.Union:
        return any(conforms(value, h) for h in args)
    if origin is typing.Literal:
        return value in args
    if origin is list:
        return isinstance(value, list) and all(conforms(v, args[0]) for v in value)
    if hint is type(None):
        return value is None
    if hint is float:
        return isinstance(value, float)
    if hint is int:
        return isinstance(value, int) and not isinstance(value, bool)
    if hint is datetime.date:
        return isinstance(value, datetime.date) and not isinstance(value, datetime.datetime)
    return isinstance(value, hint)


for row in (a, b):
    for f in dataclasses.fields(row):
        v = getattr(row, f.name)
        assert conforms(v, hints[f.name]), (f.name, v, hints[f.name])
assert a.n == decimal.Decimal("12.34") and a.m == "happy" and a.select == "sel" and a.Two_Words == "tw"
assert isinstance(a.vec, array.array) and isinstance(a.ip, ipaddress.IPv4Interface)
assert a.u == uuid.UUID("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11") and a.j == {"a": [1, 2]}
assert a.ia == [1, None, 3] and b.n is None and b.i == 7
assert "start" in [e.kind for e in map(schema.CgEvents.from_row, conn.execute(schema.CgEvents.SELECT).rows)]
assert not hasattr(schema, "CgEvents2025")
# The queries' functions (PLAN.md §M14.16 M6), their rows as annotated.
e = queries.every_by_id(conn, 1)
assert e is not None and e.n == decimal.Decimal("12.34") and e.m == "happy" and e.Two_Words == "tw"
assert queries.every_by_id(conn, 99) is None
assert [r.i for r in queries.every_ids(conn, 0, 10)] == [3, 7]
assert [r.id for r in queries.every_by_mood(conn, "happy")] == [1]
assert queries.add_event(conn, datetime.date(2025, 6, 1), 11, "py") == 1
renamed = queries.rename_event(conn, 11, None)
assert renamed is not None and renamed.id == 11 and renamed.kind is None
kinds = queries.kinds_after(conn, datetime.date(2025, 1, 1))
assert any(k.kind == "start" and k.n == 1 and k.t == "x" for k in kinds), kinds
for row in [e, renamed, *kinds]:
    row_hints = typing.get_type_hints(type(row), vars(queries))
    for f in dataclasses.fields(row):
        assert conforms(getattr(row, f.name), row_hints[f.name]), (f.name, getattr(row, f.name))
conn.close()
print("codegen python: ok")
