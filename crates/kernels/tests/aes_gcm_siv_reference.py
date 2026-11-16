"""Known answers for nucleus_kernels::aes (AES-256-GCM-SIV, RFC 8452) from
an implementation written apart: OpenSSL's, through the Python
`cryptography` package (42 or later).

Run: python3 crates/kernels/tests/aes_gcm_siv_reference.py
It prints the `KATS` table of `aes.rs`'s `known_answers` test: data
`sample(n)` (byte i = i*31 + 7) under the key 00 01 … 1f and RFC 8439's
nonce, with and without associated data — the tag, and the whole
ciphertext or, past 64 bytes, its last 16.
"""

from cryptography.hazmat.primitives.ciphers.aead import AESGCMSIV


def sample(n):
    return bytes((i * 31 + 7) & 0xFF for i in range(n))


key = bytes(range(32))
nonce = bytes.fromhex("000000090000004a00000000")
for aad in ["", "page 5"]:
    for n in [0, 1, 15, 16, 17, 33, 64, 100, 8168]:
        out = AESGCMSIV(key).encrypt(nonce, sample(n), aad.encode() or None)
        ct, tag = out[:-16], out[-16:]
        shown = ct if n <= 64 else ct[-16:]
        print(f'        ("{aad}", {n}, "{tag.hex()}", "{shown.hex()}"),')
