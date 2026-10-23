//! nucleus's cryptographic primitives, without dependencies: SHA-256
//! (FIPS 180-4), HMAC-SHA-256 (RFC 2104), PBKDF2-HMAC-SHA-256 (RFC 8018),
//! MD5 (RFC 1321, for `md5()` only — not for security), base64 (RFC
//! 4648), a constant-time comparison, and SCRAM-SHA-256 verifiers (RFC
//! 5802, RFC 7677) in PostgreSQL's text form.

/// SHA-256 (FIPS 180-4).
pub fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    for block in padded(data, true).chunks(64) {
        let mut w = [0u32; 64];
        for (i, word) in block.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes(word.try_into().expect("4 bytes"));
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0u8; 32];
    for (o, x) in out.chunks_mut(4).zip(h) {
        o.copy_from_slice(&x.to_be_bytes());
    }
    out
}

/// MD5 (RFC 1321) — for `md5()`, not for security.
pub fn md5(data: &[u8]) -> [u8; 16] {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    let k: Vec<u32> = (0..64)
        .map(|i| ((i as f64 + 1.0).sin().abs() * 4_294_967_296.0) as u32)
        .collect();
    let mut h: [u32; 4] = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476];
    for block in padded(data, false).chunks(64) {
        let m: Vec<u32> = block
            .chunks(4)
            .map(|w| u32::from_le_bytes(w.try_into().expect("4 bytes")))
            .collect();
        let [mut a, mut b, mut c, mut d] = h;
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f = f.wrapping_add(a).wrapping_add(k[i]).wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(S[i]));
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d]) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0u8; 16];
    for (o, x) in out.chunks_mut(4).zip(h) {
        o.copy_from_slice(&x.to_le_bytes());
    }
    out
}

/// HMAC-SHA-256 of `data` under `key`.
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&sha256(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Vec::with_capacity(64 + data.len());
    inner.extend(k.iter().map(|b| b ^ 0x36));
    inner.extend_from_slice(data);
    let mut outer = Vec::with_capacity(96);
    outer.extend(k.iter().map(|b| b ^ 0x5c));
    outer.extend_from_slice(&sha256(&inner));
    sha256(&outer)
}

/// PBKDF2-HMAC-SHA-256: a 32-byte key from `password` and `salt`.
pub fn pbkdf2_sha256(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
    let mut first = salt.to_vec();
    first.extend_from_slice(&1u32.to_be_bytes());
    let mut u = hmac_sha256(password, &first);
    let mut out = u;
    for _ in 1..iterations {
        u = hmac_sha256(password, &u);
        for (o, x) in out.iter_mut().zip(u) {
            *o ^= x;
        }
    }
    out
}

/// Whether `a` and `b` are equal, in time independent of where they differ.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Base64 with padding, on one line.
pub fn base64_encode(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len().div_ceil(3) * 4);
    for chunk in b.chunks(3) {
        let n = chunk.len();
        let v = u32::from(chunk[0]) << 16
            | u32::from(*chunk.get(1).unwrap_or(&0)) << 8
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for k in 0..4 {
            s.push(if k <= n {
                BASE64[(v >> (18 - 6 * k) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    s
}

/// Decodes [`base64_encode`]'s form (padding required; no whitespace).
pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    for chunk in s.as_bytes().chunks(4) {
        let pad = chunk.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 {
            return None;
        }
        let mut v = 0u32;
        for &c in &chunk[..4 - pad] {
            v = v << 6 | BASE64.iter().position(|&x| x == c)? as u32;
        }
        v <<= 6 * pad as u32;
        out.extend_from_slice(&v.to_be_bytes()[1..4 - pad]);
    }
    Some(out)
}

/// A SCRAM-SHA-256 verifier: what a server keeps instead of a password
/// (PostgreSQL's text form, `SCRAM-SHA-256$<iterations>:<salt>$<StoredKey>:<ServerKey>`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScramVerifier {
    /// PBKDF2 iterations.
    pub iterations: u32,
    /// The salt.
    pub salt: Vec<u8>,
    /// `H(HMAC(SaltedPassword, "Client Key"))`.
    pub stored_key: [u8; 32],
    /// `HMAC(SaltedPassword, "Server Key")`.
    pub server_key: [u8; 32],
}

/// The iteration count new verifiers get (PostgreSQL's default).
pub const SCRAM_ITERATIONS: u32 = 4096;

impl ScramVerifier {
    /// The verifier of `password` with `salt`.
    pub fn new(password: &str, salt: &[u8], iterations: u32) -> Self {
        let salted = pbkdf2_sha256(password.as_bytes(), salt, iterations);
        let client_key = hmac_sha256(&salted, b"Client Key");
        ScramVerifier {
            iterations,
            salt: salt.to_vec(),
            stored_key: sha256(&client_key),
            server_key: hmac_sha256(&salted, b"Server Key"),
        }
    }

    /// Whether `password` is the one it was made from.
    pub fn matches(&self, password: &str) -> bool {
        let other = ScramVerifier::new(password, &self.salt, self.iterations);
        constant_time_eq(&other.stored_key, &self.stored_key)
            && constant_time_eq(&other.server_key, &self.server_key)
    }

    /// The text form.
    pub fn to_text(&self) -> String {
        format!(
            "SCRAM-SHA-256${}:{}${}:{}",
            self.iterations,
            base64_encode(&self.salt),
            base64_encode(&self.stored_key),
            base64_encode(&self.server_key)
        )
    }

    /// Parses the text form.
    pub fn parse(s: &str) -> Option<Self> {
        let rest = s.strip_prefix("SCRAM-SHA-256$")?;
        let (head, keys) = rest.split_once('$')?;
        let (iterations, salt) = head.split_once(':')?;
        let (stored, server) = keys.split_once(':')?;
        Some(ScramVerifier {
            iterations: iterations.parse().ok().filter(|&i| i > 0)?,
            salt: base64_decode(salt)?,
            stored_key: base64_decode(stored)?.try_into().ok()?,
            server_key: base64_decode(server)?.try_into().ok()?,
        })
    }
}

/// `data`, a `1` bit, zeros, and its bit length (big- or little-endian),
/// to a multiple of 64 bytes.
fn padded(data: &[u8], big_endian: bool) -> Vec<u8> {
    let mut m = data.to_vec();
    m.push(0x80);
    while m.len() % 64 != 56 {
        m.push(0);
    }
    let bits = (data.len() as u64).wrapping_mul(8);
    m.extend_from_slice(&if big_endian {
        bits.to_be_bytes()
    } else {
        bits.to_le_bytes()
    });
    m
}

// ------------------------------------------------------- SCRAM exchange

/// A SCRAM-SHA-256 exchange, the server's side (RFC 5802): it never sees
/// the password, only the client's proof that it knows it.
#[derive(Debug)]
pub struct ScramServer {
    verifier: ScramVerifier,
    /// Whether the verifier is a stand-in (an unknown user): the exchange
    /// runs to its end and fails, so a client cannot tell names apart.
    fake: bool,
    server_nonce: String,
    client_first_bare: String,
    server_first: String,
    nonce: String,
}

/// Why an exchange failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScramError {
    /// A message is not what SCRAM-SHA-256 sends.
    Malformed,
    /// The proof is wrong (or the user unknown).
    Rejected,
}

fn attr(msg: &str, key: char) -> Option<&str> {
    msg.split(',')
        .find_map(|p| p.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
}

impl ScramServer {
    /// An exchange against `verifier` (`None`: an unknown user, whose
    /// stand-in salt `fake_salt` should be stable per name), with
    /// `server_nonce` (random, printable, no commas).
    pub fn new(verifier: Option<ScramVerifier>, fake_salt: &[u8], server_nonce: &str) -> Self {
        let fake = verifier.is_none();
        ScramServer {
            verifier: verifier.unwrap_or_else(|| ScramVerifier {
                iterations: SCRAM_ITERATIONS,
                salt: fake_salt.to_vec(),
                stored_key: [0; 32],
                server_key: [0; 32],
            }),
            fake,
            server_nonce: server_nonce.to_string(),
            client_first_bare: String::new(),
            server_first: String::new(),
            nonce: String::new(),
        }
    }

    /// The client's first message (`n,,n=…,r=…`) → the server's first.
    pub fn first(&mut self, client_first: &str) -> Result<String, ScramError> {
        // Channel binding is not offered: the GS2 header must be "n,,".
        let bare = client_first
            .strip_prefix("n,,")
            .ok_or(ScramError::Malformed)?;
        let client_nonce = attr(bare, 'r').ok_or(ScramError::Malformed)?;
        if client_nonce.is_empty() {
            return Err(ScramError::Malformed);
        }
        self.nonce = format!("{client_nonce}{}", self.server_nonce);
        self.client_first_bare = bare.to_string();
        self.server_first = format!(
            "r={},s={},i={}",
            self.nonce,
            base64_encode(&self.verifier.salt),
            self.verifier.iterations
        );
        Ok(self.server_first.clone())
    }

    /// The client's final message (`c=biws,r=…,p=…`) → the server's final
    /// (`v=…`, its signature), if the proof holds.
    pub fn finish(&self, client_final: &str) -> Result<String, ScramError> {
        let (without_proof, proof) = client_final
            .rsplit_once(",p=")
            .ok_or(ScramError::Malformed)?;
        if attr(without_proof, 'c') != Some("biws") || attr(without_proof, 'r') != Some(&self.nonce)
        {
            return Err(ScramError::Malformed);
        }
        let proof = base64_decode(proof).ok_or(ScramError::Malformed)?;
        if proof.len() != 32 {
            return Err(ScramError::Malformed);
        }
        let auth = format!(
            "{},{},{without_proof}",
            self.client_first_bare, self.server_first
        );
        let signature = hmac_sha256(&self.verifier.stored_key, auth.as_bytes());
        let client_key: Vec<u8> = proof.iter().zip(signature).map(|(a, b)| a ^ b).collect();
        if self.fake || !constant_time_eq(&sha256(&client_key), &self.verifier.stored_key) {
            return Err(ScramError::Rejected);
        }
        let server_signature = hmac_sha256(&self.verifier.server_key, auth.as_bytes());
        Ok(format!("v={}", base64_encode(&server_signature)))
    }
}

/// A SCRAM-SHA-256 exchange, the client's side.
#[derive(Debug)]
pub struct ScramClient {
    password: String,
    client_first_bare: String,
    auth: String,
    server_key: [u8; 32],
}

impl ScramClient {
    /// An exchange for `password`, with `client_nonce` (random, printable,
    /// no commas).
    pub fn new(password: &str, client_nonce: &str) -> Self {
        ScramClient {
            password: password.to_string(),
            client_first_bare: format!("n=,r={client_nonce}"),
            auth: String::new(),
            server_key: [0; 32],
        }
    }

    /// The first message.
    pub fn first(&self) -> String {
        format!("n,,{}", self.client_first_bare)
    }

    /// The server's first message → the client's final one.
    pub fn respond(&mut self, server_first: &str) -> Result<String, ScramError> {
        let nonce = attr(server_first, 'r').ok_or(ScramError::Malformed)?;
        let own = attr(&self.client_first_bare, 'r').unwrap_or_default();
        if !nonce.starts_with(own) || nonce.len() == own.len() {
            return Err(ScramError::Malformed);
        }
        let salt = attr(server_first, 's')
            .and_then(base64_decode)
            .ok_or(ScramError::Malformed)?;
        let iterations: u32 = attr(server_first, 'i')
            .and_then(|i| i.parse().ok())
            .filter(|&i| i > 0)
            .ok_or(ScramError::Malformed)?;
        let salted = pbkdf2_sha256(self.password.as_bytes(), &salt, iterations);
        let client_key = hmac_sha256(&salted, b"Client Key");
        let stored_key = sha256(&client_key);
        self.server_key = hmac_sha256(&salted, b"Server Key");
        let without_proof = format!("c=biws,r={nonce}");
        self.auth = format!("{},{server_first},{without_proof}", self.client_first_bare);
        let signature = hmac_sha256(&stored_key, self.auth.as_bytes());
        let proof: Vec<u8> = client_key
            .iter()
            .zip(signature)
            .map(|(a, b)| a ^ b)
            .collect();
        Ok(format!("{without_proof},p={}", base64_encode(&proof)))
    }

    /// Checks the server's final message: the server knew the verifier too.
    pub fn verify(&self, server_final: &str) -> Result<(), ScramError> {
        let v = attr(server_final, 'v')
            .and_then(base64_decode)
            .ok_or(ScramError::Malformed)?;
        let want = hmac_sha256(&self.server_key, self.auth.as_bytes());
        if constant_time_eq(&v, &want) {
            Ok(())
        } else {
            Err(ScramError::Rejected)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn digests() {
        // FIPS 180-4 and RFC 1321 test vectors.
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&sha256(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        assert_eq!(hex(&md5(b"")), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(hex(&md5(b"abc")), "900150983cd24fb0d6963f7d28e17f72");
    }

    #[test]
    fn hmac_and_pbkdf2() {
        // RFC 4231 test case 2; RFC 7914 §11's PBKDF2-HMAC-SHA-256 vector.
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert_eq!(
            hex(&pbkdf2_sha256(b"passwd", b"salt", 1))[..64],
            *"55ac046e56e3089fec1691c22544b605f94185216dde0465e68b9d57c20dacbc"
        );
    }

    #[test]
    fn base64_round_trips() {
        for b in [&b""[..], b"f", b"fo", b"foo", b"foob", b"\0\xff"] {
            assert_eq!(base64_decode(&base64_encode(b)).unwrap(), b);
        }
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert!(base64_decode("Zm9").is_none());
    }

    #[test]
    fn scram_exchanges() {
        let v = ScramVerifier::new("pencil", b"salt-salt-salt!!", 4096);
        let run = |verifier: Option<ScramVerifier>, password: &str| {
            let mut server = ScramServer::new(verifier, b"fake", "SNONCE");
            let mut client = ScramClient::new(password, "CNONCE");
            let first = server.first(&client.first()).unwrap();
            let fin = client.respond(&first).unwrap();
            server.finish(&fin).map(|f| client.verify(&f))
        };
        assert_eq!(run(Some(v.clone()), "pencil"), Ok(Ok(())));
        assert_eq!(run(Some(v.clone()), "pen"), Err(ScramError::Rejected));
        assert_eq!(run(None, "pencil"), Err(ScramError::Rejected));
        // A server that does not know the verifier cannot sign.
        let mut client = ScramClient::new("pencil", "CNONCE");
        let mut server = ScramServer::new(Some(v), b"", "S");
        client
            .respond(&server.first(&client.first()).unwrap())
            .unwrap();
        assert_eq!(client.verify("v=AAAA"), Err(ScramError::Rejected));
    }

    #[test]
    fn scram_verifiers() {
        // RFC 7677's example: user "user", password "pencil", salt
        // W22ZaJ0SNY7soEsUEjb6gQ==, 4096 iterations.
        let salt = base64_decode("W22ZaJ0SNY7soEsUEjb6gQ==").unwrap();
        let v = ScramVerifier::new("pencil", &salt, 4096);
        let salted = pbkdf2_sha256(b"pencil", &salt, 4096);
        // Its ServerSignature in the RFC follows from ServerKey; check the
        // derivation by the client proof the RFC gives.
        let client_key = hmac_sha256(&salted, b"Client Key");
        let auth = "n=user,r=rOprNGfwEbeRWgbNEkqO,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096,c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0";
        let signature = hmac_sha256(&v.stored_key, auth.as_bytes());
        let proof: Vec<u8> = client_key
            .iter()
            .zip(signature)
            .map(|(a, b)| a ^ b)
            .collect();
        assert_eq!(
            base64_encode(&proof),
            "dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ="
        );
        let server_signature = hmac_sha256(&v.server_key, auth.as_bytes());
        assert_eq!(
            base64_encode(&server_signature),
            "6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4="
        );
        assert!(v.matches("pencil") && !v.matches("pencil2"));
        assert_eq!(ScramVerifier::parse(&v.to_text()), Some(v));
    }
}
