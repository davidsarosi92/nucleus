#!/usr/bin/env python3
"""The boundary check (PLAN.md §M14.19), in both directions:

- the core (crates/) depends on no module (modules/) and no edition
  (editions/);
- a module (and the sample one, examples/extension) depends on nucleus's
  public API crates only — by version, and
  in its code on `nucleus_sql::ext` alone of the SQL layer — reads no files
  outside its folder, and carries its own README, changelog and licence.

Usage: scripts/check-boundaries.py   (exit status 1 on a violation)
"""

import pathlib
import re
import sys
import tomllib

ROOT = pathlib.Path(__file__).resolve().parent.parent
# What a module may depend on: the public API crates (and the test kit
# and the platform, for its tests on the simulation).
PUBLIC = {
    "nucleus-kv",
    "nucleus-sql",
    "nucleus-server",
    "nucleus-client",
    "nucleus-platform",
    "nucleus-testkit",
}
# Of nucleus-sql, a module's code may name only this.
SQL_API = {"ext"}

problems = []


def manifest(dir):
    with open(dir / "Cargo.toml", "rb") as f:
        return tomllib.load(f)


def deps(m):
    out = {}
    for table in ("dependencies", "dev-dependencies", "build-dependencies"):
        out.update(m.get(table, {}))
    for target in m.get("target", {}).values():
        for table in ("dependencies", "dev-dependencies", "build-dependencies"):
            out.update(target.get(table, {}))
    return out


def packages(glob):
    return {
        manifest(d)["package"]["name"]: d
        for d in sorted(ROOT.glob(glob))
        if (d / "Cargo.toml").exists()
    }


core = packages("crates/*")
# The sample module in examples/ is held to the same rules.
modules = {**packages("modules/*"), **packages("examples/extension")}
editions = packages("editions/*")
outside = set(modules) | set(editions)

for name, d in core.items():
    for dep, spec in deps(manifest(d)).items():
        path = spec.get("path", "") if isinstance(spec, dict) else ""
        if dep in outside or "modules/" in path or "editions/" in path:
            problems.append(f"{d.relative_to(ROOT)}: the core depends on {dep}")

for name, d in modules.items():
    rel = d.relative_to(ROOT)
    for dep, spec in deps(manifest(d)).items():
        if not dep.startswith("nucleus-"):
            continue
        if dep not in PUBLIC and dep not in modules:
            problems.append(f"{rel}: depends on {dep}, not a public API crate")
        if not isinstance(spec, dict) or "version" not in spec:
            problems.append(f"{rel}: {dep} has no version (a published copy needs one)")
        if isinstance(spec, dict) and spec.get("workspace"):
            problems.append(f"{rel}: {dep} is inherited from the workspace; say version and path")
    for f in ("README.md", "CHANGELOG.md", "LICENSE"):
        if not (d / f).exists():
            problems.append(f"{rel}: no {f}")
    for src in sorted(d.rglob("*.rs")):
        if "target" in src.relative_to(d).parts:
            continue
        text = src.read_text()
        where = src.relative_to(ROOT)
        for m in re.finditer(r"\bnucleus_sql::(\w+)", text):
            if m.group(1) not in SQL_API:
                line = text.count("\n", 0, m.start()) + 1
                problems.append(
                    f"{where}:{line}: nucleus_sql::{m.group(1)} — modules use nucleus_sql::ext only"
                )
        for m in re.finditer(r'"[^"\n]*\.\./[^"\n]*"', text):
            line = text.count("\n", 0, m.start()) + 1
            problems.append(f"{where}:{line}: a path out of the module's folder: {m.group(0)}")

if problems:
    print("boundary check failed:")
    for p in problems:
        print("  " + p)
    sys.exit(1)
print(
    f"boundary check passed: {len(core)} core crates, {len(modules)} modules, "
    f"{len(editions)} editions"
)
