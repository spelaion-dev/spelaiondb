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
        with self.assertRaisesRegex(ValueError, "less than 4 GiB"):
            memory.check(
                {"services": {"db": {"mem_limit": memory.MAX_SERVICES}}},
                8 * 1024**3,
            )


if __name__ == "__main__":
    unittest.main()
