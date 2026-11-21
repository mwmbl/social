#!/usr/bin/env python3
"""Summarise the dependency tree from `cargo metadata --offline` (read-only, no network).

Flags: crates not from crates.io, crates with a build script, proc-macro crates, and names
within edit distance 2 of a popular crate (possible typosquats). Run from the repo root.
"""
import json, subprocess, textwrap

CRATES_IO = "registry+https://github.com/rust-lang/crates.io-index"
POPULAR = """serde serde_json serde_derive tokio reqwest hyper http axum tower tower-http clap
anyhow thiserror log tracing tracing-subscriber rand rand_core regex lazy_static once_cell
futures futures-util bytes url base64 hex sha2 sha1 md5 rsa ed25519-dalek ring rustls
rustls-pemfile webpki-roots openssl libc winapi cfg-if itertools chrono time uuid toml
rusqlite libsqlite3-sys zstd flate2 tempfile walkdir syn quote proc-macro2 memchr smallvec
parking_lot crossbeam getrandom num-traits bitflags indexmap hashbrown unicode-segmentation
httpdate mime percent-encoding idna socket2 mio""".split()

def dist(a, b):  # Levenshtein distance, plain dynamic programming
    prev = list(range(len(b) + 1))
    for i, ca in enumerate(a, 1):
        cur = [i]
        for j, cb in enumerate(b, 1):
            cur.append(min(prev[j] + 1, cur[j - 1] + 1, prev[j - 1] + (ca != cb)))
        prev = cur
    return prev[-1]

def norm(name):
    return name.replace("-", "_")

def main():
    out = subprocess.run(["cargo", "metadata", "--format-version", "1", "--offline", "--locked"],
                         capture_output=True, text=True, check=True).stdout
    meta = json.loads(out)
    pkgs = {p["id"]: p for p in meta["packages"]}
    members = set(meta["workspace_members"])
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    direct = {d["pkg"] for m in members for d in nodes[m]["deps"]} - members
    resolved = [pkgs[i] for i in nodes if i not in members]  # everything in the resolved graph
    print(f"{len(members)} workspace crates, {len(direct)} direct and {len(resolved)} total "
          "external packages (all targets/platforms in the lock file)\n")
    print("== Direct dependencies of workspace crates ==")
    for i in sorted(direct, key=lambda i: pkgs[i]["name"]):
        p = pkgs[i]
        src = "crates.io" if p["source"] == CRATES_IO else p["source"]
        print(f"  {p['name']:<24} {p['version']:<10} {src}")
    sections = {
        "Not from crates.io": [p for p in resolved if p["source"] != CRATES_IO],
        "Has a build script (build.rs runs at compile time)":
            [p for p in resolved if any("custom-build" in t["kind"] for t in p["targets"])],
        "Proc-macro crates (run code at compile time)":
            [p for p in resolved if any("proc-macro" in t["kind"] for t in p["targets"])],
    }
    # Names under 5 characters are skipped: short names are within 2 edits of each other.
    popular = {norm(n) for n in POPULAR if len(n) >= 5}
    sections["Name within edit distance 2 of a popular crate (not an exact match)"] = [
        p for p in resolved if norm(p["name"]) not in popular
        and any(dist(norm(p["name"]), n) <= 2 for n in popular)]
    for title, found in sections.items():
        print(f"\n== {title}: {len(found)} ==")
        for p in sorted(found, key=lambda p: p["name"]):
            near = [n for n in popular if norm(p["name"]) != n and dist(norm(p["name"]), n) <= 2]
            extra = f"  (near: {', '.join(sorted(near))})" if "popular" in title else ""
            print(f"  {p['name']} {p['version']}  {p['repository'] or 'no repository'}{extra}")
    print("\n== All external packages ==")
    names = " ".join(sorted(f"{p['name']}@{p['version']}" for p in resolved))
    print(textwrap.fill(names, 100, initial_indent="  ", subsequent_indent="  "))

if __name__ == "__main__":
    main()
