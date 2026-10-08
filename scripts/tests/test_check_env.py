import os
from pathlib import Path
import stat
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from check_env import bree_environment, empty_bree_environment


class EnvironmentTests(unittest.TestCase):
    def test_explicit_fixture_replaces_inherited_store_and_terminal_settings(self):
        caller = {"BREE_DATA_DIR": "/caller/private-store", "TERM": "dumb",
                  "COLORTERM": "caller-color", "NO_COLOR": "1", "COLORFGBG": "0;15"}
        with patch.dict(os.environ, caller):
            env = bree_environment(Path("/synthetic/fixture"))
            self.assertEqual(env["BREE_DATA_DIR"], "/synthetic/fixture")
            self.assertEqual(env["TERM"], "xterm-256color")
            self.assertEqual(env["COLORTERM"], "truecolor")
            self.assertNotIn("NO_COLOR", env)
            self.assertNotIn("COLORFGBG", env)
            self.assertEqual({key: os.environ[key] for key in caller}, caller)

    def test_empty_stores_are_fresh_private_and_scoped_to_the_check(self):
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "evidence/result.json"
            with patch.dict(os.environ, {"BREE_DATA_DIR": "/caller/private-store"}):
                with empty_bree_environment(output) as first, empty_bree_environment(output) as second:
                    stores = [Path(env["BREE_DATA_DIR"]) for env in (first, second)]
                    self.assertNotEqual(*stores)
                    for store in stores:
                        self.assertTrue(store.is_absolute())
                        self.assertFalse(store.exists())
                        self.assertEqual(store.parent.parent, output.parent.resolve())
                        self.assertEqual(stat.S_IMODE(store.parent.stat().st_mode), 0o700)
                self.assertTrue(all(not store.parent.exists() for store in stores))

    def test_relative_explicit_fixture_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "absolute"):
            bree_environment(Path("relative"))


if __name__ == "__main__":
    unittest.main()
