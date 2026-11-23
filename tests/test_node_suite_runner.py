"""Stdlib-only regression tests for the standalone Node suite harness."""

import runpy
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
RUNNER = runpy.run_path(str(ROOT / "tools" / "node-suite" / "runner.py"))
classify = RUNNER["classify"]
skip_reason = RUNNER["skip_reason"]


class NodeSuiteVerdictTests(unittest.TestCase):
    def test_verdicts(self):
        cases = (
            ("plain output", "done", 0, False, "PASS"),
            ("timeout first", "1..0 # Skipped: unavailable\nnot ok 1 - boom", 0, True, "TIMEOUT"),
            ("skip plan", "1..0 # Skipped: unavailable", 0, False, "SKIP"),
            ("skip plan with failed exit", "1..0 # Skipped: unavailable", 7, False, "ERROR"),
            ("all points skipped", "ok 1 - disabled # SKIP\nnot ok 2 # skipped: unavailable\n1..2", 0, False, "SKIP"),
            ("all skipped with failed exit", "ok 1 - disabled # SKIP\n1..1", 7, False, "ERROR"),
            ("mixed successful and skipped", "ok 1 # SKIP\nok 2 - ran\n1..2", 0, False, "PASS"),
            ("skip beside failed point", "ok 1 # SKIP\nnot ok 2 - boom\n1..2", 0, False, "ERROR"),
            ("nested failed point", "TAP version 13\n    not ok 1 - boom\nok 1 - parent\n1..1", 0, False, "ERROR"),
            ("legacy plan beside failed point", "1..0 # Skipped: unavailable\nnot ok 1 - boom", 0, False, "ERROR"),
            ("TODO alone", "not ok 1 - known issue # todo: later", 0, False, "PASS"),
            ("TODO counts as non-skipped", "ok 1 # SKIP\nnot ok 2 # TODO", 0, False, "PASS"),
            ("TODO does not hide exit failure", "not ok 1 # TODO", 1, False, "ERROR"),
            ("unadorned zero plan", "TAP version 13\n1..0", 0, False, "PASS"),
            ("escaped directive in name", r"not ok 1 - has \# SKIP in its name", 0, False, "ERROR"),
            ("incidental prose and comments", "log: not ok 1\n# not ok 2\nnot ok really\n# skipped 3", 0, False, "PASS"),
            ("ANSI and CRLF", "\x1b[31m    not ok 1 - boom\x1b[0m\r\n1..1\r\n", 0, False, "ERROR"),
            ("non-TAP assertion", "1..0 # Skipped: unavailable\nAssertionError: x", 1, False, "ASSERT"),
            ("TAP assertion", "not ok 1\n  ---\n  code: 'ERR_ASSERTION'\n  ...", 0, False, "ASSERT"),
            ("TAP unsupported", "not ok 1\n  ---\n  code: 'ERR_NOT_SUPPORTED'\n  ...", 0, False, "UNSUPPORTED"),
            ("legacy unsupported", "Error: no test-runner host", 1, False, "UNSUPPORTED"),
            ("no exit status", "1..0 # Skipped: unavailable", None, False, "ERROR"),
        )
        for name, output, exit_code, timed_out, expected in cases:
            with self.subTest(name=name):
                self.assertEqual(classify(output, exit_code, timed_out), expected)

    def test_successful_suite_wrapper_does_not_hide_all_skipped_children(self):
        output = """TAP version 13
# Subtest: suite
    ok 1 - disabled # SKIP
    ok 2 - also disabled # SKIP
    1..2
ok 1 - suite
  ---
  type: 'suite'
  ...
1..1
"""
        self.assertEqual(classify(output, 0, False), "SKIP")
        self.assertEqual(classify(output.replace("also disabled # SKIP", "ran"), 0, False), "PASS")
        self.assertEqual(classify(output, 1, False), "ERROR")

    def test_yaml_diagnostics_are_not_result_points(self):
        todo = """TAP version 13
not ok 1 - known issue # TODO
  ---
  error: |-
    not ok 1 - only part of the message
  code: 'ERR_NOT_SUPPORTED'
  ...
"""
        self.assertEqual(classify(todo, 0, False), "PASS")
        self.assertEqual(classify(todo + "not ok 2 - real failure\n", 0, False), "ERROR")

    def test_skip_reason_comes_from_a_skip_point_or_plan(self):
        self.assertEqual(skip_reason("# SKIP unrelated\n\x1b[33mok 1 # SKIP: unavailable\x1b[0m\r\n"), "unavailable")
        self.assertEqual(skip_reason("1..0 # Skipped: disabled"), "disabled")
        self.assertEqual(skip_reason("ok 1 # SKIP\nok 2 - next result"), "")

    def test_shim_modes_follow_both_switches(self):
        shim_mode = RUNNER["shim_mode"]
        for bytecode, source, expected in (
            ("0", "1", ("source (PIR_EXT_SHIM_SRC)", True)),
            ("false", "off", ("embedded source", False)),
            ("1", "0", ("embedded bytecode", False)),
            ("1", "OFF", ("embedded bytecode with source override on fallback", True)),
        ):
            with self.subTest(bytecode=bytecode, source=source):
                self.assertEqual(shim_mode({"PIR_EXT_BYTECODE": bytecode, "PIR_EXT_SHIM_SRC": source}), expected)

    def test_fingerprint_comes_from_the_supplied_checkout(self):
        metadata = RUNNER["shim_tree_metadata"]
        with tempfile.TemporaryDirectory() as tmp:
            checkout = Path(tmp).resolve()
            shims = checkout / "src" / "extensions" / "api" / "js"
            shims.mkdir(parents=True)
            self.assertIsNone(metadata(checkout))
            shim = shims / "test_shim.js.txt"
            shim.write_text("before", encoding="utf-8")
            before = metadata(checkout)
            self.assertEqual(before["dir"], str(shims))
            self.assertEqual(before["files"], 1)
            self.assertEqual(before, metadata(checkout))
            shim.write_text("after", encoding="utf-8")
            self.assertNotEqual(before["sha256"], metadata(checkout)["sha256"])

    def test_package_marker_is_only_written_to_the_prepared_work_root(self):
        prepare = RUNNER["prepare_work_root"]
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            corpus = root / "corpus"
            for sub in ("common", "fixtures", "parallel"):
                (corpus / "test" / sub).mkdir(parents=True)
            common = corpus / "test" / "common" / "index.js"
            source = "const process = globalThis.process;\n"
            common.write_text(source, encoding="utf-8")
            work = root / "work"
            prepare(corpus, work, RUNNER["DEFAULT_PATCH"], skip_copy=False)
            marker = work / "test" / "package.json"
            self.assertEqual(marker.read_text(encoding="utf-8"), '{"type":"commonjs"}\n')
            self.assertEqual(common.read_text(encoding="utf-8"), source)
            self.assertFalse((corpus / "test" / "package.json").exists())
            marker.unlink()
            prepare(corpus, work, RUNNER["DEFAULT_PATCH"], skip_copy=True)
            self.assertTrue(marker.is_file())


if __name__ == "__main__":
    unittest.main()
