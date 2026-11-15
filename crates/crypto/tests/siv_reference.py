"""A reference for nucleus_crypto::siv, written apart from the Rust code:
ChaCha20's block function and Poly1305 from RFC 8439's definitions
(Python integers, no limbs), checked against the RFC's own test vectors,
then the construction as `siv.rs`'s module documentation states it.

Run: python3 crates/crypto/tests/siv_reference.py
It prints the known answers that `siv.rs`'s `known_answers` test holds;
with `poly`, the Poly1305 edge cases of `aead.rs`'s `poly1305_edges`.
"""

import struct

MASK = 0xFFFFFFFF


def rotl(v, n):
    return ((v << n) & MASK) | (v >> (32 - n))


def quarter(s, a, b, c, d):
    s[a] = (s[a] + s[b]) & MASK
    s[d] = rotl(s[d] ^ s[a], 16)
    s[c] = (s[c] + s[d]) & MASK
    s[b] = rotl(s[b] ^ s[c], 12)
    s[a] = (s[a] + s[b]) & MASK
    s[d] = rotl(s[d] ^ s[a], 8)
    s[c] = (s[c] + s[d]) & MASK
    s[b] = rotl(s[b] ^ s[c], 7)


def block(key, counter, nonce):
    consts = [0x61707865, 0x3320646E, 0x79622D32, 0x6B206574]
    init = consts + list(struct.unpack("<8I", key)) + [counter] + list(struct.unpack("<3I", nonce))
    s = list(init)
    for _ in range(10):
        quarter(s, 0, 4, 8, 12)
        quarter(s, 1, 5, 9, 13)
        quarter(s, 2, 6, 10, 14)
        quarter(s, 3, 7, 11, 15)
        quarter(s, 0, 5, 10, 15)
        quarter(s, 1, 6, 11, 12)
        quarter(s, 2, 7, 8, 13)
        quarter(s, 3, 4, 9, 14)
    return struct.pack("<16I", *[(a + b) & MASK for a, b in zip(s, init)])


def chacha20(key, counter, nonce, data):
    out = bytearray()
    for i in range(0, len(data), 64):
        ks = block(key, (counter + i // 64) & MASK, nonce)
        out += bytes(a ^ b for a, b in zip(data[i : i + 64], ks))
    return bytes(out)


P1305 = (1 << 130) - 5


def poly1305_raw(r_bytes, s_bytes, msg_blocks):
    """Poly1305 over full 16-byte blocks (each gets the 2^128 bit)."""
    r = int.from_bytes(r_bytes, "little") & 0x0FFFFFFC0FFFFFFC0FFFFFFC0FFFFFFF
    s = int.from_bytes(s_bytes, "little")
    acc = 0
    for b in msg_blocks:
        n = int.from_bytes(b, "little") + (1 << (8 * len(b)))
        acc = (acc + n) * r % P1305
    return ((acc + s) & ((1 << 128) - 1)).to_bytes(16, "little")


def pad16(b):
    return b + bytes((-len(b)) % 16)


def blocks(b):
    return [b[i : i + 16] for i in range(0, len(b), 16)]


def siv_seal(key, nonce, aad, plain):
    b0 = block(key, 0, nonce)
    b1 = block(key, 1, nonce)
    enc, r, prf = b0[:32], b0[32:48], b1[:32]
    lens = struct.pack("<QQ", len(aad) * 8, len(plain) * 8)
    s = poly1305_raw(r, bytes(16), blocks(pad16(aad)) + blocks(pad16(plain)) + [lens])
    tag = block(prf, struct.unpack("<I", s[12:16])[0], s[:12])[:16]
    ct = chacha20(enc, struct.unpack("<I", tag[12:16])[0], tag[:12], plain)
    return tag, ct


def check_rfc():
    # RFC 8439 §2.3.2.
    key = bytes(range(32))
    nonce = bytes.fromhex("000000090000004a00000000")
    want = (
        "10f1e7e4d13b5915500fdd1fa32071c4c7d1f4c733c068030422aa9ac3d46c4e"
        "d2826446079faa0914c2d705d98b02a2b5129cd1de164eb9cbd083e8a2503c4e"
    )
    assert block(key, 1, nonce).hex() == want, "chacha20 block"
    # RFC 8439 §2.5.2.
    k = bytes.fromhex("85d6be7857556d337f4452fe42d506a80103808afb0db2fd4abff6af4149f51b")
    msg = b"Cryptographic Forum Research Group"
    bl = [msg[i : i + 16] for i in range(0, len(msg), 16)]
    assert poly1305_raw(k[:16], k[16:], bl).hex() == "a8061dc1305136c6c22b8baf0c0127a9", "poly1305"


def sample(n):
    return bytes((i * 31 + 7) & 0xFF for i in range(n))


def poly1305_message(key, msg):
    """Poly1305 of a raw message (RFC 8439 §2.5): a short last block
    gets its 1 byte at its end instead of the 2^128 bit."""
    r = int.from_bytes(key[:16], "little") & 0x0FFFFFFC0FFFFFFC0FFFFFFC0FFFFFFF
    s = int.from_bytes(key[16:], "little")
    acc = 0
    for i in range(0, len(msg), 16):
        b = msg[i : i + 16]
        acc = (acc + int.from_bytes(b, "little") + (1 << (8 * len(b)))) * r % P1305
    return ((acc + s) & ((1 << 128) - 1)).to_bytes(16, "little")


def poly_edges():
    """Keys and messages that drive every carry and the final reduction."""
    keys = [bytes([0xFF]) * 32, bytes([0xFF]) * 16 + bytes(16), bytes(16) + bytes([0xFF]) * 16,
            bytes([0x01]) + bytes(31)]
    msgs = [bytes([0xFF]) * n for n in (16, 17, 32, 63, 64, 160)] + [bytes(48)]
    for k in keys:
        for m in msgs:
            print(f'        ("{k.hex()}", {len(m)}, {m[0] if m else 0}, "{poly1305_message(k, m).hex()}"),')


if __name__ == "__main__":
    import sys
    check_rfc()
    if sys.argv[1:] == ["poly"]:
        poly_edges()
        sys.exit()
    key = bytes(range(32))
    nonce = bytes.fromhex("000000090000004a00000000")
    cases = [
        (b"", b""),
        (b"", sample(1)),
        (b"page 5", sample(16)),
        (bytes.fromhex("0500000000000000" "0400000000000010"), sample(100)),
        (b"nucleus", sample(130)),
    ]
    for aad, plain in cases:
        tag, ct = siv_seal(key, nonce, aad, plain)
        print(f'        ("{aad.hex()}", "{plain.hex()}", "{tag.hex()}", "{ct.hex()}"),')
