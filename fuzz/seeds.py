#!/usr/bin/env python3
"""Makes the fuzz targets' seed inputs (fuzz/README.md): fuzz/seeds/<target>/.

    fuzz/seeds.py [--server target/release/nucleus] [--models target/models]

A seed is a well-formed input the fuzzer starts from and mutates, so it
reaches deep without guessing a protocol's framing first:

- sql_parse: the statements of the sqllogictest files (crates/testkit/slt).
- json: JSON texts, a few values' binary forms, and SQL/JSON paths with
  a document.
- values, text_search, copy: each statement or type the target picks by
  its first byte, with inputs that the statement or type accepts.
- scram: a whole exchange, its proof computed here (RFC 5802).
- jwt: tokens, the claims an issuer signs, the issuer's keys.
- disk: a few patches of each kind of file.
- model_files: a small `tokenizer.json` of each kind (WordPiece; Unigram
  with its precompiled normalizer), `config.json` with the weights of a
  tiny encoder, and pooling configurations.  The tokenizers are cut from
  the models in --models (`bench/embed`'s downloads), when they are there.
- ncp, pg_wire: what real clients send — nucleus's Python driver and
  psycopg — recorded through a proxy in front of --server, when given.

The seeds are kept in the repository; run this again when a target's
statements or a protocol change.
"""

import argparse
import base64
import hashlib
import hmac
import json
import os
import re
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)


def out_dir(target):
    d = os.path.join(HERE, "seeds", target)
    if os.path.isdir(d):
        for f in os.listdir(d):
            os.remove(os.path.join(d, f))
    os.makedirs(d, exist_ok=True)
    return d


def write(d, name, data):
    if isinstance(data, str):
        data = data.encode()
    with open(os.path.join(d, name), "wb") as f:
        f.write(data)


def rust_list(target, name):
    """The string constants of `const NAME: &[&str]` in a target."""
    src = open(os.path.join(HERE, "fuzz_targets", target + ".rs")).read()
    body = re.search(r"const " + name + r": &\[&str\] = &\[(.*?)\];", src, re.S).group(1)
    items = []
    for m in re.finditer(r'"((?:[^"\\]|\\.)*)"', body):
        items.append(m.group(1))
    return items


# --- sql_parse ---------------------------------------------------------------

def slt_statements():
    out = []
    d = os.path.join(ROOT, "crates", "testkit", "slt")
    for name in sorted(os.listdir(d)):
        if not name.endswith(".slt"):
            continue
        lines = open(os.path.join(d, name), encoding="utf-8").read().split("\n")
        i = 0
        while i < len(lines):
            if lines[i].startswith(("statement", "query")):
                j = i + 1
                block = []
                while j < len(lines) and lines[j].strip() and lines[j] != "----":
                    block.append(lines[j])
                    j += 1
                if block:
                    out.append("\n".join(block))
                i = j
            else:
                i += 1
    return out


def sql_parse():
    d = out_dir("sql_parse")
    stmts = slt_statements()
    for k in range(0, len(stmts), 20):
        write(d, f"slt-{k // 20:04}", ";\n".join(stmts[k:k + 20]))
    return stmts


# --- values ------------------------------------------------------------------

VALUES = {
    "boolean": ["true", "f", "yes", "off", " 1 "],
    "smallint": ["-32768", "32767", "  42 ", "+7"],
    "integer": ["-2147483648", "2147483647", "0x1F", "1_000"],
    "bigint": ["-9223372036854775808", "9223372036854775807", "12"],
    "real": ["3.14", "-1e38", "NaN", "Infinity", "1.17549435e-38"],
    "double precision": ["2.718281828459045", "-Infinity", "1e-308", "4.9e-324"],
    "numeric": ["123.456", "-0.000001", "1e100", "NaN", "99999999999999999999.99"],
    "numeric(12, 3)": ["123456789.123", "0.0005", "-1.2345"],
    "text": ["hello", "ünïcödé ✓", ""],
    "varchar(5)": ["abcde", "abcdefgh", "ab   "],
    "char(3)": ["a", "abc", "abcd"],
    "bytea": ["\\x48656c6c6f", "plain\\\\text", "\\000\\377"],
    "date": ["2026-11-13", "1999-01-08", "infinity", "4713-01-01 BC", "20261113"],
    "time": ["04:05:06.789", "24:00:00", "23:59:60", "040506"],
    "timestamp": ["2026-11-13 12:34:56.123456", "epoch", "1999-01-08 04:05:06"],
    "timestamptz": ["2026-11-13 12:34:56+02", "2026-11-13T12:34:56Z", "now"],
    "interval": ["1 year 2 months 3 days 04:05:06", "P1Y2M3DT4H5M6S", "-1 day", "1.5 weeks",
                 "@ 1 minute ago"],
    "uuid": ["a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11", "{a0eebc999c0b4ef8bb6d6bb9bd380a11}"],
    "inet": ["192.168.0.1/24", "::1", "2001:db8::ff00:42:8329/64", "10.1.2.3"],
    "cidr": ["10.0.0.0/8", "2001:db8::/32", "192.168.1"],
    "json": ['{"a": [1, 2.5, "x", null, true]}', "[]", '"\\u00e9"'],
    "jsonb": ['{"b": 1, "a": {"c": [1, {"d": 2}]}}', "1e400"],
    "vector": ["[1, 2, 3]", "[1.5,-2e-3,  4]", "[]"],
    "vector(3)": ["[1,2,3]", "[1,2]"],
    "sparsevec": ["{1:1.5,3:2}/5", "{}/3"],
    "halfvec": ["[1, 2.5, 65504]"],
    "bit(8)": ["10101010", "1"],
    "hll": ["\\x00"],
    "ddsketch": ["\\x00"],
    "integer[]": ["{1,2,3}", "{{1,2},{3,4}}", "{NULL, 5}", "[0:2]={1,2,3}", "{}"],
    "text[]": ['{a,"b c","d\\"e",NULL}', '{"",x}'],
    "numeric[]": ["{1.5, 2.25}"],
    "double precision[][]": ["{{1,2},{3,4}}"],
    "timestamptz[]": ['{"2026-01-01 00:00:00+00"}'],
    "tsvector": ["a fat cat:3A sat:1,2 on a mat", "'quoted word':1"],
    "tsquery": ["fat & (rat | cat)", "!a <-> b:*", "'x' <2> y"],
    "regclass": ["pg_class", "nc_catalog.tables"],
}


def values():
    d = out_dir("values")
    types = rust_list("values", "TYPES")
    for i, t in enumerate(types):
        for k, v in enumerate(VALUES.get(t, ["1"])):
            write(d, f"{i:02}-{k}", bytes([i]) + v.encode())


# --- json --------------------------------------------------------------------

JSON_TEXTS = [
    '{"name": "Ann", "tags": ["a", "b"], "age": 41, "pi": 3.14159, "ok": true, "x": null}',
    '[1, -0, 1.50, 1e2, -1E-7, 123456789012345678901234567890, 9223372036854775808]',
    '{"b": 1, "a": 2, "b": 3}', '"\\u00e9\\ud83d\\ude00\\n\\t\\"\\\\"', '[[[[[]]]]]', '{}', '[]',
    '{"tool": "search", "arguments": {"query": "refund policy", "k": 5}, "result": [{"id": 1}]}',
]
# Binary forms (mode 1): null, the integer 5, the string "abc", [true], {"a": 1}.
JSON_FORMS = [b"\x00", b"\x31\x05", b"\x53abc", b"\x60\x01\x01\x20",
              b"\x70\x01\x01\x02a\x31\x01"]


# SQL/JSON paths (mode 2): a path, a NUL, a document.  The last two nest
# deeply: 900 signs, and 0 minus 64 signs of $ (whose normalized text nests
# twice as deep) — both found by the fuzzer.
JSON_PATHS = [
    ('$.a[*] ? (@ > 1 && @ < 5)', '{"a": [1, 2, 7]}'),
    ('strict $.**{1 to last}.b ? (@ starts with "x")', '{"a": {"b": "xy"}}'),
    ('$.d.datetime("YYYY-MM-DD HH24:MI").type()', '{"d": "2020-01-02 03:04"}'),
    ('$ ? (@.s like_regex "^a.c$" flag "i").keyvalue()', '{"s": "AbC"}'),
    ('-' * 900 + '1', 'null'),
    ('0 - ' + '-' * 64 + '$', '1'),
]


def json_seeds():
    d = out_dir("json")
    for k, t in enumerate(JSON_TEXTS):
        write(d, f"text-{k}", b"\x00" + t.encode())
    for k, b in enumerate(JSON_FORMS):
        write(d, f"form-{k}", b"\x01" + b)
    for k, (p, doc) in enumerate(JSON_PATHS):
        write(d, f"path-{k}", b"\x02" + p.encode() + b"\x00" + doc.encode())


# --- text_search -------------------------------------------------------------

QUERIES = ["fox", "quick AND (brown OR lazy)", '"quick brown"~2', "+fox -cat", "fo*",
           "foz~1", "NOT dog", "(a OR", "fat & (rat | cat)", "!a <-> b:*",
           "fox -dog \"lazy dog\" OR cat", "the quick brown fox jumps over the lazy dog",
           "# Title\n\nOne two three. Four five.\n\n## Sub\n\n- item one\n- item two\n\n```\ncode\n```",
           "Dr. Smith went to Washington D.C. on Jan. 5th. He said: \"Hi!\" Then left…",
           "a fat cat:3A sat:1,2 on a mat", "東京は日本の首都です。人口は多い。"]


def text_search():
    d = out_dir("text_search")
    stmts = rust_list("text_search", "STATEMENTS")
    for i in range(len(stmts)):
        for k, q in enumerate(QUERIES):
            write(d, f"{i:02}-{k:02}", bytes([i]) + q.encode())


# --- copy --------------------------------------------------------------------

def copy():
    d = out_dir("copy")
    copies = rust_list("copy", "COPIES")
    t_text = ("1\t12.50\thello\t\\\\x41\t2026-01-02\t2026-01-02 03:04:05+00\t1 day\t1.5\tt\t"
              "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11\t{1,2}\t[1,2]\t1\n"
              "2\t\\N\tescaped\\ttab\\nline\t\\N\t\\N\t\\N\t\\N\t\\N\tf\t\\N\t\\N\t\\N\t2\n\\.\n")
    t_csv = ('1,12.50,"quoted, comma",\\x41,2026-01-02,2026-01-02 03:04:05+00,1 day,1.5,t,'
             'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11,"{1,2}","[1,2]",1\n'
             '2,,"a ""quote""",,,,,,f,,,,2\n')
    t_semi = "id;n;s;b;d;ts;i;f;ok;u;a;v;r\n" + t_csv.replace(",", ";").replace(";;", ";NA;")
    data = {
        "COPY t FROM STDIN": t_text,
        "COPY t (id, n, s) FROM STDIN": "1\t2.5\tx\n2\t\\N\ty\n",
        "COPY t FROM STDIN WITH (FORMAT csv)": t_csv,
        "COPY t FROM STDIN WITH CSV HEADER DELIMITER ';' NULL 'NA'": t_semi,
        "COPY t FROM STDIN WITH (FORMAT csv, QUOTE '''', ESCAPE '\\\\')": "1,1,'it\\'s',,,,,,t,,,,1\n",
        "COPY t FROM STDIN WITH (DELIMITER '|', NULL '')": "1|1|x||||||t||||1\n",
        "COPY c FROM STDIN": "1\tone\t1.5\n2\t\\N\t-0\n",
        "COPY c FROM STDIN WITH (FORMAT csv)": '1,"one",1.5\n2,,NaN\n',
    }
    for i, c in enumerate(copies):
        write(d, f"{i:02}", bytes([i]) + data.get(c, t_text).encode())


# --- scram -------------------------------------------------------------------

def scram_exchange(password, client_nonce, server_nonce="server-nonce",
                   salt=b"salt-salt-salt-1", iterations=4096, gs2="n,,"):
    bare = f"n=,r={client_nonce}"
    first = gs2 + bare
    nonce = client_nonce + server_nonce
    server_first = f"r={nonce},s={base64.b64encode(salt).decode()},i={iterations}"
    salted = hashlib.pbkdf2_hmac("sha256", password.encode(), salt, iterations)
    ck = hmac.new(salted, b"Client Key", "sha256").digest()
    stored = hashlib.sha256(ck).digest()
    without = f"c={base64.b64encode(gs2.encode()).decode()},r={nonce}"
    auth = f"{bare},{server_first},{without}"
    sig = hmac.new(stored, auth.encode(), "sha256").digest()
    proof = bytes(a ^ b for a, b in zip(ck, sig))
    final = f"{without},p={base64.b64encode(proof).decode()}"
    sk = hmac.new(salted, b"Server Key", "sha256").digest()
    server_final = "v=" + base64.b64encode(hmac.new(sk, auth.encode(), "sha256").digest()).decode()
    return first, server_first, final, server_final


def scram():
    d = out_dir("scram")
    first, server_first, final, server_final = scram_exchange("nucleus", "client-nonce")
    write(d, "0-valid", b"\x00" + first.encode() + b"\xff" + final.encode())
    _, _, wrong, _ = scram_exchange("wrong", "client-nonce")
    write(d, "0-wrong", b"\x00" + first.encode() + b"\xff" + wrong.encode())
    write(d, "0-bound", b"\x04" + b"p=tls-server-end-point,,n=,r=abc\xffc=cD10bHMt,r=abcserver-nonce,p=AAAA")
    write(d, "0-yes", b"\x00" + b"y,,n=u,r=abc\xffc=eSws,r=abcserver-nonce,p=AAAA")
    write(d, "1-right", b"\x01" + b"nonce-123\xffnucleus")
    write(d, "1-wrong", b"\x01" + b"n\xffnucleus2")
    write(d, "2-server", b"\x02" + server_first.replace("client-nonce", "client-nonce").encode()
          + b"\xff" + server_final.encode())
    write(d, "2-error", b"\x02" + b"r=client-nonceX,s=c2FsdA==,i=1\xffe=invalid-proof")


# --- jwt ---------------------------------------------------------------------

def b64u(b):
    if isinstance(b, str):
        b = b.encode()
    return base64.urlsafe_b64encode(b).rstrip(b"=").decode()


def jwt():
    d = out_dir("jwt")
    claims = {"iss": "https://issuer.example", "aud": "nucleus", "sub": "alice",
              "exp": 4102444800, "nbf": 1700000000, "iat": 1700000000,
              "roles": ["reader"], "tenant": "acme", "nested": {"a": [1, 2]}}
    for k, (h, c) in enumerate([
        ({"alg": "EdDSA", "kid": "ed", "typ": "JWT"}, claims),
        ({"alg": "EdDSA", "kid": "ed"}, dict(claims, aud=["x", "nucleus"], exp=1)),
        ({"alg": "EdDSA"}, dict(claims, roles="reader", sub="nobody")),
        ({"alg": "RS256", "kid": "rsa"}, claims),
        ({"alg": "none"}, claims),
    ]):
        token = f"{b64u(json.dumps(h))}.{b64u(json.dumps(c))}.{b64u(bytes(64))}"
        write(d, f"0-{k}", b"\x00" + token.encode())
        write(d, f"1-{k}", b"\x01" + json.dumps(h).encode() + b"\xff" + json.dumps(c).encode())
    write(d, "0-short", b"\x00a.b")
    write(d, "2-keys", b"\x02" + json.dumps({"keys": [
        {"kty": "RSA", "kid": "rsa", "alg": "RS256", "use": "sig", "n": b64u(bytes([0xC5]) * 256),
         "e": "AQAB"},
        {"kty": "EC", "kid": "k", "crv": "P-256", "x": b64u(bytes(32)), "y": b64u(bytes(32))},
        {"kty": "OKP", "kid": "ed", "crv": "Ed25519", "x": b64u(bytes(32))},
        {"kty": "RSA", "kid": "enc", "use": "enc", "n": "AQAB", "e": "AQAB"}]}).encode())


# --- disk --------------------------------------------------------------------

def disk():
    d = out_dir("disk")
    write(d, "none", b"\x00")
    k = 0
    for f in range(4):
        for off in [0, 4, 8, 16, 24, 8192 + 24, 8192 * 3 + 40, 100000]:
            for val in [b"\x00", b"\xff\xff\xff\xff", b"\x01\x00\x00\x00\x00\x00\x00\x80"]:
                write(d, f"patch-{k:03}", b"\x00" + bytes([f]) + struct.pack("<I", off)
                      + bytes([len(val)]) + val)
                k += 1
    write(d, "unsealed", b"\x01" + bytes([0]) + struct.pack("<I", 8192 + 100) + b"\x01\x07")


# --- model_files -------------------------------------------------------------

def wordpiece(models):
    path = os.path.join(models, "bge-small-en-v1.5", "tokenizer.json")
    if not os.path.exists(path):
        return None
    t = json.load(open(path))
    words = ["the", "quick", "brown", "fox", "jump", "##s", "over", "lazy", "dog", "hello",
             "world", "!", ",", ".", "na", "##ive", "cafe", "東", "京", "##ing", "un", "##able"]
    vocab = {}
    for w in ["[PAD]", "[UNK]", "[CLS]", "[SEP]", "[MASK]"] + words:
        vocab[w] = len(vocab)
    t["model"]["vocab"] = vocab
    t["added_tokens"] = [a for a in t["added_tokens"] if a["content"] in vocab]
    for a in t["added_tokens"]:
        a["id"] = vocab[a["content"]]
    for k in ("special_tokens",):
        for name, s in t["post_processor"].get(k, {}).items():
            s["ids"] = [vocab[x] for x in s["tokens"]]
    return t


def unigram(models, precompiled):
    path = os.path.join(models, "multilingual-e5-small", "tokenizer.json")
    if not os.path.exists(path):
        return None
    t = json.load(open(path))
    keep = t["model"]["vocab"][:4] + [p for p in t["model"]["vocab"][4:300]]
    keep += [p for p in t["model"]["vocab"] if p[0] in ("▁the", "▁quick", "▁fox", "s", "▁東京")]
    t["model"]["vocab"] = keep
    ids = {p[0]: i for i, p in enumerate(keep)}
    t["added_tokens"] = [a for a in t["added_tokens"] if a["content"] in ids]
    for a in t["added_tokens"]:
        a["id"] = ids[a["content"]]
    for name, s in t["post_processor"].get("special_tokens", {}).items():
        s["ids"] = [ids[x] for x in s["tokens"]]
    if not precompiled:
        t["normalizer"] = {"type": "Sequence", "normalizers": [
            {"type": "Replace", "pattern": {"String": "  "}, "content": " "}]}
    return t


def safetensors(tensors):
    header, data = {}, b""
    for name, (shape, values) in tensors.items():
        b = struct.pack(f"<{len(values)}f", *values)
        header[name] = {"dtype": "F32", "shape": shape, "data_offsets": [len(data), len(data) + len(b)]}
        data += b
    header["__metadata__"] = {"format": "pt"}
    h = json.dumps(header).encode()
    h += b" " * (-len(h) % 8)
    return struct.pack("<Q", len(h)) + h + data


def tiny_encoder(h=4, inter=8, vocab=8, positions=8, types=2, layers=1, prefix=""):
    config = {"model_type": "bert", "hidden_act": "gelu", "hidden_size": h,
              "num_hidden_layers": layers, "num_attention_heads": 2, "intermediate_size": inter,
              "vocab_size": vocab, "max_position_embeddings": positions,
              "type_vocab_size": types, "layer_norm_eps": 1e-12}
    seq = iter(range(1 << 30))

    def t(*shape):
        n = 1
        for s in shape:
            n *= s
        return (list(shape), [((next(seq) * 37) % 17 - 8) / 16 for _ in range(n)])

    w = {"embeddings.word_embeddings.weight": t(vocab, h),
         "embeddings.position_embeddings.weight": t(positions, h),
         "embeddings.token_type_embeddings.weight": t(types, h),
         "embeddings.LayerNorm.weight": t(h), "embeddings.LayerNorm.bias": t(h)}
    for l in range(layers):
        p = f"encoder.layer.{l}."
        for part in ("query", "key", "value"):
            w[f"{p}attention.self.{part}.weight"] = t(h, h)
            w[f"{p}attention.self.{part}.bias"] = t(h)
        w[f"{p}attention.output.dense.weight"] = t(h, h)
        w[f"{p}attention.output.dense.bias"] = t(h)
        w[f"{p}attention.output.LayerNorm.weight"] = t(h)
        w[f"{p}attention.output.LayerNorm.bias"] = t(h)
        w[f"{p}intermediate.dense.weight"] = t(inter, h)
        w[f"{p}intermediate.dense.bias"] = t(inter)
        w[f"{p}output.dense.weight"] = t(h, inter)
        w[f"{p}output.dense.bias"] = t(h)
        w[f"{p}output.LayerNorm.weight"] = t(h)
        w[f"{p}output.LayerNorm.bias"] = t(h)
    return json.dumps(config).encode(), safetensors({prefix + k: v for k, v in w.items()})


def model_files(models):
    d = out_dir("model_files")
    text = "Hello, world! The quick brown fox jumps over the lazy dog. Naïve café 東京"
    for name, t in [("wordpiece", wordpiece(models)), ("unigram-plain", unigram(models, False)),
                    ("unigram", unigram(models, True))]:
        if t is not None:
            write(d, f"0-{name}", b"\x00" + json.dumps(t, ensure_ascii=False).encode()
                  + b"\xff" + text.encode())
    for k, (config, weights) in enumerate([tiny_encoder(), tiny_encoder(prefix="bert."),
                                           tiny_encoder(layers=2, types=1)]):
        write(d, f"1-{k}", bytes([1 + 4 * (k % 2)]) + config + b"\xff" + weights)
    for k, p in enumerate([{"pooling_mode_cls_token": True}, {"pooling_mode_mean_tokens": True},
                           {"pooling_mode_max_tokens": True, "word_embedding_dimension": 384}]):
        write(d, f"2-{k}", b"\x02" + json.dumps(p).encode())


# --- ncp and pg_wire, recorded -------------------------------------------------

class Recorder:
    """A proxy that keeps what the client sends."""

    def __init__(self, upstream):
        self.upstream = upstream
        self.sock = socket.socket()
        self.sock.bind(("127.0.0.1", 0))
        self.sock.listen()
        self.port = self.sock.getsockname()[1]
        self.sent = []
        threading.Thread(target=self.serve, daemon=True).start()

    def serve(self):
        while True:
            c, _ = self.sock.accept()
            u = socket.create_connection(("127.0.0.1", self.upstream))
            buf = bytearray()
            self.sent.append(buf)

            def pump(a, b, keep):
                try:
                    while True:
                        x = a.recv(65536)
                        if not x:
                            break
                        if keep is not None:
                            keep.extend(x)
                        b.sendall(x)
                except OSError:
                    pass
                finally:
                    try:
                        b.shutdown(socket.SHUT_WR)
                    except OSError:
                        pass

            threading.Thread(target=pump, args=(c, u, buf), daemon=True).start()
            threading.Thread(target=pump, args=(u, c, None), daemon=True).start()


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def wait_port(port):
    for _ in range(400):
        try:
            socket.create_connection(("127.0.0.1", port)).close()
            return
        except OSError:
            time.sleep(0.05)
    raise RuntimeError("the server did not start")


NCP_SESSIONS = [
    ["CREATE TABLE t (id int PRIMARY KEY, s text, n numeric, v vector(2))",
     ("INSERT INTO t VALUES ($1, $2, $3, $4)", (1, "a", 1.5, "[1,2]")),
     "SELECT * FROM t", ("SELECT $1::int + 1, $2::text", (41, "x")),
     "BEGIN", "UPDATE t SET s = 'b'", "ROLLBACK", "SELECT 1/0"],
    ["CREATE TABLE c (a int, b text)", ("copy", "COPY c FROM STDIN", "1\tx\n2\t\\N\n"),
     ("load", "c", {"a": [1, 2, 3], "b": ["x", None, "z"]}), "SELECT count(*) FROM c"],
    [("SELECT generate_series(1, 1000)", ()), "SELECT now(), uuid_generate_v4()",
     "SELECT ARRAY[1,2], '{\"a\": 1}'::json, interval '1 day', 1.5::real, true"],
]


def record_ncp(binary, password):
    sys.path.insert(0, os.path.join(ROOT, "drivers", "python"))
    import nucleus
    work = tempfile.mkdtemp(prefix="nucleus-fuzz-seeds-")
    port = free_port()
    args = [binary, "server", os.path.join(work, "db"), "--listen", f"127.0.0.1:{port}"]
    args += [] if password else ["--no-auth"]
    p = subprocess.Popen(args, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        wait_port(port)
        rec = Recorder(port)
        for k, session in enumerate(NCP_SESSIONS):
            if k:
                # A database of its own each: the target's is new each run.
                pass
            c = nucleus.connect("127.0.0.1", rec.port, password="nucleus" if password else None)
            for st in session:
                try:
                    if isinstance(st, str):
                        c.execute(st)
                    elif st[0] == "copy":
                        c.copy(st[1], st[2])
                    elif st[0] == "load":
                        c.load(st[1], st[2])
                    else:
                        c.execute(st[0], st[1])
                except nucleus.Error:
                    pass
            c.close()
        time.sleep(0.3)
        return [bytes(b) for b in rec.sent]
    finally:
        p.terminate()
        p.wait()
        shutil.rmtree(work)


PG_SESSIONS = [
    ["CREATE TABLE t (id int PRIMARY KEY, s text, n numeric, d date)",
     ("INSERT INTO t VALUES (%s, %s, %s, %s)", (1, "a", 1.5, "2026-01-02")),
     ("SELECT * FROM t WHERE id = %s", (1,)), ("SELECT %s::int + 1", (41,), True),
     "SELECT to_tsvector('english', 'the quick brown fox') @@ to_tsquery('fox')",
     "SELECT 1/0"],
    ["CREATE TABLE c (a int, b text)", ("copy", "COPY c FROM STDIN", "1\tx\n2\t\\N\n"),
     "SELECT count(*) FROM c", ("SELECT %s, %s", (b"\x00\x01", [1, 2]), True)],
]


def record_pg(binary, password):
    import psycopg
    work = tempfile.mkdtemp(prefix="nucleus-fuzz-seeds-")
    port, pg = free_port(), free_port()
    args = [binary, "server", os.path.join(work, "db"), "--listen", f"127.0.0.1:{port}",
            "--pg-compat", f"127.0.0.1:{pg}"]
    args += [] if password else ["--no-auth"]
    p = subprocess.Popen(args, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        wait_port(pg)
        rec = Recorder(pg)
        for session in PG_SESSIONS:
            c = psycopg.connect(host="127.0.0.1", port=rec.port, user="nucleus",
                                password="nucleus", dbname="nucleus", sslmode="disable",
                                autocommit=True)
            for st in session:
                try:
                    if isinstance(st, str):
                        c.execute(st)
                    elif st[0] == "copy":
                        with c.cursor() as cur:
                            with cur.copy(st[1]) as cp:
                                cp.write(st[2])
                    else:
                        c.execute(st[0], st[1], binary=len(st) > 2)
                except psycopg.Error:
                    pass
            c.close()
        time.sleep(0.3)
        return [bytes(b) for b in rec.sent]
    finally:
        p.terminate()
        p.wait()
        shutil.rmtree(work)


def wire(binary):
    for target, record in [("ncp", record_ncp), ("pg_wire", record_pg)]:
        d = out_dir(target)
        for mode, password in [(0, False), (1, True)]:
            try:
                sent = record(binary, password)
            except ImportError as e:
                print(f"{target}: skipped ({e})", file=sys.stderr)
                break
            for k, b in enumerate(sent):
                write(d, f"{mode}-{k}", bytes([mode]) + b)
        print(f"{target}: {len(os.listdir(d))} seeds", file=sys.stderr)


def main():
    p = argparse.ArgumentParser(description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--server", help="a nucleus binary, to record ncp and pg_wire seeds")
    p.add_argument("--models", default=os.path.join(ROOT, "target", "models"))
    a = p.parse_args()
    stmts = sql_parse()
    values()
    json_seeds()
    text_search()
    copy()
    scram()
    jwt()
    disk()
    model_files(a.models)
    if a.server:
        wire(a.server)
    print(f"sql_parse: {len(stmts)} statements", file=sys.stderr)


if __name__ == "__main__":
    main()
