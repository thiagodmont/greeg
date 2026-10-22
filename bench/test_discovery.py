import os, subprocess, tempfile, tomllib, unittest
from collections import Counter

import discovery


class DiscoveryTests(unittest.TestCase):
    def test_words_are_distinct_lowercased_and_skip_stop_words(self):
        self.assertEqual(discovery.words("Where does the Timer reset a timer_wheel entry? It is reset."),
                         ["timer", "reset", "timer_wheel", "entry"])

    def test_first_hit_and_metrics_use_any_gold_file(self):
        rs = [{"ranked": ["a", "b", "c"], "gold": ["c", "z"]}, {"ranked": ["x"], "gold": ["y"]}]
        self.assertEqual(discovery.first_hit(rs[0]["ranked"], rs[0]["gold"]), 3)
        m = discovery.metrics(rs)
        self.assertEqual((m["acc@1"], m["acc@5"], m["mrr"]), ([0.0, 0.0], [1.0, 0.0], [1 / 3, 0.0]))

    def test_held_out_answers_match_by_digest(self):
        gold = [discovery.digest("src/a.rs")]
        self.assertEqual(discovery.first_hit(["b.rs", "src/a.rs"], gold, hashed=True), 2)
        self.assertIsNone(discovery.first_hit(["b.rs", "src/a.rs"], ["src/a.rs"], hashed=True))

    def test_paired_difference_needs_the_same_queries(self):
        a = [{"id": "q1", "ranked": ["g"], "gold": ["g"]}, {"id": "q2", "ranked": [], "gold": ["g"]}]
        b = [{"id": "q1", "ranked": [], "gold": ["g"]}, {"id": "q2", "ranked": [], "gold": ["g"]}]
        mean, lo, hi = discovery.paired(a, b, "acc@5")
        self.assertEqual(mean, 0.5)
        with self.assertRaises(SystemExit):
            discovery.paired(a, b[:1], "acc@5")

    def test_rg_ranks_files_by_distinct_words_then_hits(self):
        with tempfile.TemporaryDirectory() as d:
            for name, text in [("one.rs", "timer timer timer\n"), ("two.rs", "timer wheel\n"), ("none.rs", "x\n")]:
                with open(os.path.join(d, name), "w") as f:
                    f.write(text)
            self.assertEqual(discovery.rank_rg(d, ["timer", "wheel"]), ["two.rs", "one.rs"])
            self.assertEqual(discovery.rank_rg(d, []), [])

    def test_greeg_find_passes_the_question_whole_and_keeps_file_order(self):
        with tempfile.TemporaryDirectory() as d:
            stub = os.path.join(d, "greeg")
            with open(stub, "w") as f:
                # answers with the argument after `--` as the second file
                f.write("#!/bin/sh\nfor a; do q=$a; done\n"
                        "printf '%s\\n' '{\"type\":\"greeg\",\"data\":{}}' '{\"type\":\"file\",\"data\":{\"path\":{\"text\":\"b.rs\"}}}'\n"
                        "printf '{\"type\":\"file\",\"data\":{\"path\":{\"text\":\"%s\"}}}\\n' \"$q\"\n"
                        "printf '%s\\n' '{\"type\":\"footer\",\"data\":{}}'\n")
            os.chmod(stub, 0o755)
            self.assertEqual(discovery.rank_greeg_find(stub, d, "-x where", os.path.join(d, "idx")),
                             ["b.rs", "-x where"])

    def test_the_frozen_dataset_is_whole(self):
        d = tomllib.load(open(discovery.DATASET, "rb"))
        ids = [x["id"] for x in d["query"]] + [x["id"] for x in d["swebench"]]
        self.assertEqual(len(ids), len(set(ids)))
        self.assertEqual(Counter(x["corpus"] for x in d["query"]), {"tokio": 30, "TypeScript-5.9": 30, "ktor": 30})
        self.assertEqual(Counter(x["split"] for x in d["query"]), {"dev": 30, "heldout": 60})
        self.assertEqual(Counter(x["split"] for x in d["swebench"]), {"dev": 40, "heldout": 80})
        for x in d["query"] + d["swebench"]:
            # held-out answers only as digests
            self.assertEqual("files_sha256" in x, x["split"] == "heldout", x["id"])
            self.assertTrue(x.get("files") or x.get("files_sha256"), x["id"])
        for x in d["swebench"]:
            self.assertEqual(len(x["statement_sha256"]), 64, x["id"])
        self.assertEqual(set(d["corpora"]), {"tokio", "TypeScript-5.9", "ktor"})


if __name__ == "__main__":
    unittest.main()
