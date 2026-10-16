import unittest

from qualify import glibc_floor, macos_floor

# `objdump -p` of the 0.11.0 aarch64 archive: GLIBC_2.39 is needed by weak
# symbols only, but the entry is not flagged weak, so the loader requires it
AARCH64_0_11 = """
Version References:
  required from ld-linux-aarch64.so.1:
    0x06969197 0x00 13 GLIBC_2.17
  required from libc.so.6:
    0x069691b3 0x00 12 GLIBC_2.33
    0x069691b2 0x00 11 GLIBC_2.32
    0x069691b0 0x00 10 GLIBC_2.30
    0x069691b4 0x00 09 GLIBC_2.34
    0x069691b9 0x00 08 GLIBC_2.39
"""

# the x86_64 archive: the same versions, but 2.30 and 2.39 flagged weak
X86_64_0_11 = """
Version References:
  required from libc.so.6:
    0x0d696913 0x00 14 GLIBC_2.3
    0x09691974 0x00 13 GLIBC_2.3.4
    0x069691b0 0x02 12 GLIBC_2.30
    0x069691b2 0x00 11 GLIBC_2.32
    0x069691b3 0x00 10 GLIBC_2.33
    0x069691b4 0x00 09 GLIBC_2.34
    0x069691b9 0x02 08 GLIBC_2.39
  required from libgcc_s.so.1:
    0x0b792650 0x00 07 GCC_3.0
"""


class FloorTests(unittest.TestCase):
    def test_a_version_the_loader_requires_sets_the_floor(self):
        self.assertEqual(glibc_floor(AARCH64_0_11), (2, 39))

    def test_versions_flagged_weak_do_not_raise_the_floor(self):
        self.assertEqual(glibc_floor(X86_64_0_11), (2, 34))

    def test_no_version_references_has_no_floor(self):
        self.assertIsNone(glibc_floor("Dynamic Section:\n  NEEDED libc.so.6\n"))

    def test_macos_floor_from_either_load_command(self):
        build = "Load command 9\n      cmd LC_BUILD_VERSION\n  cmdsize 32\n platform 1\n    minos 11.0\n      sdk 15.5\n"
        legacy = "Load command 9\n      cmd LC_VERSION_MIN_MACOSX\n  cmdsize 16\n  version 10.12\n      sdk 15.5\n"
        self.assertEqual(macos_floor(build), "11.0")
        self.assertEqual(macos_floor(legacy), "10.12")
        self.assertIsNone(macos_floor("Load command 0\n      cmd LC_SEGMENT_64\n"))


if __name__ == "__main__":
    unittest.main()
