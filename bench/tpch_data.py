"""TPC-H data, generated (bench/tpch.py): the specification's schema, key
relationships and value distributions (TPC-H 3.0, §4.2.3), from a fixed
seed — not `dbgen`'s exact text, so results are compared between the
systems run, not with the specification's answer sets.

    python3 bench/tpch_data.py <scale factor> <directory>

writes `<table>.tbl` files, `|`-separated, one row per line.
"""

import os
import random
import sys
from datetime import date, timedelta

NATIONS = [
    ("ALGERIA", 0), ("ARGENTINA", 1), ("BRAZIL", 1), ("CANADA", 1), ("EGYPT", 4),
    ("ETHIOPIA", 0), ("FRANCE", 3), ("GERMANY", 3), ("INDIA", 2), ("INDONESIA", 2),
    ("IRAN", 4), ("IRAQ", 4), ("JAPAN", 2), ("JORDAN", 4), ("KENYA", 0),
    ("MOROCCO", 0), ("MOZAMBIQUE", 0), ("PERU", 1), ("CHINA", 2), ("ROMANIA", 3),
    ("SAUDI ARABIA", 4), ("VIETNAM", 2), ("RUSSIA", 3), ("UNITED KINGDOM", 3),
    ("UNITED STATES", 1),
]
REGIONS = ["AFRICA", "AMERICA", "ASIA", "EUROPE", "MIDDLE EAST"]
COLORS = (
    "almond antique aquamarine azure beige bisque black blanched blue blush brown burlywood "
    "burnished chartreuse chiffon chocolate coral cornflower cornsilk cream cyan dark deep dim "
    "dodger drab firebrick floral forest frosted gainsboro ghost goldenrod green grey honeydew "
    "hot indian ivory khaki lace lavender lawn lemon light lime linen magenta maroon medium "
    "metallic midnight mint misty moccasin navajo navy olive orange orchid pale papaya peach "
    "peru pink plum powder puff purple red rose rosy royal saddle salmon sandy seashell sienna "
    "sky slate smoke snow spring steel tan thistle tomato turquoise violet wheat white yellow"
).split()
TYPE1 = ["STANDARD", "SMALL", "MEDIUM", "LARGE", "ECONOMY", "PROMO"]
TYPE2 = ["ANODIZED", "BURNISHED", "PLATED", "POLISHED", "BRUSHED"]
TYPE3 = ["TIN", "NICKEL", "BRASS", "STEEL", "COPPER"]
CONT1 = ["SM", "LG", "MED", "JUMBO", "WRAP"]
CONT2 = ["CASE", "BOX", "BAG", "JAR", "PKG", "PACK", "CAN", "DRUM"]
SEGMENTS = ["AUTOMOBILE", "BUILDING", "FURNITURE", "MACHINERY", "HOUSEHOLD"]
PRIORITIES = ["1-URGENT", "2-HIGH", "3-MEDIUM", "4-NOT SPECIFIED", "5-LOW"]
INSTRUCT = ["DELIVER IN PERSON", "COLLECT COD", "NONE", "TAKE BACK RETURN"]
MODES = ["REG AIR", "AIR", "RAIL", "SHIP", "TRUCK", "MAIL", "FOB"]
WORDS = (
    "furiously quickly carefully blithely slyly ironic final regular express special pending "
    "bold even silent unusual ruthless daring fluffy idle busy close accounts deposits packages "
    "requests instructions theodolites foxes pinto beans asymptotes dependencies excuses "
    "platelets ideas dolphins frets sauternes warhorses sheaves dugouts courts somas hockey "
    "players sentiments realms dinos tithes waters about above across after against along "
    "among around at before behind beneath beside between beyond by despite during except "
    "for from in inside into near of on outside over past since through throughout to toward "
    "under until up upon with within without sleep wake are cajole haggle nag use boost affix "
    "detect integrate maintain nod was lose sublate solve thrash promise engage hinder print "
    "x-ray breach eat grow impress mold poach serve run dazzle snooze doze unwind kindle play "
    "hang believe doubt"
).split()

START = date(1992, 1, 1)
END = date(1998, 12, 31)
CURRENT = date(1995, 6, 17)


def text(rng, lo, hi):
    """A comment of `lo` to `hi` characters, of the grammar's words."""
    n = rng.randint(lo, hi)
    out = []
    size = 0
    while size < n:
        w = rng.choice(WORDS)
        out.append(w)
        size += len(w) + 1
    return " ".join(out)[:n].rstrip()


def phone(rng, nation):
    return f"{nation + 10}-{rng.randint(100, 999)}-{rng.randint(100, 999)}-{rng.randint(1000, 9999)}"


def money(rng, lo, hi):
    return f"{rng.randint(int(lo * 100), int(hi * 100)) / 100:.2f}"


def address(rng):
    chars = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 ,"
    return "".join(rng.choice(chars) for _ in range(rng.randint(10, 40))).strip() or "x"


def retail(partkey):
    return (90000 + (partkey // 10) % 20001 + 100 * (partkey % 1000)) / 100


def generate(sf, out):
    os.makedirs(out, exist_ok=True)
    rng = random.Random(19920101)
    nsupp = max(1, int(10_000 * sf))
    npart = max(1, int(200_000 * sf))
    ncust = max(1, int(150_000 * sf))
    nord = max(1, int(1_500_000 * sf))

    def write(name, rows):
        with open(os.path.join(out, f"{name}.tbl"), "w") as f:
            for r in rows:
                f.write("|".join(str(v) for v in r))
                f.write("\n")

    write("region", ((i, r, text(rng, 31, 115)) for i, r in enumerate(REGIONS)))
    write("nation", ((i, n, r, text(rng, 31, 114)) for i, (n, r) in enumerate(NATIONS)))

    def suppliers():
        for k in range(1, nsupp + 1):
            nation = rng.randint(0, 24)
            comment = text(rng, 25, 100)
            # Some suppliers have complaints, some recommendations (Q16).
            if k % 2000 == 7:
                comment = "slyly Customer final Complaints " + comment[:60]
            elif k % 2000 == 13:
                comment = "blithely Customer even Recommends " + comment[:60]
            yield (k, f"Supplier#{k:09d}", address(rng), nation, phone(rng, nation),
                   money(rng, -999.99, 9999.99), comment)

    write("supplier", suppliers())

    def parts():
        for k in range(1, npart + 1):
            m = rng.randint(1, 5)
            yield (k, " ".join(rng.sample(COLORS, 5)), f"Manufacturer#{m}",
                   f"Brand#{m}{rng.randint(1, 5)}",
                   f"{rng.choice(TYPE1)} {rng.choice(TYPE2)} {rng.choice(TYPE3)}",
                   rng.randint(1, 50), f"{rng.choice(CONT1)} {rng.choice(CONT2)}",
                   f"{retail(k):.2f}", text(rng, 5, 22))

    write("part", parts())

    def supplier_of(partkey, i):
        return (partkey + i * (nsupp // 4 + (partkey - 1) // nsupp)) % nsupp + 1

    def partsupps():
        for k in range(1, npart + 1):
            for i in range(4):
                yield (k, supplier_of(k, i), rng.randint(1, 9999), money(rng, 1.0, 1000.0),
                       text(rng, 49, 198))

    write("partsupp", partsupps())

    def customers():
        for k in range(1, ncust + 1):
            nation = rng.randint(0, 24)
            yield (k, f"Customer#{k:09d}", address(rng), nation, phone(rng, nation),
                   money(rng, -999.99, 9999.99), rng.choice(SEGMENTS), text(rng, 29, 116))

    write("customer", customers())

    last_order = END - timedelta(days=151)
    span = (last_order - START).days
    with open(os.path.join(out, "orders.tbl"), "w") as fo, \
            open(os.path.join(out, "lineitem.tbl"), "w") as fl:
        for n in range(1, nord + 1):
            # Keys sparse: the first 8 of every 32.
            key = (n - 1) // 8 * 32 + (n - 1) % 8 + 1
            cust = rng.randint(1, ncust)
            while cust % 3 == 0:  # a third of the customers never order
                cust = rng.randint(1, ncust)
            odate = START + timedelta(days=rng.randint(0, span))
            total = 0.0
            statuses = set()
            for line in range(1, rng.randint(1, 7) + 1):
                part = rng.randint(1, npart)
                supp = supplier_of(part, rng.randint(0, 3))
                qty = rng.randint(1, 50)
                price = round(qty * retail(part), 2)
                disc = rng.randint(0, 10) / 100
                tax = rng.randint(0, 8) / 100
                ship = odate + timedelta(days=rng.randint(1, 121))
                commit = odate + timedelta(days=rng.randint(30, 90))
                receipt = ship + timedelta(days=rng.randint(1, 30))
                flag = rng.choice("RA") if receipt <= CURRENT else "N"
                status = "O" if ship > CURRENT else "F"
                statuses.add(status)
                total += price * (1 + tax) * (1 - disc)
                fl.write(f"{key}|{part}|{supp}|{line}|{qty}|{price:.2f}|{disc:.2f}|{tax:.2f}|"
                         f"{flag}|{status}|{ship}|{commit}|{receipt}|{rng.choice(INSTRUCT)}|"
                         f"{rng.choice(MODES)}|{text(rng, 10, 43)}\n")
            ostatus = "F" if statuses == {"F"} else "O" if statuses == {"O"} else "P"
            comment = text(rng, 19, 78)
            if n % 97 == 5:
                comment = "carefully special pending requests " + comment[:40]
            fo.write(f"{key}|{cust}|{ostatus}|{total:.2f}|{odate}|{rng.choice(PRIORITIES)}|"
                     f"Clerk#{rng.randint(1, max(1, int(1000 * sf))):09d}|0|{comment}\n")


SCHEMA = """
CREATE TABLE region (r_regionkey INT PRIMARY KEY, r_name CHAR(25) NOT NULL, r_comment VARCHAR(152));
CREATE TABLE nation (n_nationkey INT PRIMARY KEY, n_name CHAR(25) NOT NULL, n_regionkey INT NOT NULL,
  n_comment VARCHAR(152));
CREATE TABLE supplier (s_suppkey INT PRIMARY KEY, s_name CHAR(25) NOT NULL, s_address VARCHAR(40) NOT NULL,
  s_nationkey INT NOT NULL, s_phone CHAR(15) NOT NULL, s_acctbal DECIMAL(15,2) NOT NULL,
  s_comment VARCHAR(101) NOT NULL){with_};
CREATE TABLE part (p_partkey INT PRIMARY KEY, p_name VARCHAR(55) NOT NULL, p_mfgr CHAR(25) NOT NULL,
  p_brand CHAR(10) NOT NULL, p_type VARCHAR(25) NOT NULL, p_size INT NOT NULL,
  p_container CHAR(10) NOT NULL, p_retailprice DECIMAL(15,2) NOT NULL, p_comment VARCHAR(23) NOT NULL){with_};
CREATE TABLE partsupp (ps_partkey INT NOT NULL, ps_suppkey INT NOT NULL, ps_availqty INT NOT NULL,
  ps_supplycost DECIMAL(15,2) NOT NULL, ps_comment VARCHAR(199) NOT NULL,
  PRIMARY KEY (ps_partkey, ps_suppkey)){with_};
CREATE TABLE customer (c_custkey INT PRIMARY KEY, c_name VARCHAR(25) NOT NULL, c_address VARCHAR(40) NOT NULL,
  c_nationkey INT NOT NULL, c_phone CHAR(15) NOT NULL, c_acctbal DECIMAL(15,2) NOT NULL,
  c_mktsegment CHAR(10) NOT NULL, c_comment VARCHAR(117) NOT NULL){with_};
CREATE TABLE orders (o_orderkey INT PRIMARY KEY, o_custkey INT NOT NULL, o_orderstatus CHAR(1) NOT NULL,
  o_totalprice DECIMAL(15,2) NOT NULL, o_orderdate DATE NOT NULL, o_orderpriority CHAR(15) NOT NULL,
  o_clerk CHAR(15) NOT NULL, o_shippriority INT NOT NULL, o_comment VARCHAR(79) NOT NULL){with_};
CREATE TABLE lineitem (l_orderkey INT NOT NULL, l_partkey INT NOT NULL, l_suppkey INT NOT NULL,
  l_linenumber INT NOT NULL, l_quantity DECIMAL(15,2) NOT NULL, l_extendedprice DECIMAL(15,2) NOT NULL,
  l_discount DECIMAL(15,2) NOT NULL, l_tax DECIMAL(15,2) NOT NULL, l_returnflag CHAR(1) NOT NULL,
  l_linestatus CHAR(1) NOT NULL, l_shipdate DATE NOT NULL, l_commitdate DATE NOT NULL,
  l_receiptdate DATE NOT NULL, l_shipinstruct CHAR(25) NOT NULL, l_shipmode CHAR(10) NOT NULL,
  l_comment VARCHAR(44) NOT NULL, PRIMARY KEY (l_orderkey, l_linenumber)){with_};
"""

TABLES = ["region", "nation", "supplier", "part", "partsupp", "customer", "orders", "lineitem"]


if __name__ == "__main__":
    generate(float(sys.argv[1]), sys.argv[2])
