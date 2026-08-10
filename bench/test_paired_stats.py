#!/usr/bin/env python3
"""Unit tests for bench/paired_stats.py - hand-computed fixtures.

Run: python bench/test_paired_stats.py
"""

import json
import os
import tempfile
import unittest

import paired_stats as ps


class TestMcNemarExact(unittest.TestCase):
    def test_known_exact_p(self):
        # b=1, c=8 -> n=9 discordant. Two-sided exact:
        # 2 * (C(9,0)+C(9,1)) / 2^9 = 2 * 10/512 = 0.0390625
        base = {f"t{i}": True for i in range(1)}  # baseline_only = 1
        base["x0"] = True
        for i in range(8):
            base[f"c{i}"] = False  # condition_only = 8
        cond = {"x0": False}
        for i in range(8):
            cond[f"c{i}"] = True
        r = ps.mcnemar_exact(base, cond)
        self.assertEqual(r["baseline_only"], 1)
        self.assertEqual(r["condition_only"], 8)
        self.assertEqual(r["discordant"], 9)
        self.assertAlmostEqual(r["p_value"], 0.0390625, places=10)
        self.assertTrue(r["significant"])

    def test_no_discordant_pairs(self):
        base = {"a": True, "b": False}
        cond = {"a": True, "b": False}
        r = ps.mcnemar_exact(base, cond)
        self.assertEqual(r["discordant"], 0)
        self.assertEqual(r["p_value"], 1.0)
        self.assertFalse(r["significant"])

    def test_negative_result_not_dropped(self):
        # b=3, c=4 -> n=7. 2*sum_{i=0..3}C(7,i)/2^7 = 2*64/128 = 1.0
        base, cond = {}, {}
        for i in range(3):
            base[f"b{i}"], cond[f"b{i}"] = True, False
        for i in range(4):
            base[f"c{i}"], cond[f"c{i}"] = False, True
        r = ps.mcnemar_exact(base, cond)
        self.assertEqual(r["discordant"], 7)
        self.assertAlmostEqual(r["p_value"], 1.0, places=10)
        self.assertFalse(r["significant"])  # reported, not dropped

    def test_no_shared_tasks_errors(self):
        with self.assertRaises(ValueError):
            ps.mcnemar_exact({"a": True}, {"b": True})

    def test_independent_exact_p_b2_c6(self):
        # b=2, c=6 -> n=8, k=2. 2*sum_{i=0..2}C(8,i)/2^8 = 2*37/256 = 0.2890625
        base, cond = {}, {}
        for i in range(2):
            base[f"b{i}"], cond[f"b{i}"] = True, False
        for i in range(6):
            base[f"c{i}"], cond[f"c{i}"] = False, True
        r = ps.mcnemar_exact(base, cond)
        self.assertEqual((r["baseline_only"], r["condition_only"]), (2, 6))
        self.assertEqual(r["discordant"], 8)
        self.assertAlmostEqual(r["p_value"], 0.2890625, places=10)
        self.assertFalse(r["significant"])

    def test_clean_sweep_five_not_quite_significant(self):
        # b=0, c=5 -> n=5, k=0. 2*C(5,0)/2^5 = 0.0625 (just misses p<0.05)
        base, cond = {}, {}
        for i in range(5):
            base[f"c{i}"], cond[f"c{i}"] = False, True
        r = ps.mcnemar_exact(base, cond)
        self.assertEqual((r["baseline_only"], r["condition_only"]), (0, 5))
        self.assertAlmostEqual(r["p_value"], 0.0625, places=10)
        self.assertFalse(r["significant"])

    def test_p_clamped_to_one(self):
        # b=1, c=1 -> n=2, k=1. 2*(C(2,0)+C(2,1))/4 = 1.5, clamped to 1.0
        r = ps.mcnemar_exact({"b": True, "c": False}, {"b": False, "c": True})
        self.assertEqual(r["discordant"], 2)
        self.assertAlmostEqual(r["p_value"], 1.0, places=10)

    def test_both_pass_and_fail_counts(self):
        base = {"p": True, "q": True, "f": False, "g": False}
        cond = {"p": True, "q": True, "f": False, "g": False}
        r = ps.mcnemar_exact(base, cond)
        self.assertEqual(r["both_pass"], 2)
        self.assertEqual(r["both_fail"], 2)
        self.assertEqual(r["discordant"], 0)

    def test_pairs_only_on_shared_ids(self):
        # extra ids on either side are ignored; only shared are paired
        base = {"a": True, "b": False, "only_base": True}
        cond = {"a": False, "b": True, "only_cond": True}
        r = ps.mcnemar_exact(base, cond)
        self.assertEqual(r["n_paired"], 2)
        self.assertEqual((r["baseline_only"], r["condition_only"]), (1, 1))


class TestBootstrapCI(unittest.TestCase):
    def test_deterministic_n1(self):
        ci = ps.bootstrap_ci([0.42])
        self.assertEqual(ci["n_seeds"], 1)
        self.assertIsNone(ci["ci_low"])
        self.assertEqual(ci["note"], "deterministic, n=1")
        self.assertAlmostEqual(ci["mean"], 0.42)

    def test_ci_brackets_mean_fixed_seed(self):
        vals = [0.40, 0.50, 0.60, 0.55, 0.45]
        ci = ps.bootstrap_ci(vals)
        self.assertEqual(ci["n_seeds"], 5)
        self.assertIsNotNone(ci["ci_low"])
        self.assertLessEqual(ci["ci_low"], ci["mean"])
        self.assertGreaterEqual(ci["ci_high"], ci["mean"])
        self.assertAlmostEqual(ci["mean"], statistics_mean(vals))

    def test_ci_reproducible(self):
        vals = [0.3, 0.7, 0.5, 0.9, 0.1, 0.6]
        a = ps.bootstrap_ci(vals)
        b = ps.bootstrap_ci(vals)
        self.assertEqual((a["ci_low"], a["ci_high"]), (b["ci_low"], b["ci_high"]))

    def test_n2_degrades_no_ci(self):
        # n<3 but not n==1: no CI, note explains the shortfall (per contract)
        ci = ps.bootstrap_ci([0.4, 0.6])
        self.assertEqual(ci["n_seeds"], 2)
        self.assertIsNone(ci["ci_low"])
        self.assertIsNone(ci["ci_high"])
        self.assertEqual(ci["note"], "n=2, CI needs n>=3")
        self.assertAlmostEqual(ci["mean"], 0.5)

    def test_n3_gets_ci(self):
        # n exactly 3 is the boundary that earns a CI
        ci = ps.bootstrap_ci([0.2, 0.5, 0.8])
        self.assertEqual(ci["n_seeds"], 3)
        self.assertIsNotNone(ci["ci_low"])
        self.assertLessEqual(ci["ci_low"], ci["mean"])
        self.assertGreaterEqual(ci["ci_high"], ci["mean"])
        self.assertIsNone(ci["note"])

    def test_degenerate_all_equal_ci_is_point(self):
        ci = ps.bootstrap_ci([0.5, 0.5, 0.5, 0.5])
        self.assertAlmostEqual(ci["ci_low"], 0.5)
        self.assertAlmostEqual(ci["ci_high"], 0.5)


class TestTaskOutcomes(unittest.TestCase):
    def test_majority_vote_odd_seeds(self):
        tasks = [
            {"task_id": "v", "resolved": True},
            {"task_id": "v", "resolved": True},
            {"task_id": "v", "resolved": False},  # 2/3 -> pass
            {"task_id": "w", "resolved": True},
            {"task_id": "w", "resolved": False},
            {"task_id": "w", "resolved": False},  # 1/3 -> fail
        ]
        self.assertEqual(ps.task_outcomes(tasks), {"v": True, "w": False})

    def test_even_seed_tie_breaks_to_pass(self):
        tasks = [
            {"task_id": "t", "resolved": True},
            {"task_id": "t", "resolved": False},  # 1/2 tie -> pass
            {"task_id": "u", "resolved": False},
            {"task_id": "u", "resolved": False},  # 0/2 -> fail
        ]
        self.assertEqual(ps.task_outcomes(tasks), {"t": True, "u": False})

    def test_single_seed_passthrough(self):
        tasks = [{"task_id": "a", "resolved": True}, {"task_id": "b", "resolved": False}]
        self.assertEqual(ps.task_outcomes(tasks), {"a": True, "b": False})


class TestOutcome(unittest.TestCase):
    def test_key_precedence_resolved_first(self):
        # first present key wins in OUTCOME_KEYS order (resolved, passed, success)
        self.assertFalse(ps.outcome({"resolved": False, "passed": True}))

    def test_passed_when_no_resolved(self):
        self.assertTrue(ps.outcome({"passed": True}))

    def test_missing_outcome_errors(self):
        with self.assertRaises(ValueError):
            ps.outcome({"task_id": "z"})


class TestCellLabel(unittest.TestCase):
    def test_refuses_blended_model(self):
        tasks = [
            {"task_id": "a", "model": "m1", "benchmark": "swebench", "condition": "x"},
            {"task_id": "b", "model": "m2", "benchmark": "swebench", "condition": "x"},
        ]
        with self.assertRaises(ValueError):
            ps.cell_label(tasks, "all")

    def test_full_cell_fields(self):
        tasks = [
            {"task_id": "a", "model": "gemma", "benchmark": "swebench",
             "condition": "spelunk", "seed": 42},
            {"task_id": "b", "model": "gemma", "benchmark": "swebench",
             "condition": "spelunk", "seed": 43},
        ]
        label = ps.cell_label(tasks, "django-only")
        self.assertEqual(label["model"], "gemma")
        self.assertEqual(label["harness"], "swebench")
        self.assertEqual(label["condition"], "spelunk")
        self.assertEqual(label["instance_filter"], "django-only")
        self.assertEqual(label["n_tasks"], 2)
        self.assertEqual(label["seeds"], [42, 43])

    def _base(self, **over):
        t = {"task_id": "x", "model": "m", "benchmark": "h", "condition": "c"}
        t.update(over)
        return t

    def test_refuses_blended_harness(self):
        tasks = [self._base(task_id="a", benchmark="swebench"),
                 self._base(task_id="b", benchmark="humaneval")]
        with self.assertRaises(ValueError):
            ps.cell_label(tasks, "all")

    def test_refuses_blended_condition(self):
        tasks = [self._base(task_id="a", condition="baseline"),
                 self._base(task_id="b", condition="spelunk")]
        with self.assertRaises(ValueError):
            ps.cell_label(tasks, "all")

    def test_model_source_appended(self):
        tasks = [self._base(model="gemma", model_source="local")]
        self.assertEqual(ps.cell_label(tasks, "all")["model"], "gemma (local)")


class TestLoadTasks(unittest.TestCase):
    def _write(self, obj):
        # mkstemp (not mktemp) creates the file atomically: no window between
        # generating the name and opening it for a symlink to land in.
        fd, p = tempfile.mkstemp(suffix=".json")
        with os.fdopen(fd, "w") as f:
            json.dump(obj, f)
        self.addCleanup(lambda: os.path.exists(p) and os.remove(p))
        return p

    def test_dict_form_extracts_tasks(self):
        p = self._write({"aggregate": {}, "tasks": [{"task_id": "a", "resolved": True}]})
        self.assertEqual([t["task_id"] for t in ps.load_tasks(p)], ["a"])

    def test_skipped_and_errored_dropped(self):
        p = self._write([
            {"task_id": "a", "resolved": True},
            {"task_id": "b", "resolved": True, "skipped": True},
            {"task_id": "c", "resolved": True, "error": "boom"},
        ])
        self.assertEqual([t["task_id"] for t in ps.load_tasks(p)], ["a"])

    def test_non_list_payload_errors(self):
        p = self._write({"not_tasks": 1})
        with self.assertRaises(ValueError):
            ps.load_tasks(p)


def statistics_mean(vals):
    return sum(vals) / len(vals)


if __name__ == "__main__":
    unittest.main(verbosity=2)
