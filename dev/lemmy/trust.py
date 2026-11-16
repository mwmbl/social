#!/usr/bin/env python3
"""Make the released Lemmy binary trust this environment's CA.

Lemmy 0.19 talks HTTPS through reqwest with rustls and the compiled-in Mozilla roots
(webpki-roots): it ignores SSL_CERT_FILE and the system store, so it can't be told to trust
Caddy's internal CA. Instead we give Caddy a root CA whose subject and key have exactly the
encoded lengths of one compiled-in root (Amazon Root CA 3, a P-256 root) and overwrite that
root's bytes in a copy of the binary. Nothing moves, so the binary is otherwise unchanged; the
copy trusts our CA in place of Amazon Root CA 3.

    trust.py gen-subject           print the subject to give `openssl req -subj`
    trust.py patch IN OUT ROOT.crt write the patched binary

Standard library only.
"""
import base64
import sys

# The DER contents (without the outer SEQUENCE header) of Amazon Root CA 3's subject and
# SubjectPublicKeyInfo, as webpki-roots stores them.
AMAZON_SUBJECT = bytes.fromhex(
    "310b3009060355040613025553310f300d060355040a1306416d617a6f6e3119301706035504031310"
    "416d617a6f6e20526f6f742043412033"
)
AMAZON_SPKI = bytes.fromhex(
    "301306072a8648ce3d020106082a8648ce3d030107034200042997a7c6417fc00d9be8011b56c6f252a5"
    "ba2db212e8d22ed7fac9c5d8aa6d1f73813b3b986b397c33a5c54e868e8017686245577d44581db337e5"
    "6708eb66de"
)
# A one-attribute CN subject is 11 bytes of DER around the name, so the name is 46 bytes.
CN = "Mwmbl Social dev CA for lemmy.test social.test"
assert len(AMAZON_SUBJECT) == 11 + len(CN.encode())


def tlv(data, pos):
    """Return (tag, contents start, end) of the DER element at pos."""
    tag = data[pos]
    length = data[pos + 1]
    start = pos + 2
    if length & 0x80:
        n = length & 0x7F
        length = int.from_bytes(data[start : start + n], "big")
        start += n
    return tag, start, start + length


def children(data, start, end):
    pos = start
    while pos < end:
        tag, cstart, cend = tlv(data, pos)
        yield tag, pos, cstart, cend
        pos = cend


def subject_and_spki(pem):
    body = "".join(l for l in pem.splitlines() if l and not l.startswith("-----"))
    der = base64.b64decode(body)
    _, cstart, cend = tlv(der, 0)  # Certificate
    _, tstart, tend = tlv(der, cstart)  # TBSCertificate
    fields = list(children(der, tstart, tend))
    if fields[0][0] == 0xA0:  # explicit version
        fields = fields[1:]
    # serialNumber, signature, issuer, validity, subject, subjectPublicKeyInfo
    subject, spki = fields[4], fields[5]
    return der[subject[2] : subject[3]], der[spki[2] : spki[3]]


def patch(src, dst, root):
    data = open(src, "rb").read()
    subject, spki = subject_and_spki(open(root).read())
    if len(subject) != len(AMAZON_SUBJECT) or len(spki) != len(AMAZON_SPKI):
        sys.exit(f"{root}: subject or key has the wrong length (want a P-256 key, CN={CN!r})")
    for old, new in ((AMAZON_SUBJECT, subject), (AMAZON_SPKI, spki)):
        count = data.count(old)
        if count != 1:
            sys.exit(f"{src}: expected Amazon Root CA 3 once among the compiled-in roots, found {count}")
        data = data.replace(old, new)
    open(dst, "wb").write(data)


if __name__ == "__main__":
    if sys.argv[1:] == ["gen-subject"]:
        print(f"/CN={CN}")
    elif len(sys.argv) == 5 and sys.argv[1] == "patch":
        patch(*sys.argv[2:])
    else:
        sys.exit(__doc__)
