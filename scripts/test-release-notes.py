#!/usr/bin/env python3
"""Tests for ``scripts/release-notes.py`` (no third-party dependencies)."""

import importlib.util
import os
import re
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location(
    "release_notes", os.path.join(HERE, "release-notes.py")
)
release_notes = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release_notes)

REPO = "mttydev/mtty"
TAG = "v9.9.9"
BLOB = f"https://github.com/{REPO}/blob/{TAG}"


def write(root, name, text):
    path = os.path.join(root, "docs", "releases", name)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8") as handle:
        handle.write(text)


class ReleaseNotes(unittest.TestCase):
    def test_repository_notes_follow_the_rule(self):
        rel = os.path.join(HERE, "..", "docs", "releases")
        cjk = re.compile(r"[\u4e00-\u9fff]")
        for name in sorted(os.listdir(rel)):
            if not name.endswith(".md") or name.endswith(".zh-CN.md"):
                continue
            tag = name[:-3]
            if not os.path.exists(os.path.join(rel, tag + ".zh-CN.md")):
                continue
            with open(os.path.join(rel, name), encoding="utf-8") as handle:
                text = handle.read()
            self.assertIn(
                f"]({tag}.zh-CN.md)",
                text,
                f"{name} must link to its 简体中文 mirror",
            )
            without_link = re.sub(r"\[简体中文\]\([^)]*\)", "", text)
            self.assertIsNone(
                cjk.search(without_link),
                f"{name} must be English outside the 简体中文 link",
            )

    def test_missing_notes_returns_none(self):
        with tempfile.TemporaryDirectory() as root:
            self.assertIsNone(release_notes.body(TAG, REPO, root))

    def test_relative_links_become_absolute(self):
        with tempfile.TemporaryDirectory() as root:
            write(
                root,
                f"{TAG}.md",
                "# mtty v9.9.9\n\n"
                "See [CONFIG](../CONFIG.md), [ADR](../decisions/0001-x.md) and\n"
                "[v9.9.8](v9.9.8.md). Already absolute: "
                "[site](https://example.com/x) and [here](#section).\n",
            )
            text = release_notes.body(TAG, REPO, root)
            self.assertIn(f"[CONFIG]({BLOB}/docs/CONFIG.md)", text)
            self.assertIn(f"[ADR]({BLOB}/docs/decisions/0001-x.md)", text)
            self.assertIn(f"[v9.9.8]({BLOB}/docs/releases/v9.9.8.md)", text)
            self.assertIn("[site](https://example.com/x)", text)
            self.assertIn("[here](#section)", text)

    def test_chinese_link_is_added_after_heading(self):
        with tempfile.TemporaryDirectory() as root:
            write(root, f"{TAG}.md", "# mtty v9.9.9\n\nA change.\n")
            text = release_notes.body(TAG, REPO, root)
            lines = text.splitlines()
            self.assertEqual(lines[0], "# mtty v9.9.9")
            self.assertEqual(
                lines[1], ""
            )
            self.assertEqual(lines[2], f"[简体中文]({BLOB}/docs/releases/{TAG}.zh-CN.md)")

    def test_chinese_link_is_made_absolute(self):
        with tempfile.TemporaryDirectory() as root:
            write(
                root,
                f"{TAG}.md",
                "# mtty v9.9.9\n\n[简体中文](v9.9.9.zh-CN.md)\n\nA change.\n",
            )
            text = release_notes.body(TAG, REPO, root)
            self.assertEqual(text.count("简体中文"), 1)
            self.assertIn(f"[简体中文]({BLOB}/docs/releases/{TAG}.zh-CN.md)", text)
            self.assertNotIn("](" + TAG + ".zh-CN.md)", text)


if __name__ == "__main__":
    unittest.main()
