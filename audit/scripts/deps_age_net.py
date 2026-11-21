#!/usr/bin/env python3
"""Check the age and popularity of each direct dependency on crates.io.

NETWORK: sends one GET per direct dependency to https://crates.io/api/v1/crates/<name>
(about 25 requests, one per second, User-Agent "audit-script"). Nothing else is contacted and
nothing is sent beyond the crate name. The dependency list comes from `cargo metadata
--offline`. Flags crates first published under 6 months ago or with under 100k downloads,
and locked versions released in the last 30 days.
"""
import json, subprocess, time, urllib.request
from datetime import datetime, timedelta, timezone

MIN_AGE = timedelta(days=182)
MIN_DOWNLOADS = 100_000
FRESH = timedelta(days=30)  # a locked version this new deserves a look at its diff

def direct_deps():
    meta = json.loads(subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--offline", "--locked"],
        capture_output=True, text=True, check=True).stdout)
    pkgs = {p["id"]: p for p in meta["packages"]}
    members = set(meta["workspace_members"])
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    ids = {d["pkg"] for m in members for d in nodes[m]["deps"]} - members
    return sorted((pkgs[i]["name"], pkgs[i]["version"]) for i in ids)

def fetch(name):
    req = urllib.request.Request(f"https://crates.io/api/v1/crates/{name}",
                                 headers={"User-Agent": "audit-script"})
    with urllib.request.urlopen(req, timeout=30) as resp:
        return json.load(resp)

def when(stamp):  # crates.io timestamps look like 2015-12-11T23:55:51.270207+00:00
    return datetime.fromisoformat(stamp)

def main():
    now = datetime.now(timezone.utc)
    print(f"{'crate':<22} {'locked':<18} {'first published':<16} {'this version':<13} "
          f"{'downloads':>12}  repository")
    flagged = []
    for name, version in direct_deps():
        try:
            data = fetch(name)
        except OSError as e:
            print(f"{name:<22} lookup failed: {e}")
            flagged.append(f"{name}: lookup failed")
            continue
        crate = data["crate"]
        first = when(crate["created_at"])
        this = next((v for v in data["versions"] if v["num"] == version), None)
        this_date = when(this["created_at"]).date().isoformat() if this else "NOT FOUND"
        print(f"{name:<22} {version:<18} {first.date().isoformat():<16} {this_date:<13} "
              f"{crate['downloads']:>12,}  {crate.get('repository') or '-'}")
        if now - first < MIN_AGE:
            flagged.append(f"{name}: first published {first.date()} (under 6 months ago)")
        if crate["downloads"] < MIN_DOWNLOADS:
            flagged.append(f"{name}: only {crate['downloads']:,} downloads")
        if this and now - when(this["created_at"]) < FRESH:
            flagged.append(f"{name}: locked version {version} released {this_date} (under 30 days)")
        if this is None or this.get("yanked"):
            flagged.append(f"{name}: locked version {version} missing or yanked on crates.io")
        time.sleep(1)  # about one request per second, per crates.io's crawler policy
    print(f"\n== Flagged: {len(flagged)} ==")
    print("\n".join(f"  {f}" for f in flagged) or "  none")

if __name__ == "__main__":
    main()
