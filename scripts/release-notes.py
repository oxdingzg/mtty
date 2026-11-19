#!/usr/bin/env python3
"""Emit the GitHub release body for a tag.

Release notes are written in English in ``docs/releases/<tag>.md`` and mirrored
in Simplified Chinese in ``docs/releases/<tag>.zh-CN.md``. A published release
body is the English file with every relative link made absolute and a
``[简体中文](…)`` link at the top, so readers of the release page can switch
language. This keeps the release body in one language instead of pasting both.

Usage::

    python3 scripts/release-notes.py <tag> [--repository OWNER/REPO]

With no ``docs/releases/<tag>.md`` the script prints nothing and exits 1, so the
caller can fall back to ``gh release create --generate-notes``.
"""

import argparse
import os
import posixpath
import re
import sys

REL = "docs/releases"

# ``[label](target)`` where the target is a relative path.
RELATIVE_LINK = re.compile(r"\]\((?P<href>[^)\s]+)\)")
ANY_SCHEME = re.compile(r"^[a-zA-Z][a-zA-Z0-9+.-]*:")
CHINESE_LINK = re.compile(r"\[[^\]]*\]\([^)]*\.zh-CN\.md\)")


def absolute_href(href: str, tag: str, repository: str) -> str:
    """Make a relative href inside a release-notes file absolute at ``tag``."""
    if href.startswith("#") or href.startswith("/") or ANY_SCHEME.match(href):
        return href
    target = posixpath.normpath(posixpath.join(REL, href))
    return f"https://github.com/{repository}/blob/{tag}/{target}"


def make_absolute(text: str, tag: str, repository: str) -> str:
    return RELATIVE_LINK.sub(
        lambda m: f"]({absolute_href(m.group('href'), tag, repository)})", text
    )


def ensure_chinese_link(text: str, tag: str, repository: str) -> str:
    link = f"https://github.com/{repository}/blob/{tag}/{REL}/{tag}.zh-CN.md"
    if CHINESE_LINK.search(text):
        return CHINESE_LINK.sub(f"[简体中文]({link})", text, count=1)
    lines = text.splitlines()
    for i, line in enumerate(lines):
        if line.startswith("# "):
            lines[i + 1 : i + 1] = ["", f"[简体中文]({link})"]
            break
    return "\n".join(lines) + ("\n" if text.endswith("\n") else "")


def body(tag: str, repository: str, root: str) -> str | None:
    path = os.path.join(root, REL, f"{tag}.md")
    if not os.path.exists(path):
        return None
    with open(path, encoding="utf-8") as handle:
        text = handle.read()
    return ensure_chinese_link(make_absolute(text, tag, repository), tag, repository)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("tag")
    parser.add_argument(
        "--repository", default=os.environ.get("GITHUB_REPOSITORY", "mttydev/mtty")
    )
    parser.add_argument("--root", default=".")
    args = parser.parse_args()
    text = body(args.tag, args.repository, args.root)
    if text is None:
        print(
            f"no {REL}/{args.tag}.md; falling back to generated notes",
            file=sys.stderr,
        )
        return 1
    sys.stdout.write(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
