"""Offline tests for the dashboard proxy's auto-tune target handling.

No sockets: urlopen is patched. Run: python -m unittest test_dashboard_autotune
"""
import json
import os
import tempfile
import unittest
from unittest import mock

import dashboard


class _Resp:
    status = 200

    def __enter__(self):
        return self

    def __exit__(self, *a):
        return False


class CleanTargetTests(unittest.TestCase):
    def test_accepts_power_and_hashrate(self):
        self.assertEqual(dashboard.clean_target({"axis": "power", "value": 1300}),
                         {"axis": "power", "value": 1300.0})
        self.assertEqual(dashboard.clean_target({"axis": "hashrate", "value": 6.0}),
                         {"axis": "hashrate", "value": 6.0})

    def test_rejects_bad_input(self):
        for bad in (None, "power", {"axis": "volts", "value": 5},
                    {"axis": "power"}, {"axis": "power", "value": 0},
                    {"axis": "power", "value": -5}, {"axis": "power", "value": "15"},
                    {"axis": "power", "value": True}, {"axis": "power", "value": float("nan")},
                    {"axis": "power", "value": float("inf")}):
            self.assertIsNone(dashboard.clean_target(bad), bad)


class PatchAutotuneTests(unittest.TestCase):
    def setUp(self):
        fd, self.path = tempfile.mkstemp(suffix=".json")
        os.close(fd)
        os.remove(self.path)
        self._orig = dashboard.AUTOTUNE_PATH
        dashboard.AUTOTUNE_PATH = self.path
        self.sent = []

        def fake_urlopen(req, timeout=None):
            self.sent.append(json.loads(req.data))
            return _Resp()

        self.patcher = mock.patch.object(dashboard.urllib.request, "urlopen", fake_urlopen)
        self.patcher.start()

    def tearDown(self):
        self.patcher.stop()
        dashboard.AUTOTUNE_PATH = self._orig
        for p in (self.path, self.path + ".tmp"):
            if os.path.exists(p):
                os.remove(p)

    def test_target_wins_over_profile_and_is_sent_clean(self):
        ok, _ = dashboard.patch_autotune(
            "b0", {"enabled": True, "profile": "balanced",
                   "target": {"axis": "power", "value": 1300, "junk": 1}})
        self.assertTrue(ok)
        self.assertEqual(self.sent[-1], {"enabled": True,
                                         "target": {"axis": "power", "value": 1300.0}})

    def test_target_implies_enabled(self):
        dashboard.patch_autotune("b0", {"target": {"axis": "hashrate", "value": 6.0}})
        self.assertTrue(self.sent[-1]["enabled"])

    def test_invalid_target_is_rejected_without_a_request(self):
        ok, detail = dashboard.patch_autotune("b0", {"enabled": True,
                                                     "target": {"axis": "power", "value": -1}})
        self.assertFalse(ok)
        self.assertIn("target", detail)
        self.assertEqual(self.sent, [])

    def test_store_keeps_profile_and_target_mutually_exclusive(self):
        dashboard.patch_autotune("b0", {"enabled": True, "profile": "balanced"})
        self.assertEqual(dashboard.load_autotune_store()["b0"]["profile"], "balanced")
        dashboard.patch_autotune("b0", {"enabled": True,
                                        "target": {"axis": "power", "value": 15}})
        store = dashboard.load_autotune_store()["b0"]
        self.assertNotIn("profile", store)
        self.assertEqual(store["target"], {"axis": "power", "value": 15.0})
        dashboard.patch_autotune("b0", {"enabled": True, "profile": "quiet"})
        store = dashboard.load_autotune_store()["b0"]
        self.assertNotIn("target", store)
        self.assertEqual(store["profile"], "quiet")


if __name__ == "__main__":
    unittest.main()
