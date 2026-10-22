//! `inet` and `cidr` (PLAN.md §M14.26): IPv4 and IPv6 host addresses with
//! an optional network mask, and networks.
//!
//! **Order** (PostgreSQL's): IPv4 before IPv6; then the address masked to
//! its own mask length; then the mask length; then the whole address.  In
//! it a network's addresses — the values `<<=` it — are one contiguous
//! range, from the network itself to its broadcast address with a full
//! mask, so containment is an index range scan
//! ([`ScalarFunction::implied_range`]).  Keys follow the same order (tag
//! `0F`, the family, the masked address, the mask length, the address);
//! an `inet` and a `cidr` that are equal have the same key.

use std::cmp::Ordering;
use std::ops::RangeInclusive;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::ext::{
    Arg, ArgInfo, CallContext, Registry, Returns, ScalarFunction, Signature, Volatility,
};
use crate::types::{DataType, Value};

/// An address and mask length: an `inet` or a `cidr` value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Net {
    /// IPv6 (else IPv4).
    pub v6: bool,
    /// The address (IPv4 in the low 32 bits).
    pub addr: u128,
    /// The mask length.
    pub bits: u8,
}

impl Net {
    /// 32 or 128.
    pub fn max_bits(self) -> u8 {
        if self.v6 { 128 } else { 32 }
    }

    fn mask(self) -> u128 {
        let width = u32::from(self.max_bits());
        let all = if self.v6 {
            u128::MAX
        } else {
            u128::from(u32::MAX)
        };
        if self.bits == 0 {
            0
        } else {
            all & !((1u128 << (width - u32::from(self.bits))) - 1)
        }
    }

    fn host_mask(self) -> u128 {
        let all = if self.v6 {
            u128::MAX
        } else {
            u128::from(u32::MAX)
        };
        all & !self.mask()
    }

    /// The network: host bits cleared.
    pub fn network(self) -> Net {
        Net {
            addr: self.addr & self.mask(),
            ..self
        }
    }

    /// The broadcast address: host bits set.
    pub fn broadcast(self) -> Net {
        Net {
            addr: self.addr | self.host_mask(),
            ..self
        }
    }

    /// Whether `self`'s first `bits` bits equal `other`'s.
    fn same_prefix(self, other: Net, bits: u8) -> bool {
        let m = Net { bits, ..self }.mask();
        self.v6 == other.v6 && self.addr & m == other.addr & m
    }

    /// `self <<= other`: within the network `other`, or equal to it.
    pub fn within(self, other: Net) -> bool {
        other.bits <= self.bits && self.same_prefix(other, other.bits)
    }

    /// `self && other`: either contains the other.
    pub fn overlaps(self, other: Net) -> bool {
        self.same_prefix(other, self.bits.min(other.bits))
    }

    /// PostgreSQL's order (see the module documentation).
    pub fn cmp_net(self, other: Net) -> Ordering {
        self.v6
            .cmp(&other.v6)
            .then((self.addr & self.mask()).cmp(&(other.addr & other.mask())))
            .then(self.bits.cmp(&other.bits))
            .then(self.addr.cmp(&other.addr))
    }

    fn addr_bytes(self, a: u128) -> Vec<u8> {
        if self.v6 {
            a.to_be_bytes().to_vec()
        } else {
            (a as u32).to_be_bytes().to_vec()
        }
    }

    /// The order-preserving key bytes (after the tag).
    pub fn key(self, out: &mut Vec<u8>) {
        out.push(if self.v6 { 6 } else { 4 });
        out.extend(self.addr_bytes(self.addr & self.mask()));
        out.push(self.bits);
        out.extend(self.addr_bytes(self.addr));
    }

    /// Decodes [`Net::key`]; returns it and the bytes used.
    pub fn from_key(b: &[u8]) -> Option<(Net, usize)> {
        let v6 = match b.first()? {
            4 => false,
            6 => true,
            _ => return None,
        };
        let n = if v6 { 16 } else { 4 };
        let bits = *b.get(1 + n)?;
        let addr = b.get(2 + n..2 + 2 * n)?;
        let mut a = [0u8; 16];
        a[16 - n..].copy_from_slice(addr);
        Some((
            Net {
                v6,
                addr: u128::from_be_bytes(a),
                bits,
            },
            2 + 2 * n,
        ))
    }

    /// The row form: family (4 or 6), mask length, address bytes.
    pub fn to_bytes(self) -> Vec<u8> {
        let mut out = vec![if self.v6 { 6 } else { 4 }, self.bits];
        out.extend(self.addr_bytes(self.addr));
        out
    }

    /// Reads [`Net::to_bytes`] (length checked by the caller's reader).
    pub fn from_bytes(b: &[u8]) -> Option<Net> {
        let (v6, n) = match b.first()? {
            4 => (false, 4),
            6 => (true, 16),
            _ => return None,
        };
        let bits = *b.get(1)?;
        let addr = b.get(2..2 + n)?;
        let mut a = [0u8; 16];
        a[16 - n..].copy_from_slice(addr);
        let net = Net {
            v6,
            addr: u128::from_be_bytes(a),
            bits,
        };
        (bits <= net.max_bits()).then_some(net)
    }

    /// The address alone, as text (`host()`).
    pub fn host_text(self) -> String {
        if self.v6 {
            v6_text(self.addr)
        } else {
            let b = (self.addr as u32).to_be_bytes();
            format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3])
        }
    }

    /// An `inet`'s text: the address, and `/bits` unless a full mask.
    pub fn inet_text(self) -> String {
        if self.bits == self.max_bits() {
            self.host_text()
        } else {
            format!("{}/{}", self.host_text(), self.bits)
        }
    }

    /// A `cidr`'s text: always with `/bits`.
    pub fn cidr_text(self) -> String {
        format!("{}/{}", self.host_text(), self.bits)
    }
}

/// An IPv6 address as PostgreSQL prints it: lower-case groups, the
/// longest run of two or more zero groups as `::` (the first of equal
/// ones), and an embedded IPv4 address (`::ffff:a.b.c.d`, `::a.b.c.d`)
/// dotted.
fn v6_text(a: u128) -> String {
    let w: Vec<u16> = (0..8).map(|i| (a >> (112 - 16 * i)) as u16).collect();
    let (mut best, mut best_len) = (None, 0);
    let mut i = 0;
    while i < 8 {
        if w[i] == 0 {
            let start = i;
            while i < 8 && w[i] == 0 {
                i += 1;
            }
            if i - start > best_len {
                best = Some(start);
                best_len = i - start;
            }
        } else {
            i += 1;
        }
    }
    if best_len < 2 {
        best = None;
    }
    if best == Some(0) && (best_len == 6 || (best_len == 5 && w[5] == 0xffff)) {
        let v4 = (a as u32).to_be_bytes();
        let prefix = if best_len == 5 { "::ffff:" } else { "::" };
        return format!("{prefix}{}.{}.{}.{}", v4[0], v4[1], v4[2], v4[3]);
    }
    let mut out = String::new();
    let mut i = 0;
    while i < 8 {
        if Some(i) == best {
            out.push_str("::");
            i += best_len;
            continue;
        }
        if !out.is_empty() && !out.ends_with(':') {
            out.push(':');
        }
        out.push_str(&format!("{:x}", w[i]));
        i += 1;
    }
    out
}

fn parse_v4(s: &str) -> Option<u32> {
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    let mut a = 0u32;
    for p in parts {
        if p.is_empty() || p.len() > 3 || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let o: u32 = p.parse().ok()?;
        if o > 255 {
            return None;
        }
        a = a << 8 | o;
    }
    Some(a)
}

fn parse_v6(s: &str) -> Option<u128> {
    let (head, tail) = match s.split_once("::") {
        Some((h, t)) => {
            if t.contains("::") {
                return None;
            }
            (h, Some(t))
        }
        None => (s, None),
    };
    let groups = |part: &str, last: bool| -> Option<Vec<u16>> {
        if part.is_empty() {
            return Some(Vec::new());
        }
        let items: Vec<&str> = part.split(':').collect();
        let mut out = Vec::new();
        for (i, g) in items.iter().enumerate() {
            if last && i == items.len() - 1 && g.contains('.') {
                let v4 = parse_v4(g)?;
                out.push((v4 >> 16) as u16);
                out.push(v4 as u16);
            } else {
                if g.is_empty() || g.len() > 4 {
                    return None;
                }
                out.push(u16::from_str_radix(g, 16).ok()?);
            }
        }
        Some(out)
    };
    let h = groups(head, tail.is_none())?;
    let words = match tail {
        None => {
            if h.len() != 8 {
                return None;
            }
            h
        }
        Some(t) => {
            let t = groups(t, true)?;
            if h.len() + t.len() > 7 {
                return None;
            }
            let mut w = h;
            w.resize(8 - t.len(), 0);
            w.extend(t);
            w
        }
    };
    Some(words.iter().fold(0u128, |a, &g| a << 16 | u128::from(g)))
}

fn bad(ty: &str, s: &str) -> Error {
    Error::invalid_text(format!("invalid input syntax for type {ty}: \"{s}\""))
}

/// An address with an optional `/bits`.
fn parse_net(s: &str, ty: &str) -> Result<Net> {
    let (addr, bits) = match s.split_once('/') {
        Some((a, b)) => (a, Some(b)),
        None => (s, None),
    };
    let (v6, a) = if addr.contains(':') {
        (true, parse_v6(addr).ok_or_else(|| bad(ty, s))?)
    } else {
        (false, u128::from(parse_v4(addr).ok_or_else(|| bad(ty, s))?))
    };
    let max = if v6 { 128 } else { 32 };
    let bits = match bits {
        None => max,
        Some(b) => b
            .parse::<u8>()
            .ok()
            .filter(|&b| b <= max)
            .ok_or_else(|| bad(ty, s))?,
    };
    Ok(Net { v6, addr: a, bits })
}

/// Parses an `inet`: an IPv4 or IPv6 address, optionally `/bits`.
pub fn parse_inet(s: &str) -> Result<Net> {
    parse_net(s, "inet")
}

/// Parses a `cidr`: a network, whose host bits must be zero.  An IPv4 one
/// may give fewer than four octets (`10`, `192.168/16`); without `/bits`
/// its mask is the address class's, widened to the octets given.
pub fn parse_cidr(s: &str) -> Result<Net> {
    let net = match s.split_once('/').map_or(s, |(a, _)| a) {
        a if !a.contains(':') && a.split('.').count() < 4 => {
            let (addr, bits) = match s.split_once('/') {
                Some((a, b)) => (a, Some(b)),
                None => (s, None),
            };
            let octets: Vec<&str> = addr.split('.').collect();
            let mut full = octets.clone();
            full.resize(4, "0");
            let a = parse_v4(&full.join(".")).ok_or_else(|| bad("cidr", s))?;
            let bits = match bits {
                Some(b) => b
                    .parse::<u8>()
                    .ok()
                    .filter(|&b| b <= 32)
                    .ok_or_else(|| bad("cidr", s))?,
                None => {
                    let first = a >> 24;
                    let class = match first {
                        240.. => 32,
                        224.. => 8,
                        192.. => 24,
                        128.. => 16,
                        _ => 8,
                    };
                    let bits = class.max(8 * octets.len() as u8);
                    if bits == 8 && first == 224 { 4 } else { bits }
                }
            };
            Net {
                v6: false,
                addr: u128::from(a),
                bits,
            }
        }
        _ => parse_net(s, "cidr")?,
    };
    if net.addr & net.host_mask() != 0 {
        return Err(Error::invalid_text(format!(
            "invalid cidr value: \"{s}\": value has bits set to right of mask"
        )));
    }
    Ok(net)
}

// ----------------------------------------------------------- functions

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum F {
    Host,
    Abbrev,
    Network,
    Masklen,
    Family,
    Broadcast,
    Netmask,
    Hostmask,
    SetMasklen,
    SameFamily,
    // Operators.
    Within,
    WithinEq,
    Contains,
    ContainsEq,
    Overlaps,
    // The bounds of a network's range (for an index scan).
    Low,
    High,
}

struct NetFn(F, &'static str);

/// A function's network argument, and whether it is a `cidr`.
fn net(v: &Value) -> Result<(Net, bool)> {
    match v {
        Value::Inet(n) => Ok((*n, false)),
        Value::Cidr(n) => Ok((*n, true)),
        v => Err(Error::internal(format!("expected inet, got {v:?}"))),
    }
}

fn is_net(t: DataType) -> bool {
    matches!(t, DataType::Inet | DataType::Cidr)
}

impl ScalarFunction for NetFn {
    fn name(&self) -> &str {
        self.1
    }

    fn arity(&self) -> RangeInclusive<usize> {
        match self.0 {
            F::SetMasklen
            | F::SameFamily
            | F::Within
            | F::WithinEq
            | F::Contains
            | F::ContainsEq
            | F::Overlaps => 2..=2,
            _ => 1..=1,
        }
    }

    fn resolve(&self, args: &[ArgInfo]) -> Result<Signature> {
        use DataType::{Bool, Cidr, Inet, Integer, Text};
        // A network argument: inet or cidr (an untyped literal: inet).
        let arg = |a: &ArgInfo| -> Result<(Arg, DataType)> {
            match a.ty.filter(|_| !a.text_literal) {
                Some(t) if is_net(t) => Ok((Arg::Keep, t)),
                None => Ok((Arg::Untyped(Inet), Inet)),
                Some(t) => Err(Error::undefined_function(format!(
                    "function {}({t}) does not exist",
                    self.1
                ))),
            }
        };
        let (a0, t0) = arg(&args[0])?;
        let ret = |r: DataType| Returns::Type(Some(r));
        Ok(match self.0 {
            F::Host | F::Abbrev => Signature::new(vec![a0], ret(Text)),
            F::Network => Signature::new(vec![a0], ret(Cidr)),
            F::Masklen | F::Family => Signature::new(vec![a0], ret(Integer)),
            F::Broadcast | F::Netmask | F::Hostmask | F::Low | F::High => {
                Signature::new(vec![a0], ret(Inet))
            }
            F::SetMasklen => Signature::new(vec![a0, Arg::Untyped(Integer)], ret(t0)),
            _ => {
                let (a1, _) = arg(&args[1])?;
                Signature::new(vec![a0, a1], ret(Bool))
            }
        })
    }

    fn volatility(&self) -> Volatility {
        Volatility::Immutable
    }

    fn implied_range(&self) -> Option<(Arc<dyn ScalarFunction>, Arc<dyn ScalarFunction>)> {
        matches!(self.0, F::Within | F::WithinEq).then(|| {
            (
                Arc::new(NetFn(F::Low, "__inet_low")) as Arc<dyn ScalarFunction>,
                Arc::new(NetFn(F::High, "__inet_high")) as Arc<dyn ScalarFunction>,
            )
        })
    }

    fn call(&self, _: &CallContext<'_>, args: &[Value]) -> Result<Value> {
        let (n, cidr) = net(&args[0])?;
        let inet = Value::Inet;
        Ok(match self.0 {
            F::Host => Value::Text(n.host_text()),
            F::Abbrev if cidr && !n.v6 => {
                let octets = (usize::from(n.bits).div_ceil(8)).max(1);
                let b = (n.addr as u32).to_be_bytes();
                let shown: Vec<String> = b[..octets].iter().map(u8::to_string).collect();
                Value::Text(format!("{}/{}", shown.join("."), n.bits))
            }
            F::Abbrev if cidr => Value::Text(n.cidr_text()),
            F::Abbrev => Value::Text(n.inet_text()),
            F::Network => Value::Cidr(n.network()),
            F::Masklen => Value::Int(i64::from(n.bits)),
            F::Family => Value::Int(if n.v6 { 6 } else { 4 }),
            F::Broadcast => inet(n.broadcast()),
            F::Netmask => inet(Net {
                addr: n.mask(),
                bits: n.max_bits(),
                ..n
            }),
            F::Hostmask => inet(Net {
                addr: n.host_mask(),
                bits: n.max_bits(),
                ..n
            }),
            F::SetMasklen => {
                let Value::Int(b) = args[1] else {
                    return Err(Error::internal("set_masklen length"));
                };
                let b = u8::try_from(b)
                    .ok()
                    .filter(|&b| b <= n.max_bits())
                    .ok_or_else(|| Error::invalid_parameter(format!("invalid mask length: {b}")))?;
                let m = Net { bits: b, ..n };
                if cidr {
                    Value::Cidr(m.network())
                } else {
                    inet(m)
                }
            }
            F::Low => inet(n.network()),
            F::High => inet(Net {
                bits: n.max_bits(),
                ..n.broadcast()
            }),
            _ => {
                let (m, _) = net(&args[1])?;
                Value::Bool(match self.0 {
                    F::SameFamily => n.v6 == m.v6,
                    F::Within => n.within(m) && n.bits > m.bits,
                    F::WithinEq => n.within(m),
                    F::Contains => m.within(n) && m.bits > n.bits,
                    F::ContainsEq => m.within(n),
                    _ => n.overlaps(m),
                })
            }
        })
    }
}

/// Registers the network functions and operators.
pub(crate) fn register(r: &mut Registry) -> Result<()> {
    for (f, name) in [
        (F::Host, "host"),
        (F::Abbrev, "abbrev"),
        (F::Network, "network"),
        (F::Masklen, "masklen"),
        (F::Family, "family"),
        (F::Broadcast, "broadcast"),
        (F::Netmask, "netmask"),
        (F::Hostmask, "hostmask"),
        (F::SetMasklen, "set_masklen"),
        (F::SameFamily, "inet_same_family"),
    ] {
        r.add_function(&[name], Arc::new(NetFn(f, name)))?;
    }
    for (f, op, name) in [
        (F::Within, "<<", "network_sub"),
        (F::WithinEq, "<<=", "network_subeq"),
        (F::Contains, ">>", "network_sup"),
        (F::ContainsEq, ">>=", "network_supeq"),
        (F::Overlaps, "&&", "network_overlap"),
    ] {
        let fun: Arc<dyn ScalarFunction> = Arc::new(NetFn(f, name));
        r.add_function(&[name], fun.clone())?;
        for l in [DataType::Inet, DataType::Cidr] {
            for rt in [DataType::Inet, DataType::Cidr] {
                r.add_operator(op, l, rt, fun.clone())?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inet(s: &str) -> Net {
        parse_inet(s).unwrap()
    }

    #[test]
    fn text_forms() {
        // PostgreSQL 18's output.
        for (input, want) in [
            ("192.168.1.5/24", "192.168.1.5/24"),
            ("192.168.1.5", "192.168.1.5"),
            ("::1", "::1"),
            ("2001:DB8::/32", "2001:db8::/32"),
            ("::ffff:1.2.3.4", "::ffff:1.2.3.4"),
            ("1::2:0:0:3", "1::2:0:0:3"),
            ("0:0:0:1:0:0:0:1", "::1:0:0:0:1"),
            ("::", "::"),
            ("1:0:0:0:0:0:0:0", "1::"),
            ("::1.2.3.4", "::1.2.3.4"),
            ("::ffff:0:0", "::ffff:0.0.0.0"),
            ("64:ff9b::1.2.3.4", "64:ff9b::102:304"),
            ("1:0:0:2::", "1:0:0:2::"),
            ("0:1:0:0:2:0:0:0", "0:1:0:0:2::"),
            ("1.2.3.04", "1.2.3.4"),
        ] {
            assert_eq!(inet(input).inet_text(), want, "{input}");
        }
        for (input, want) in [
            ("10", "10.0.0.0/8"),
            ("10.1", "10.1.0.0/16"),
            ("192.168.1", "192.168.1.0/24"),
            ("128", "128.0.0.0/16"),
            ("224", "224.0.0.0/4"),
            ("10/8", "10.0.0.0/8"),
            ("2001:db8::/32", "2001:db8::/32"),
            ("1.2.3.4", "1.2.3.4/32"),
            ("10.0.0.0/7", "10.0.0.0/7"),
            ("::/0", "::/0"),
        ] {
            assert_eq!(parse_cidr(input).unwrap().cidr_text(), want, "{input}");
        }
        for bad in [
            "10",
            "10.1",
            " 1.2.3.4",
            "256.1.1.1",
            "1.2.3.4/33",
            "abc",
            "1::2::3",
        ] {
            assert!(parse_inet(bad).is_err(), "{bad}");
        }
        assert!(parse_cidr("192.168.1.5/24").is_err());
    }

    #[test]
    fn order_and_containment() {
        // PostgreSQL 18's ORDER BY of these.
        let sorted = [
            "0.0.0.0/0",
            "9.255.0.0/16",
            "10.0.0.0/7",
            "10.0.0.0/8",
            "10.0.0.5/8",
            "10.0.0.5",
            "10.5.0.0/16",
            "11.0.0.1",
            "::1",
        ];
        for w in sorted.windows(2) {
            assert_eq!(inet(w[0]).cmp_net(inet(w[1])), Ordering::Less, "{w:?}");
            let (mut a, mut b) = (Vec::new(), Vec::new());
            inet(w[0]).key(&mut a);
            inet(w[1]).key(&mut b);
            assert!(a < b, "{w:?}");
        }
        let net = parse_cidr("10/8").unwrap();
        assert!(inet("10.1.2.3").within(net));
        assert!(inet("10.0.0.0/8").within(net));
        assert!(!inet("10.0.0.0/7").within(net));
        assert!(!inet("::1").within(net));
        assert!(net.overlaps(parse_cidr("10.1/16").unwrap()));
        assert!(!net.overlaps(parse_cidr("11/8").unwrap()));
    }
}
