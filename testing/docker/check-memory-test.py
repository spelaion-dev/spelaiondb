import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location(
    "memory", Path(__file__).with_name("check-memory.py")
)
memory = importlib.util.module_from_spec(spec)
spec.loader.exec_module(memory)


class MemoryBudget(unittest.TestCase):
    def test_accepts_budget_with_host_reserve(self):
        config = {"services": {"db": {"mem_limit": memory.MAX_SERVICES}}}
        self.assertEqual(
            memory.check(config, memory.MAX_SERVICES + memory.HOST_RESERVE),
            memory.MAX_SERVICES,
        )

    def test_refuses_unbounded_new_service(self):
        for limit in (None, 0, -1, "512m", True):
            with self.subTest(limit=limit), self.assertRaises(ValueError):
                memory.check(
                    {"services": {"new": {"mem_limit": limit}}}, 16 * 1024**3
                )

    def test_refuses_combined_limits_above_budget(self):
        with self.assertRaisesRegex(ValueError, "above"):
            memory.check(
                {"services": {
                    "a": {"mem_limit": memory.MAX_SERVICES},
                    "b": {"mem_limit": 1},
                }},
                16 * 1024**3,
            )

    def test_accepts_compose_decimal_byte_strings(self):
        self.assertEqual(
            memory.check(
                {"services": {"db": {"mem_limit": "536870912"}}}, 16 * 1024**3
            ),
            512 * memory.MIB,
        )

    def test_refuses_host_too_small_even_below_budget(self):
        with self.assertRaisesRegex(ValueError, "at least 4 GB"):
            memory.check(
                {"services": {"db": {"mem_limit": memory.MAX_SERVICES}}},
                8 * 1024**3,
            )

    def test_fits_the_measured_public_runner(self):
        # A real ubuntu-latest runner's free -m reported 15,700 MiB, not 16 GiB.
        config = {"services": {"db": {"mem_limit": memory.MAX_SERVICES}}}
        self.assertEqual(memory.check(config, 15700 * memory.MIB), memory.MAX_SERVICES)

    def test_refuses_one_byte_below_the_host_reserve(self):
        config = {"services": {"db": {"mem_limit": memory.MAX_SERVICES}}}
        with self.assertRaisesRegex(ValueError, "at least 4 GB"):
            memory.check(config, memory.MAX_SERVICES + memory.HOST_RESERVE - 1)

    def test_budgets_default_and_explicit_profile_phases(self):
        config = {"services": {
            "main": {"mem_limit": memory.MAX_SERVICES - 64 * memory.MIB},
            "bastion": {"mem_limit": 64 * memory.MIB},
            "hive": {"mem_limit": 1536 * memory.MIB, "profiles": ["hive"]},
        }}
        self.assertEqual(memory.check(config, 16 * 1024**3), memory.MAX_SERVICES)
        self.assertEqual(
            memory.check(config, 16 * 1024**3, ["hive", "bastion"]),
            1600 * memory.MIB,
        )
        with self.assertRaisesRegex(ValueError, "above"):
            memory.check(config, 16 * 1024**3, ["main", "bastion", "hive"])

    def test_refuses_unbounded_inactive_service(self):
        config = {"services": {
            "main": {"mem_limit": memory.MIB},
            "hive": {"profiles": ["hive"]},
        }}
        with self.assertRaisesRegex(ValueError, "positive mem_limit"):
            memory.check(config, 16 * 1024**3)

    def test_refuses_unknown_and_empty_phases(self):
        config = {"services": {"main": {"mem_limit": memory.MIB}}}
        for selected in ([], ["typo"]):
            with self.subTest(selected=selected), self.assertRaises(ValueError):
                memory.check(config, 16 * 1024**3, selected)


if __name__ == "__main__":
    unittest.main()
