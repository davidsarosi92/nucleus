"""bench/method.py's statistics and bookkeeping (PLAN.md §M14.31 P1):
python3 bench/test_method.py."""

import itertools
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import method  # noqa: E402


class Statistics(unittest.TestCase):
    def test_median_and_cv(self):
        self.assertEqual(method.median([3, 1, 2]), 2)
        self.assertEqual(method.median([4, 1, 2, 3]), 2.5)
        self.assertEqual(method.cv([5, 5, 5]), 0)
        self.assertAlmostEqual(method.cv([9, 10, 11]), 0.1)

    def test_the_interval_holds_the_median_and_is_seeded(self):
        xs = [10.0, 10.4, 9.8, 10.1, 30.0, 10.2, 9.9]
        lo, hi = method.bootstrap_ci(xs)
        self.assertLessEqual(lo, method.median(xs))
        self.assertGreaterEqual(hi, method.median(xs))
        self.assertLess(hi, 30.0, "an outlier does not stretch the median's interval to itself")
        self.assertEqual(method.bootstrap_ci(xs), (lo, hi))

    def test_disjoint_intervals_alone_are_a_difference(self):
        a = method.Sample("q", "a", [10, 10.1, 9.9, 10, 10.2])
        b = method.Sample("q", "b", [10.1, 10, 10.3, 9.8, 10])
        c = method.Sample("q", "c", [20, 20.2, 19.9, 20.1, 20])
        self.assertFalse(method.disjoint(a, b))
        self.assertTrue(method.disjoint(a, c))


class Running(unittest.TestCase):
    def test_a_noisy_measurement_is_run_again_then_flagged(self):
        noisy = itertools.cycle([1, 10])
        s = method.repeat(lambda: next(noisy), "q", "x", runs=5, retries=2)
        self.assertTrue(s.flagged)
        self.assertEqual(s.attempts, 3)
        steady = method.repeat(lambda: 5.0, "q", "x")
        self.assertFalse(steady.flagged)
        self.assertEqual((steady.attempts, len(steady.times)), (1, 5))

    def test_systems_interleaved(self):
        order = []
        out = method.interleave({"a": lambda: order.append("a") or 1.0,
                                 "b": lambda: order.append("b") or 2.0}, "q", runs=3)
        self.assertEqual(order, ["a", "b"] * 3)
        self.assertEqual(out["b"].median, 2.0)

    def test_a_wrong_answer_is_refused(self):
        method.check("q", [["1.000"]], [["1"]], normalize=lambda c: float(c))
        with self.assertRaises(AssertionError):
            method.check("q", [["2"]], [["1"]])

    def test_a_scripts_own_numbers_gathered_run_by_run(self):
        emit = ("import sys; sys.path.insert(0, 'bench'); import method; "
                "method.emit('build', [3.0, 3.1]); method.emit('recall', 0.97, unit='recall', phase='cold'); "
                "method.emit_system('nucleus', {'version': 'x'}); print('chatter')")
        samples, systems = method.run_commands({"nucleus": [sys.executable, "-c", emit]}, runs=3, echo=False)
        by = {(s.name, s.phase): s for s in samples}
        self.assertEqual(len(by[("build", "warm")].times), 6)
        self.assertEqual(by[("recall", "cold")].times, [0.97] * 3)
        self.assertEqual(systems, {"nucleus": {"version": "x"}})
        self.assertIn("build, ms", method.table(samples))


class Results(unittest.TestCase):
    def test_written_read_shown_and_reproduced(self):
        env = method.environment(systems={"nucleus": {"version": "test"}}, where=".")
        self.assertIn("cpu", env["machine"])
        self.assertTrue(env["nucleus"]["commit"])
        a = [method.Sample("scan", "nucleus", [10, 11, 10.5, 10.2, 10.8]),
             method.Sample("scan", "postgresql", [20, 21, 20.5, 20.2, 20.8])]
        b = [method.Sample("scan", "nucleus", [10.4, 10.9, 10.1, 10.6, 10.3]),
             method.Sample("scan", "postgresql", [30, 31, 30.5, 30.2, 30.8])]
        with tempfile.TemporaryDirectory() as d:
            method.write(os.path.join(d, "a.json"), "t", env, a, {"baseline": "postgresql"})
            method.write(os.path.join(d, "b.json"), "t", env, b)
            first, second = method.read(os.path.join(d, "a.json")), method.read(os.path.join(d, "b.json"))
        self.assertEqual(method.reproduce(first, second), [("scan", "warm", "postgresql")])
        t = method.table(first["samples"], baseline="postgresql")
        self.assertIn("| scan | 10.5 [", t)
        self.assertIn("0.51×", t)
        self.assertIn("ms", method.table(a))
        self.assertIn("logical CPUs", method.describe(env))


if __name__ == "__main__":
    unittest.main()
