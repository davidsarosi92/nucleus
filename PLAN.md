# nucleus — munkaterv

> **Mi ez a fájl:** a nucleus előretekintő roadmapje.  Minden milestone-nak
> van státusza, rövid tervvázlata és explicit **„kész, ha”** (Definition of
> Done) feltételei, hogy egy új session (ember vagy AI) tisztán fel tudja
> venni a fonalat.
>
> **Mi lesz a `DOCS.md`:** a *jelenlegi állapot* — ami már létezik, az API-ja
> és a furcsaságai.  Ha egy milestone kész, a tervjegyzetei átkerülnek a
> DOCS.md-be, és az itteni bejegyzés egysoros „✅ kész → DOCS §x” mutatóvá
> zsugorodik.

Státusz jelölések: ⬜ nem kezdett · ◐ folyamatban · ✅ kész · ⏸ parkolva

---

## Tartalom

| Szakasz | Cél |
|---------|-----|
| Vízió | Mivé nő a rendszer |
| Architektúra | A rétegek és a határaik |
| Alapelvek (north-star) | A szabályok, amik miatt a későbbi fázisok nem igényelnek újraírást |
| Nyitott döntések | Amit a kód első sora előtt el kell dönteni |
| Státusz-pillanatkép | Fázisok + milestone-ok táblázata |
| §F0 – §F4 | Fázisok és milestone-ok részletesen |
| Változásnapló | A terv módosításai |

---

## Vízió

A nucleus egy **rétegzett, multi-modell adatbázis**, amelynek a magja (a
„nucleus”) egy **tranzakcionális, rendezett kulcs-érték motor**.  Minden más
erre épül:

1. **SQL** — relációs modell, ami sorokat és indexeket kulcs-érték párokra képez le.
2. **NoSQL** — dokumentum (és később más) modellek *ugyanarra* a KV-magra,
   ugyanazzal a tranzakciókezeléssel.
3. **Elosztott** — a lokális KV-motort kicseréljük egy particionált,
   replikált (Raft) KV-rétegre; a fölötte lévő SQL/NoSQL réteg **nem
   változik**, csak a KV-szerződés implementációja.
4. **Fölé** — föderáció: a nucleus lekérdezőmotorja külső rendszereket
   (más adatbázisokat, fájlokat, más nucleus klasztereket) is el tud érni.

Ez a FoundationDB („rétegek egy rendezett tranzakcionális KV fölött”) és a
CockroachDB/TiDB („SQL egy elosztott KV fölött”) bevált mintája.  Az ok,
amiért *most* ehhez igazodunk: ha a KV-szerződés jól van megválasztva, a 3.
fázis egy implementációcsere, nem újraírás.

## Architektúra

```
┌──────────────────────────────────────────────────────────────────┐
│  Interfészek: beágyazott API · CLI/REPL · Postgres wire protokoll │  F1+
├──────────────────────────────────────────────────────────────────┤
│  Lekérdezés: parser → binder → logikai terv → optimalizáló →      │
│              fizikai terv → végrehajtó        (SQL + doc közös)  │  F1, F2
├────────────────────────────┬─────────────────────────────────────┤
│  Relációs modell           │  Dokumentum modell (+ később más)   │
│  katalógus, sorkódolás,    │  kollekciók, JSON-kódolás,          │
│  indexek                   │  útvonal-indexek                    │  F1, F2
├────────────────────────────┴─────────────────────────────────────┤
│  ════════ KV-SZERZŐDÉS (a „nucleus contract”) ════════            │
│  get / put / delete / range-scan · tranzakció · snapshot · ts     │  F0
├──────────────────────────────────────────────────────────────────┤
│  Lokális implementáció (F0)    │  Elosztott implementáció (F3)    │
│  MVCC · WAL · B+fa · buffer    │  range-ek · Raft · HLC ·         │
│  pool · pager                  │  elosztott tx · placement        │
├──────────────────────────────────────────────────────────────────┤
│  Platform: fájl I/O · idő · véletlen · hálózat — MIND injektálható│  F0
└──────────────────────────────────────────────────────────────────┘
```

**Rétegszabály:** egy réteg csak a közvetlenül alatta lévő réteg interfészét
látja.  Az SQL réteg soha nem nyúl a B+fához vagy a WAL-hoz.

## Alapelvek (north-star)

Ezek a szabályok drágák most és nagyon olcsók később.  Ha egy milestone
megsértené valamelyiket, előbb itt kell módosítani, indoklással.

1. **Minden a KV-szerződésen megy át.**  Az adatmodellek és a lekérdező réteg
   csak a KV API-t ismeri.  Ez teszi lehetővé, hogy a 3. fázisban a lokális
   motor helyére elosztott kerüljön.
2. **Rendezés-megőrző (memcomparable) kulcskódolás az első naptól.**  A
   kulcsok bájtonkénti összehasonlítása = logikai rendezés.  Range-scan,
   indexek, majd később a range-alapú particionálás mind erre épül.
3. **Absztrahált időbélyegek.**  A tranzakciós időbélyeg most egy logikai
   `u64` számláló, de típus mögé van zárva, hogy később HLC (hybrid logical
   clock) kerülhessen a helyére.
4. **Determinisztikus szimulációs tesztelhetőség.**  Fájl I/O, idő,
   véletlen és (később) hálózat csak injektálható interfészen keresztül
   érhető el.  Teszt alatt egy szimulált platform fut, amely hibákat
   injektál (torn write, fsync-hiba, crash bármely ponton), és egy seed-ből
   reprodukálható.  (FoundationDB / TigerBeetle stílus.)  Ez az, ami a Raftot
   és az elosztott tranzakciókat egyáltalán debugolhatóvá teszi.
5. **Helyesség a teljesítmény előtt.**  Minden tartóssággal kapcsolatos
   milestone-nak van crash-teszt és invariáns-ellenőrzés a DoD-jában.
6. **Verziózott formátumok.**  Lemezformátum, WAL-rekord, kulcskódolás és
   wire protokoll mind verziószámot hordoz; checksum minden lapon és
   WAL-rekordon.
7. **Beágyazott először, szerver másodszor.**  A mag egy könyvtár (mint a
   SQLite); a szerver és a hálózati protokoll erre épülő vékony réteg.
8. **A metaadat is adat.**  Katalógus, sémák, később a range-leírók is a
   KV-térben élnek, saját kulcs-prefix alatt — nincs külön metaadat-tároló,
   amit később replikálni kellene.

## Nyitott döntések

Ezeket a §F0/M0 előtt le kell zárni.  Mindegyiknél ott a javaslat; a végső
szó a tiéd.  A lezárt döntések ✅ jelet kapnak.

| # | Kérdés | Javaslat | Alternatívák / megjegyzés |
|---|--------|----------|---------------------------|
| D1 | Nyelv | ✅ **Rust** (eldöntve 2026-10-16) — memóriabiztonság + konkurencia (buffer pool, MVCC, Raft) | elvetve: C, Zig, Go |
| D2 | Első storage motor | **B+fa** (copy-on-write vagy in-place + WAL) | LSM-fa: írásra jobb, de compaction miatt bonyolultabb; később második motorként a KV-szerződés mögé |
| D3 | Konkurenciamodell | **Szálak + zárak** a motorban, async csak a hálózati rétegben | thread-per-core (shared-nothing) — 3. fázisban újragondolandó |
| D4 | SQL dialektus | **PostgreSQL-közeli** részhalmaz | a Postgres wire protokollal együtt ingyen kliensek (psql, driverek) |
| D5 | Izoláció | **Snapshot Isolation** először, **Serializable (SSI)** később | — |
| D6 | Fájlelrendezés | **Egy adatfájl + egy WAL-könyvtár** adatbázisonként | egyfájlos (SQLite-szerű) |
| D7 | Licenc | ✅ **MIT** (a GitHub repóval már létrejött) | — |

---

## Státusz-pillanatkép

| Fázis | Milestone | Cél | Státusz |
|-------|-----------|-----|:-------:|
| **F0 — Mag** | M0 | Projekt alap, build, teszt-harness, szimulált platform | ⬜ |
| | M1 | Pager + lemezformátum | ⬜ |
| | M2 | Buffer pool | ⬜ |
| | M3 | B+fa | ⬜ |
| | M4 | WAL + helyreállítás | ⬜ |
| | M5 | Tranzakciók + MVCC | ⬜ |
| | M6 | KV-szerződés stabilizálása + beágyazott API + CLI | ⬜ |
| **F1 — SQL** | M7 | Kulcskódolás + katalógus | ⬜ |
| | M8 | SQL parser + AST | ⬜ |
| | M9 | Binder, tervező, Volcano végrehajtó | ⬜ |
| | M10 | Join, aggregáció, másodlagos indexek | ⬜ |
| | M11 | SQL tranzakciók + kényszerek | ⬜ |
| | M12 | Szerver + Postgres wire protokoll | ⬜ |
| | M13 | Költségalapú optimalizáló | ⬜ |
| | M14 | Serializable (SSI) | ⬜ |
| **F2 — NoSQL** | M15 | Dokumentum modell | ⬜ |
| | M16 | Dokumentum lekérdezés (közös végrehajtóval) | ⬜ |
| | M17 | További modellek (döntés később) | ⬜ |
| **F3 — Elosztott** | M18 | Range-particionálás (még egy gépen) | ⬜ |
| | M19 | Raft replikáció range-enként | ⬜ |
| | M20 | Klaszter-tagság + placement | ⬜ |
| | M21 | Elosztott tranzakciók (HLC) | ⬜ |
| | M22 | Elosztott lekérdezés-végrehajtás | ⬜ |
| | M23 | Rebalancing, hibatűrés, Jepsen-stílusú tesztek | ⬜ |
| **F4 — Fölé** | M24 | Föderáció / konnektorok | ⬜ |
| | M25 | CDC / changefeed, multi-region | ⬜ |

---

## §F0 — A mag (tranzakcionális rendezett KV)

A cél egy beágyazható könyvtár, ami tartósan, crash-biztosan, tranzakciósan
tárol rendezett kulcs-érték párokat.  Ez a rendszer legfontosabb része —
minden további fázis ezt használja.

### §M0 — Projekt alap ⬜

- Cargo workspace, crate-onként egy réteg; `rustfmt` + `clippy`, GitHub Actions CI.
- Modulhatárok kijelölése az architektúra-ábra szerint (`platform`,
  `storage`, `txn`, `kv`, később `sql`, `doc`, `server`).
- **Platform-interfész** (fájl, idő, véletlen) + két implementáció: valós
  OS és szimulált (memóriában, hibainjektálással, seed-ből determinisztikus).
- Property-based teszt infrastruktúra.

**Kész, ha:** `build` + `test` zöld a CI-ban; egy példateszt a szimulált
platformon fut és seed-ből reprodukálható; DOCS.md váz létezik.

### §M1 — Pager + lemezformátum ⬜

- Fix méretű lapok (4 KiB vagy 8 KiB — döntés M1-ben), lapazonosító = offset.
- Fájlfejléc: magic, formátumverzió, lapméret, gyökér-lap mutatók.
- Minden lapon checksum + laptípus + LSN mező (WAL-hoz előkészítve).
- Szabadlap-kezelés (free list).

**Kész, ha:** lapok írhatók/olvashatók; sérült checksum detektált hibát ad;
a szimulált platformon a torn write felismerhető.

### §M2 — Buffer pool ⬜

- Fix kapacitású lapgyorsítótár, pin/unpin, dirty jelölés.
- Kilakoltatás: clock vagy LRU-K.
- Szálbiztos; lap-szintű latch-ek (olvasó/író).
- WAL-szabály előkészítése: dirty lap csak akkor írható ki, ha a hozzá
  tartozó WAL már tartós (M4-ben élesedik).

**Kész, ha:** konkurens stresszteszt adatvesztés és deadlock nélkül fut;
kilakoltatás alatt pinnelt lap soha nem kerül ki.

### §M3 — B+fa ⬜

- Változó hosszú kulcsok és értékek; túlcsorduló lapok nagy értékekhez.
- Beszúrás, törlés, pontkeresés, **range-scan iterátor** (előre/hátra).
- Lap-szétvágás és -összevonás.
- Konkurens hozzáférés: latch crabbing (vagy B-link fa).

**Kész, ha:** property-teszt egy referencia `BTreeMap`-pel szemben millió
véletlen műveleten egyezik; invariáns-ellenőrző (rendezettség, kitöltöttség,
szülő-gyerek kulcsok) minden teszt végén lefut.

### §M4 — WAL + helyreállítás ⬜

- Írás-előtti napló: LSN-ek, rekordonkénti checksum, group commit.
- Fizikai-logikai (physiological) naplózás; ARIES-szerű redo (+ undo, ha
  M5 megköveteli).
- Checkpoint, WAL-csonkolás.
- Helyreállítás indításkor.

**Kész, ha:** a szimulációs teszt **minden** I/O ponton crash-t injektál,
újraindít, és ellenőrzi: minden commitolt tranzakció megvan, semmi
nem-commitolt nem látszik, a fa invariánsai érvényesek.

### §M5 — Tranzakciók + MVCC ⬜

- Verziózott értékek: kulcs + commit-időbélyeg (az időbélyeg absztrahált
  típus — lásd alapelv 3).
- Snapshot Isolation: olvasás snapshotból, írás-írás konfliktus detektálás
  (first-committer-wins).
- Régi verziók szemétgyűjtése (GC / vacuum) a legrégebbi aktív snapshot alapján.
- Tranzakciós API: `begin` → `get/put/delete/scan` → `commit` / `abort`.

**Kész, ha:** anomália-tesztek (dirty read, lost update, non-repeatable read)
SI alatt nem fordulnak elő; write skew *dokumentáltan* előfordulhat (M14
javítja); a GC nem töröl még látható verziót.

### §M6 — KV-szerződés stabilizálása ⬜

- A KV API véglegesítése — **ez az interfész, amit az F3 elosztott
  implementációja is meg fog valósítani**, ezért itt átgondoljuk:
  range-alapú műveletek, batch-ek, snapshot-olvasás adott időbélyegen,
  hibatípusok (pl. retryable konfliktus vs. végleges hiba).
- Beágyazott könyvtár publikus API + dokumentáció.
- Kis CLI/REPL KV-parancsokhoz (`put`, `get`, `scan`, `begin`, `commit`).
- Első benchmark-alapvonal.

**Kész, ha:** a KV API egy trait/interfész mögött van, amit az F1 kód
egyetlen konkrét típus ismerete nélkül használ; DOCS.md leírja.

---

## §F1 — SQL réteg

### §M7 — Kulcskódolás + katalógus ⬜

- Memcomparable kódolás minden típusra (egész, float, string, bool, NULL,
  összetett kulcsok); DESC sorrend támogatása.
- Kulcstér-felosztás: `/<tábla_id>/<index_id>/<kulcsoszlopok>` → sor.
- Katalógus (táblák, oszlopok, típusok, indexek) **a KV-térben**, rendszer-prefix alatt.
- Sémaverziózás (online sémaváltozás csak később, de a verziómező már most).

**Kész, ha:** kódolás round-trip és rendezés-megőrzés property-tesztelt.

### §M8 — SQL parser + AST ⬜

- Kézzel írt rekurzív leszálló (vagy Pratt) parser, jó hibaüzenetekkel.
- Kezdő részhalmaz: `CREATE TABLE`, `DROP TABLE`, `INSERT`, `SELECT ... WHERE`,
  `UPDATE`, `DELETE`.

### §M9 — Binder, tervező, végrehajtó ⬜

- Binder: nevek feloldása, típusellenőrzés.
- Logikai terv → fizikai terv (egyelőre szabályalapú).
- Volcano (iterátor) végrehajtó: scan, filter, project, sort, limit.
- `EXPLAIN`.

**Kész, ha:** egy SQL logic test (sqllogictest formátum) alapcsomag zöld.

### §M10 — Join, aggregáció, indexek ⬜

- Nested loop, hash join, (merge join).
- `GROUP BY`, aggregátorok, `HAVING`, `ORDER BY`, `DISTINCT`.
- Másodlagos indexek, index-scan kiválasztása.

### §M11 — SQL tranzakciók + kényszerek ⬜

- `BEGIN` / `COMMIT` / `ROLLBACK`, autocommit.
- `PRIMARY KEY`, `UNIQUE`, `NOT NULL`, `CHECK`; `FOREIGN KEY` később.

### §M12 — Szerver + Postgres wire protokoll ⬜

- Hálózati szerver, kapcsolatkezelés, session állapot.
- PostgreSQL frontend/backend protokoll (simple + extended query).

**Kész, ha:** `psql` csatlakozik és lefut rajta az M9–M11 tesztcsomag.

### §M13 — Költségalapú optimalizáló ⬜

- Statisztikák (sorszám, hisztogram, NDV), `ANALYZE`.
- Join-sorrend (DP vagy Cascades-stílusú keresés).

### §M14 — Serializable (SSI) ⬜

- Serializable Snapshot Isolation (olvasás-írás függőségek követése).
- Write skew teszt, ami M5-ben „dokumentáltan elbukott”, itt zöld.

---

## §F2 — NoSQL réteg

Ugyanazon a KV-szerződésen és tranzakciókezelésen, mint az SQL.  A
lekérdező réteg (tervező + végrehajtó) **közös**, csak a frontend és az
adatmodell-leképezés más.

### §M15 — Dokumentum modell ⬜

- Bináris dokumentumkódolás (JSON-kompatibilis, gyors mezőelérés).
- Kollekciók a katalógusban; dokumentum-azonosító → kulcs.
- Útvonal-indexek (`a.b.c`), tömb-indexek.

### §M16 — Dokumentum lekérdezés ⬜

- Szűrő-nyelv (Mongo-szerű) **és/vagy** SQL/JSON függvények — döntés M15 végén.
- Ugyanarra a logikai tervre fordul, mint az SQL.
- Tranzakció átível SQL tábla és dokumentum-kollekció között.

### §M17 — További modellek ⏸

Döntés később: wide-column, idősor, gráf, vektor.  Mind ugyanaz a minta:
leképezés a rendezett KV-ra + operátorok a közös végrehajtóba.

---

## §F3 — Elosztott réteg

A KV-szerződés elosztott implementációja.  Az F1/F2 kód változatlan.

### §M18 — Range-particionálás (egy gépen) ⬜

- A kulcstér folytonos range-ekre bontása; range-leírók a KV-ben (meta range).
- Range-szétvágás méret/terhelés alapján, összevonás.
- Még egy folyamatban — a cél az, hogy a routing logika elkészüljön, mielőtt a hálózat bejön.

### §M19 — Raft replikáció ⬜

- Raft range-enként (leader election, log replikáció, snapshot, membership change).
- A Raft log **a determinisztikus szimulátorban** fut, hálózati partíció,
  üzenetvesztés, átrendezés injektálással.

**Kész, ha:** a szimulátor ezer seed-en linearizálhatósági ellenőrzést
(Porcupine/Knossos-szerű) futtat hiba nélkül.

### §M20 — Klaszter-tagság + placement ⬜

- Csomópont-csatlakozás/-kilépés, gossip vagy központi placement driver.
- Replika-elhelyezési szabályok.

### §M21 — Elosztott tranzakciók ⬜

- HLC időbélyegek (az alapelv-3 típus cseréje).
- Írási intent-ek + 2PC / parallel commits; tranzakciós rekord.
- Olvasási bizonytalansági ablak (uncertainty interval) kezelése.

### §M22 — Elosztott lekérdezés-végrehajtás ⬜

- Filter/projekció lenyomása a range-ekhez; elosztott join és aggregáció.

### §M23 — Hibatűrés + tesztelés ⬜

- Rebalancing, csomópont-kiesés kezelése, lassú csomópont.
- Valós hálózaton Jepsen-stílusú tesztek.

---

## §F4 — Fölé

### §M24 — Föderáció / konnektorok ⬜

- Külső adatforrások a lekérdező réteg alá (FDW-szerűen): más SQL
  adatbázisok, CSV/Parquet, másik nucleus klaszter.
- Predikátum-lenyomás a forrásokhoz.

### §M25 — CDC, multi-region ⏸

- Changefeed (a WAL / MVCC verziókból), multi-region elhelyezés, follower reads.

---

## Változásnapló

- **2026-10-16** — első terv: vízió, rétegek, 8 alapelv, 7 nyitott döntés, F0–F4 / M0–M25.
- **2026-10-16** — D1 lezárva: Rust.  D7 lezárva: MIT.  Repo: `github.com/davidsarosi92/nucleus`.
