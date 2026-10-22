import unittest

import scip_headroom as H


def row(bucket, change, first):
    return {"bucket": bucket, "refs_change": change, "def_from_use_first": first, "other_share": 0.5 if change else 0.0}


class HeadroomTests(unittest.TestCase):
    def test_a_name_counts_once_when_either_answer_changes(self):
        s = H.summary([row("1", False, True), row("2-5", True, False), row("6+", True, True), row("2-5", False, False), row("1", False, None)])
        self.assertEqual((s["n"], s["refs_change"], s["def_judged"], s["def_miss"], s["either"]), (5, 2, 4, 2, 3))
        self.assertEqual(s["by_bucket"]["2-5"], {"n": 2, "refs_change": 1, "def_miss": 2})

    def test_population_keeps_named_in_repo_definitions_with_references(self):
        syms = {
            "a#run().": {"name": "run", "kind": "method", "defs": {("a.py", 1)}, "refs": {("b.py", 2), ("b.py", 3)}},
            "c#run().": {"name": "run", "kind": "method", "defs": set(), "refs": {("b.py", 4)}},
            "m/": {"name": "mod", "kind": "namespace", "defs": {("m.py", 1)}, "refs": {("x", 1), ("y", 1), ("z", 1)}},
            "a#go().": {"name": "go", "kind": "method", "defs": {("a.py", 9)}, "refs": {("b.py", 1)} | {("c.py", i) for i in range(5)}},
        }
        pop = H.population(syms)
        self.assertEqual(sorted(pop), ["run"])
        self.assertEqual((pop["run"]["local"], pop["run"]["refs"]), (["a#run()."], 3))


if __name__ == "__main__":
    unittest.main()
