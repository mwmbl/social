#!/usr/bin/env python3
"""Map the attack surface: HTTP routes and whether their handler checks auth, plus outbound
HTTP, subprocesses, file writes and environment reads. Read-only and offline; run from the
repo root. Only non-test code is scanned (crates/*/src, cut at the first `#[cfg(test)]`).
"""
import glob, re

# The real auth checks, found by reading the code:
#   tokens.reject(       operator.rs TokenCheck::reject: bearer token (admin and crawl APIs)
#   authenticate(        fasp/src/http/auth.rs: RFC 9421 signature of a registered server
#   app.verify(          inbox_app.rs InboxApp::verify -> apub::inbox::signer + verify_post
AUTH_CHECKS = re.compile(r"tokens\.reject\(|\bauthenticate\(|app\.verify\(|verify_post\(")
# `any_service(x)` hands every request to another router. The FASP router is
# `Router::new().fallback(dispatch)` (fasp/src/http/mod.rs), so its handler is `dispatch`.
SERVICES = {"fasp": "dispatch"}
SITES = {
    "Routers merged into the app (all share one listener)": r"\.merge\(|\.nest\(|\.fallback\(",
    "Outbound HTTP (reqwest)": r"\.send\(\)|Client::(new|builder)\(|ClientBuilder|reqwest::get",
    "Subprocesses": r"Command::new|process::Command",
    "File writes": r"fs::(write|rename|remove\w*|create_dir\w*|copy)\b|File::create|OpenOptions",
    # ENV_PREFIX: config.rs builds MWMBL_SOCIAL_<FIELD> names for every config field.
    "Environment reads": r"env::var|std::env|\benv\s*=|option_env!|\benv!\(|ENV_PREFIX",
}

def matching(text, start):  # index just past the bracket that closes text[start]
    pairs, depth = {"(": ")", "{": "}"}, 0
    for i in range(start, len(text)):
        depth += (text[i] == text[start]) - (text[i] == pairs[text[start]])
        if depth == 0:
            return i + 1
    return len(text)

sources = {}
for path in sorted(glob.glob("crates/*/src/**/*.rs", recursive=True)):
    text = open(path, encoding="utf-8").read()
    sources[path] = text.split("#[cfg(test)]")[0]
everything = "\n".join(sources.values())

def body_of(fn):  # the body of `fn <name>` anywhere in the sources, or None
    m = re.search(rf"\bfn {fn}\b[^{{;]*\{{", everything)
    return everything[m.end() - 1:matching(everything, m.end() - 1)] if m else None

print("== Routes (handler: auth? = calls one of AUTH_CHECKS) ==")
for path, text in sources.items():
    consts = dict(re.findall(r'const (\w+): &str = "([^"]*)"', text))
    for m in re.finditer(r"\.route\(", text):
        args = text[m.end() - 1:matching(text, m.end() - 1)][1:-1]
        route, _, methods = args.partition(",")
        route = route.strip().strip('"')
        route = consts.get(route, route if route.startswith("/") else f"<{route}> (dynamic)")
        line = text.count("\n", 0, m.start()) + 1
        for verb, handler in re.findall(r"\b(get|post|put|patch|delete|any|any_service)"
                                        r"\(\s*(move \|\||\|\||\w+)", methods):
            target = SERVICES.get(handler, handler) if verb == "any_service" else handler
            body = None if "|" in target else body_of(target)
            auth = "closure" if body is None and "|" in target else (
                "NOT FOUND" if body is None else ("yes" if AUTH_CHECKS.search(body) else "NO"))
            print(f"  {verb.upper():<11} {route:<40} {target:<22} auth={auth:<5} {path}:{line}")
for name, items in re.findall(r"const (\w+_ROUTES): \[&str; \d+\] = \[(.*?)\];", everything, re.S):
    patterns = ", ".join(re.findall(r'"([^"]+)"', items))
    print(f"  {name} (the dynamic patterns): {patterns}")
# Inside `dispatch`, after authenticate(), handlers::route matches (method, path segments).
handlers = sources.get("crates/fasp/src/http/handlers.rs", "")
for n, line in enumerate(handlers.splitlines(), 1):
    if re.match(r'\s*\("(GET|POST|DELETE|PUT|PATCH)[^)]*\[', line):
        print(f"    FASP dispatch arm  crates/fasp/src/http/handlers.rs:{n}: {line.strip()[:80]}")

for title, rx in SITES.items():
    print(f"\n== {title} ==")
    for path, text in sources.items():
        for n, line in enumerate(text.splitlines(), 1):
            if re.search(rx, line) and not line.strip().startswith("//"):
                print(f"  {path}:{n}: {line.strip()[:100]}")
