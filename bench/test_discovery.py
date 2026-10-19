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

    def test_rg_ranks_files_by_distinct_words_then_hits(self):
        with tempfile.TemporaryDirectory() as d:
            for name, text in [("one.rs", "timer timer timer\n"), ("two.rs", "timer wheel\n"), ("none.rs", "x\n")]:
                with open(os.path.join(d, name), "w") as f:
                    f.write(text)
            self.assertEqual(discovery.rank_rg(d, ["timer", "wheel"]), ["two.rs", "one.rs"])
            self.assertEqual(discovery.rank_rg(d, []), [])

    def test_the_frozen_dataset_is_whole(self):
        d = tomllib.load(open(discovery.DATASET, "rb"))
        ids = [x["id"] for x in d["query"]] + [x["id"] for x in d["swebench"]]
        self.assertEqual(len(ids), len(set(ids)))
        self.assertEqual(Counter(x["corpus"] for x in d["query"]), {"tokio": 30, "TypeScript-5.9": 30, "ktor": 30})
        self.assertEqual(Counter(x["split"] for x in d["query"]), {"dev": 30, "heldout": 60})
        self.assertEqual(Counter(x["split"] for x in d["swebench"]), {"dev": 40, "heldout": 80})
        for x in d["query"] + d["swebench"]:
            self.assertTrue(x["files"], x["id"])
        self.assertEqual(set(d["corpora"]), {"tokio", "TypeScript-5.9", "ktor"})


if __name__ == "__main__":
    unittest.main()
