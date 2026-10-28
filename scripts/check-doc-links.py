#!/usr/bin/env python3
"""Fail on a relative Markdown link to a file that is not there.

    scripts/check-doc-links.py <tree>

Run it over the tree the public mirror publishes (CI stages it first), because that is where a
link breaks: a doc that is published can point at one that is not (a private strategy doc, an
unmirrored benchmark), and the link works in the monorepo and 404s for every reader of the
mirror. External links (http, https, mailto) and pure #anchors are not checked; a `path#anchor`
link is checked for the path.
"""

import os
import re
import sys

# Links and images alike: a broken image is a broken page.
LINK = re.compile(r"!?\[[^\]]*\]\(([^)\s]+)(?:\s+\"[^\"]*\")?\)")
FENCE = re.compile(r"^\s*(```|~~~)")
SKIP_DIRS = {".git", "node_modules", "target", "dist", ".tsbuild"}


def links(path):
    in_fence = False
    with open(path, encoding="utf-8") as f:
        for n, line in enumerate(f, 1):
            if FENCE.match(line):
                in_fence = not in_fence
                continue
            if in_fence:
                continue
            # Inline code spans are not links.
            stripped = re.sub(r"`[^`]*`", "", line)
            for m in LINK.finditer(stripped):
                yield n, m.group(1)


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    root = os.path.abspath(sys.argv[1])
    broken = []
    checked = 0
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if d not in SKIP_DIRS]
        for name in filenames:
            if not name.endswith(".md"):
                continue
            doc = os.path.join(dirpath, name)
            for line, target in links(doc):
                if re.match(r"^[a-z][a-z0-9+.-]*:", target) or target.startswith("#"):
                    continue
                path = target.split("#", 1)[0].split("?", 1)[0]
                if not path:
                    continue
                checked += 1
                resolved = os.path.normpath(os.path.join(os.path.dirname(doc), path))
                if not os.path.exists(resolved):
                    broken.append((os.path.relpath(doc, root), line, target))
    for doc, line, target in broken:
        print(f"::error file={doc},line={line}::broken link {target}")
    if broken:
        sys.exit(f"{len(broken)} broken relative link(s) of {checked} checked")
    print(f"OK: {checked} relative links, all resolve.")


if __name__ == "__main__":
    main()
