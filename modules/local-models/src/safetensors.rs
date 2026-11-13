//! Hugging Face's safetensors format: an 8-byte little-endian header
//! length, a JSON header naming each tensor's type, shape and byte range,
//! then the bytes.  Read here without copying the file: a tensor is
//! converted to `f32` when it is taken.

use std::collections::HashMap;

use serde_json::Value as Json;

/// A tensor's element type.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Dtype {
    F32,
    F16,
    Bf16,
    Other,
}

struct Entry {
    dtype: Dtype,
    shape: Vec<usize>,
    start: usize,
    end: usize,
}

/// The tensors of a safetensors file, over its bytes.
pub struct SafeTensors<'a> {
    data: &'a [u8],
    entries: HashMap<String, Entry>,
}

impl<'a> SafeTensors<'a> {
    /// Reads the header of `bytes`.
    pub fn new(bytes: &'a [u8]) -> Result<SafeTensors<'a>, String> {
        let bad = |why: &str| format!("safetensors: {why}");
        let len = bytes
            .get(..8)
            .map(|b| u64::from_le_bytes(b.try_into().unwrap_or_default()) as usize)
            .ok_or_else(|| bad("shorter than its header"))?;
        let header = bytes
            .get(8..8usize.saturating_add(len))
            .ok_or_else(|| bad("shorter than its header"))?;
        let data = &bytes[8 + len..];
        let header: Json =
            serde_json::from_slice(header).map_err(|e| bad(&format!("a header not JSON: {e}")))?;
        let mut entries = HashMap::new();
        for (name, t) in header
            .as_object()
            .ok_or_else(|| bad("a header not an object"))?
        {
            if name == "__metadata__" {
                continue;
            }
            let dtype = match t["dtype"].as_str() {
                Some("F32") => Dtype::F32,
                Some("F16") => Dtype::F16,
                Some("BF16") => Dtype::Bf16,
                _ => Dtype::Other,
            };
            let shape = t["shape"]
                .as_array()
                .and_then(|s| s.iter().map(|d| d.as_u64().map(|d| d as usize)).collect())
                .ok_or_else(|| bad(&format!("{name} without a shape")))?;
            let (start, end) = match (t["data_offsets"][0].as_u64(), t["data_offsets"][1].as_u64())
            {
                (Some(s), Some(e)) if s <= e && e as usize <= data.len() => {
                    (s as usize, e as usize)
                }
                _ => return Err(bad(&format!("{name}'s bytes are not in the file"))),
            };
            entries.insert(
                name.clone(),
                Entry {
                    dtype,
                    shape,
                    start,
                    end,
                },
            );
        }
        Ok(SafeTensors { data, entries })
    }

    /// The names of the tensors.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }

    /// Tensor `name` as `f32`s, with its shape, which must be `shape`.
    pub fn f32(&self, name: &str, shape: &[usize]) -> Result<Vec<f32>, String> {
        let e = self
            .entries
            .get(name)
            .ok_or_else(|| format!("the model has no tensor {name}"))?;
        if e.shape != shape {
            return Err(format!(
                "tensor {name} is {:?}, the configuration says {shape:?}",
                e.shape
            ));
        }
        let bytes = &self.data[e.start..e.end];
        let size = match e.dtype {
            Dtype::F32 => 4,
            Dtype::F16 | Dtype::Bf16 => 2,
            Dtype::Other => return Err(format!("tensor {name} is not F32, F16 or BF16")),
        };
        // Checked: a shape whose product wraps would match an empty
        // tensor.
        let n = shape
            .iter()
            .try_fold(1usize, |a, &d| a.checked_mul(d))
            .and_then(|n| n.checked_mul(size));
        if n != Some(bytes.len()) {
            return Err(format!("tensor {name}'s bytes do not match its shape"));
        }
        Ok(match e.dtype {
            Dtype::F32 => bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|b| f32::from_le_bytes(*b))
                .collect(),
            Dtype::F16 => bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| half(u16::from_le_bytes(*b)))
                .collect(),
            Dtype::Bf16 => bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| f32::from_bits(u32::from(u16::from_le_bytes(*b)) << 16))
                .collect(),
            Dtype::Other => unreachable!(),
        })
    }
}

/// An IEEE half as `f32`.
fn half(h: u16) -> f32 {
    let sign = u32::from(h >> 15) << 31;
    let exp = u32::from((h >> 10) & 0x1F);
    let frac = u32::from(h & 0x3FF);
    let bits = match exp {
        0 if frac == 0 => sign,
        0 => {
            // Subnormal: normalize.
            let shift = frac.leading_zeros() - 21;
            sign | ((113 - shift) << 23) | ((frac << shift) & 0x3FF) << 13
        }
        0x1F => sign | 0x7F80_0000 | (frac << 13),
        _ => sign | ((exp + 112) << 23) | (frac << 13),
    };
    f32::from_bits(bits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_tensors_of_each_type() {
        let header = br#"{"a":{"dtype":"F32","shape":[2],"data_offsets":[0,8]},"b":{"dtype":"F16","shape":[3],"data_offsets":[8,14]},"c":{"dtype":"BF16","shape":[1],"data_offsets":[14,16]},"__metadata__":{"format":"pt"}}"#;
        let mut file = (header.len() as u64).to_le_bytes().to_vec();
        file.extend_from_slice(header);
        file.extend(1.5f32.to_le_bytes());
        file.extend((-2.0f32).to_le_bytes());
        for h in [0x3C00u16, 0x0001, 0xC000] {
            file.extend(h.to_le_bytes());
        }
        file.extend(((0.25f32.to_bits() >> 16) as u16).to_le_bytes());
        let t = SafeTensors::new(&file).unwrap();
        assert_eq!(t.f32("a", &[2]).unwrap(), [1.5, -2.0]);
        assert_eq!(t.f32("b", &[3]).unwrap(), [1.0, 2f32.powi(-24), -2.0]);
        assert_eq!(t.f32("c", &[1]).unwrap(), [0.25]);
        assert!(t.f32("a", &[3]).unwrap_err().contains("configuration"));
        assert!(t.f32("z", &[1]).is_err());
        assert!(SafeTensors::new(&file[..20]).is_err());
        // A shape whose product wraps to the bytes there are: refused.
        let wraps = 1usize << (usize::BITS - 1);
        assert!(t.f32("a", &[2, wraps]).is_err());
    }
}
