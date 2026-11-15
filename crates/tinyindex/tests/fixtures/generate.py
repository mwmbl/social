# Regenerates the fixtures: pip install mmh3 zstandard; python3 generate.py .
import json, sys, mmh3, zstandard
out = sys.argv[1]
keys = ["", "a", "rust", "hello world", "uri:https://example.social/users/alice/statuses/1",
        "#rust", "über", "日本語", "😀 emoji", "a" * 1000, "Mixed Case", "mwmbl"]
vectors = [{"key": k, "hash": mmh3.hash(k, signed=False), "page_65536": mmh3.hash(k, signed=False) % 65536,
            "page_1000": mmh3.hash(k, signed=False) % 1000} for k in keys]
json.dump(vectors, open(f"{out}/mmh3_vectors.json", "w"), ensure_ascii=False, indent=1)
# A page as Mwmbl's Python writes it: Document.as_tuple() with trailing Nones dropped,
# json.dumps with default separators, zstd level 3, zero-padded to 4096 bytes.
docs = [
    ("Rust Programming Language", "https://www.rust-lang.org/", "A language empowering everyone", 0.1, "rust"),
    ("Title", "https://example.com/a", "Extract", 12.5, "rust", 7, [1, 2], 1700000000, 3),
    ("", "https://example.com/b", "No title", None, "rust", None, None, 1700000001),
    ("Unicode ü 日本", "https://example.com/c", "é", 1e-7, "other"),
    ("No term", "https://example.com/d", "x", 2.0),
]
data = zstandard.ZstdCompressor(level=3).compress(json.dumps(docs).encode("utf8"))
assert len(data) <= 4096
open(f"{out}/python_document_page.bin", "wb").write(data + b"\0" * (4096 - len(data)))
json.dump(docs, open(f"{out}/python_document_page.json", "w"), ensure_ascii=False, indent=1)
meta = b"mwmbl-tiny-search" + json.dumps({"version": 1, "page_size": 4096, "num_pages": 3, "item_factory": "Document"}).encode()
open(f"{out}/python_metadata.bin", "wb").write(meta + b"\0" * (4096 - len(meta)))
