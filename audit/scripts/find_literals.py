#!/usr/bin/env python3
"""List hard-coded IPs, URLs, domains, emails, long base64/hex blobs and PEM blocks.

Read-only and offline: runs `git ls-files` and reads the tracked files. Run from the repo root.
Findings in tests, fixtures, docs, dev/, Markdown, comments and `#[cfg(test)]` code go in a
separate "expected" section, so the first section is what deserves a close look.
"""
import re, subprocess, sys
from collections import defaultdict

PATTERNS = [  # (kind, regex); checked in order, and each match is blanked out of the line
    ("pem", re.compile(r"-----BEGIN [A-Z ]+-----")),
    ("url", re.compile(r"\b(?:https?|wss?|ftp)://[^\s\"'<>)\]`\\]+")),
    ("email", re.compile(r"\b[\w.+-]+@[\w-]+(?:\.[\w-]+)+\b")),
    ("ipv4", re.compile(r"\b(?:\d{1,3}\.){3}\d{1,3}\b")),
    ("ipv6", re.compile(r"[\"\[](?:[0-9a-fA-F]{0,4}:){2,7}[0-9a-fA-F]{0,4}[\"\]]")),
    ("domain", re.compile(r"\b[a-z0-9-]+(?:\.[a-z0-9-]+)*\.(?:com|org|net|io|social|dev|app|xyz"
                          r"|me|co|cloud|online|site|top|ru|cn|onion|zone|network|sh|ly|gg|tk)\b")),
    ("hex", re.compile(r"\b[0-9a-fA-F]{32,}\b")),
    ("base64", re.compile(r"[A-Za-z0-9+/]{40,}={0,2}")),
]
EXPECTED_PATH = re.compile(r"(^|/)(tests?|fixtures|dev|docs)/|\.md$|^LICENSE$")
COMMENT = re.compile(r"^\s*(//|#(?!\[)|\*|/\*)")  # Rust/shell/TOML comment lines

def looks_random(s):  # base64 heuristic: real blobs mix digits, upper and lower case
    return all(re.search(c, s) for c in ("[0-9]", "[A-Z]", "[a-z]"))

def main():
    files = subprocess.run(["git", "ls-files"], capture_output=True, text=True, check=True).stdout.split()
    hits = {True: defaultdict(list), False: defaultdict(list)}  # expected? -> file -> lines
    for path in files:
        if path == "Cargo.lock":
            continue
        try:
            lines = open(path, encoding="utf-8").read().splitlines()
        except (UnicodeDecodeError, OSError):
            continue  # binary fixture or unreadable: skip
        in_test = False  # approximation: test modules sit at the end of a .rs file
        for n, line in enumerate(lines, 1):
            in_test = in_test or (path.endswith(".rs") and "#[cfg(test)]" in line)
            expected = bool(EXPECTED_PATH.search(path)) or in_test or bool(COMMENT.match(line))
            rest = line
            for kind, rx in PATTERNS:
                for m in rx.findall(rest):
                    if kind == "base64" and not looks_random(m):
                        continue
                    if kind == "domain" and m.split(".")[0] in ("self", "profile"):
                        continue  # Rust field access and Cargo profile names, not domains
                    hits[expected][path].append(f"  {n:>5}  {kind:<7} {m[:90]}")
                rest = rx.sub(" ", rest)
    print(f"=== UNEXPECTED (production code and config): {sum(map(len, hits[False].values()))} hits ===")
    for path, found in sorted(hits[False].items()):
        print(path + "\n" + "\n".join(found))
    # Expected hits are summarised per file (counts by kind); PEM blocks are still listed.
    print(f"\n=== EXPECTED (tests, fixtures, docs, dev/, comments): per-file counts ===")
    for path, found in sorted(hits[True].items()):
        kinds = defaultdict(int)
        for f in found:
            kinds[f.split()[1]] += 1
        print(f"{path}: " + ", ".join(f"{k}={v}" for k, v in sorted(kinds.items())))
        for f in found:
            if f.split()[1] == "pem":
                print(f)

if __name__ == "__main__":
    sys.exit(main())
