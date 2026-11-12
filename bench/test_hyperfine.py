import json
import unittest

from bench import hyperfine_results

# hyperfine 1.x: one flat object per command
EXPORT_1 = """{"results": [{
  "command": "true", "mean": 0.0013, "stddev": 0.0001, "median": 0.0012, "user": 0.0004,
  "system": 0.0003, "min": 0.0011, "max": 0.0015, "times": [0.0011, 0.0012, 0.0015],
  "exit_codes": [0, 0, 0]
}]}"""

# hyperfine 2.0.0: a `-N --ignore-failure "true" "false"` export, values rounded, other metrics cut
EXPORT_2 = """{"schema_version": 2,
 "metadata": {"hyperfine_version": "2.0.0", "platform": {"os": "Darwin", "architecture": "arm64"}},
 "primary_metric": "time_wall_clock",
 "results": [
  {"command": "true",
   "measurements": [
    {"time_wall_clock": {"value": 0.0011, "unit": "second"}, "time_user": {"value": 0.0004, "unit": "second"},
     "time_system": {"value": 0.0003, "unit": "second"}, "instructions": {"value": 7911874}, "exit_code": 0},
    {"time_wall_clock": {"value": 0.0012, "unit": "second"}, "time_user": {"value": 0.0004, "unit": "second"},
     "time_system": {"value": 0.0003, "unit": "second"}, "instructions": {"value": 7882739}, "exit_code": 0},
    {"time_wall_clock": {"value": 0.0015, "unit": "second"}, "time_user": {"value": 0.0004, "unit": "second"},
     "time_system": {"value": 0.0003, "unit": "second"}, "instructions": {"value": 7919657}, "exit_code": 0}],
   "summary": {
    "instructions": {"count": 3, "mean": 7904756.67, "stddev": 19460.9, "median": 7911874.0, "min": 7882739.0, "max": 7919657.0},
    "time_system": {"unit": "second", "count": 3, "mean": 0.0003, "stddev": 0.0, "median": 0.0003, "min": 0.0003, "max": 0.0003},
    "time_user": {"unit": "second", "count": 3, "mean": 0.0004, "stddev": 0.0, "median": 0.0004, "min": 0.0004, "max": 0.0004},
    "time_wall_clock": {"unit": "second", "count": 3, "mean": 0.0013, "stddev": 0.0001, "median": 0.0012, "min": 0.0011, "max": 0.0015}}},
  {"command": "false",
   "measurements": [{"time_wall_clock": {"value": 0.0012, "unit": "second"}, "exit_code": 1}],
   "summary": {
    "time_system": {"unit": "second", "count": 1, "mean": 0.0003, "stddev": null, "median": 0.0003, "min": 0.0003, "max": 0.0003},
    "time_user": {"unit": "second", "count": 1, "mean": 0.0004, "stddev": null, "median": 0.0004, "min": 0.0004, "max": 0.0004},
    "time_wall_clock": {"unit": "second", "count": 1, "mean": 0.0012, "stddev": null, "median": 0.0012, "min": 0.0012, "max": 0.0012}}}
 ]}"""

TIMES = {"mean": 0.0013, "median": 0.0012, "stddev": 0.0001, "min": 0.0011, "max": 0.0015, "user": 0.0004, "system": 0.0003, "n": 3}


class HyperfineResults(unittest.TestCase):
    def test_1x_export(self):
        self.assertEqual(hyperfine_results(json.loads(EXPORT_1)), [TIMES])

    def test_2_0_export_gives_the_same_fields(self):
        first, second = hyperfine_results(json.loads(EXPORT_2))
        self.assertEqual(first, {**TIMES, "instructions": 7911874.0})
        self.assertEqual(second, {"mean": 0.0012, "median": 0.0012, "stddev": 0.0, "min": 0.0012, "max": 0.0012, "user": 0.0004, "system": 0.0003, "n": 1})

    def test_no_results(self):
        self.assertEqual(hyperfine_results({}), [])


if __name__ == "__main__":
    unittest.main()
