#!/usr/bin/env python3
"""Who changed the security-critical files, and which tracked files nothing refers to.

Read-only and offline: runs `git log`, `git ls-files` and `cargo metadata --offline`, and reads
tracked files. Run from the repo root.
"""
import json, os, re, subprocess
from collections import defaultdict

CRITICAL = re.compile(r"signatures/|inbox|/net\.rs$|/fetch\.rs$|crates/fasp/|fasp_app|registration"
                      r"|admin|consent|decide|sweep|writer|follows|keys|config|operator\.rs$"
                      r"|Dockerfile|\.dockerignore$|\.github/workflows/|Cargo\.toml$")
# Files tools find by convention, so no other file needs to name them.
CONVENTIONAL = {"LICENSE", ".gitignore", ".dockerignore", "Cargo.lock", "Cargo.toml", "clippy.toml",
                "rustfmt.toml", "rust-toolchain.toml", "README.md", "CLAUDE.md", "Dockerfile"}

def run(*cmd):
    return subprocess.run(cmd, capture_output=True, text=True, check=True).stdout

files = run("git", "ls-files").split()
print("== Commit authors (name <email>: commits) ==")
print(run("git", "shortlog", "-sne", "HEAD").rstrip())

# One log pass: a header line per commit, then the files it touched.
by_file, commit = defaultdict(list), None
for line in run("git", "log", "--format=@%h %an %ad %s", "--date=short", "--name-only").splitlines():
    if line.startswith("@"):
        commit = line[1:]
    elif line and CRITICAL.search(line) and "/tests/" not in line:
        by_file[line].append(commit)
print("\n== Commits touching security-critical files (newest first) ==")
for path in sorted(by_file):
    print(f"{path}  ({len(by_file[path])} commits){'' if path in files else '  [deleted]'}")
    for c in by_file[path]:
        print(f"    {c[:110]}")

# A .rs file is used if it is a Cargo target, a `mod` of some file, or named in #[path].
meta = json.loads(run("cargo", "metadata", "--format-version", "1", "--offline", "--no-deps"))
targets = {os.path.relpath(t["src_path"]) for p in meta["packages"] for t in p["targets"]}
texts = {}
for f in files:
    try:
        texts[f] = open(f, encoding="utf-8").read()
    except (UnicodeDecodeError, OSError):
        texts[f] = ""  # binary fixture
rust = "\n".join(t for f, t in texts.items() if f.endswith(".rs"))
mods = set(re.findall(r"\bmod\s+(\w+)\s*;", rust)) | set(re.findall(r'#\[path\s*=\s*"([^"]+)"', rust))
print("\n== Tracked .rs files nothing references ==")
for f in files:
    if not f.endswith(".rs") or f in targets or f.endswith("build.rs"):
        continue
    stem = os.path.basename(os.path.dirname(f)) if f.endswith("/mod.rs") else os.path.basename(f)[:-3]
    if stem not in mods and not any(f.endswith(p) for p in mods):
        print(f"  {f}")

print("\n== Other tracked files whose name appears in no other tracked file ==")
for f in files:
    name = os.path.basename(f)
    if f.endswith(".rs") or name in CONVENTIONAL:
        continue
    quoted_stem = f'"{os.path.splitext(name)[0]}"'  # e.g. load("mastodon_note") in tests
    if not any(name in t or quoted_stem in t for other, t in texts.items() if other != f):
        print(f"  {f}")
