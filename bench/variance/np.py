"""numpy's var(ddof=1) on the data of var.rs (PLAN.md §M14.27): python3 np.py"""
import numpy as np, time
rng = np.random.default_rng(1)
for name, xs in [("1e9 + U(0,1)", 1e9 + rng.random(10_000_000)), ("N(0,1)", rng.standard_normal(10_000_000))]:
    best = 1e9
    for _ in range(5):
        t = time.perf_counter(); v = xs.var(ddof=1); best = min(best, time.perf_counter() - t)
    print(f"numpy {name}: var {v:.17e}, {best * 1e9 / xs.size:.2f} ns a value (two passes)")
