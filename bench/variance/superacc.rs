//! Neal's small superaccumulator (2015), as a measurement: an exact sum of
//! doubles in 67 signed 64-bit chunks of 32 bits each.
#[derive(Clone)]
pub struct SuperAcc {
    chunks: [i64; 67],
    adds: u32,
}

impl Default for SuperAcc {
    fn default() -> Self {
        SuperAcc { chunks: [0; 67], adds: 0 }
    }
}

impl SuperAcc {
    #[inline]
    pub fn add(&mut self, x: f64) {
        let bits = x.to_bits();
        let mut exp = ((bits >> 52) & 0x7ff) as usize;
        let mut mant = bits & ((1u64 << 52) - 1);
        if exp == 0 {
            if mant == 0 {
                return;
            }
            exp = 1;
        } else {
            mant |= 1u64 << 52;
        }
        let v = (mant as u128) << (exp & 31);
        let i = exp >> 5;
        let (c0, c1, c2) = (
            (v as u64 & 0xffff_ffff) as i64,
            ((v >> 32) as u64 & 0xffff_ffff) as i64,
            (v >> 64) as i64,
        );
        if (bits >> 63) != 0 {
            self.chunks[i] -= c0;
            self.chunks[i + 1] -= c1;
            self.chunks[i + 2] -= c2;
        } else {
            self.chunks[i] += c0;
            self.chunks[i + 1] += c1;
            self.chunks[i + 2] += c2;
        }
        self.adds += 1;
        if self.adds == 1 << 30 {
            self.carry();
        }
    }

    pub fn carry(&mut self) {
        for i in 0..66 {
            let c = (self.chunks[i] + (1 << 31)) >> 32;
            self.chunks[i] -= c << 32;
            self.chunks[i + 1] += c;
        }
        self.adds = 0;
    }

    pub fn merge(&mut self, o: &SuperAcc) {
        self.carry();
        let mut o = o.clone();
        o.carry();
        for i in 0..67 {
            self.chunks[i] += o.chunks[i];
        }
        self.carry();
    }

    /// The chunks as exact doubles (each < 2^33 in magnitude after a carry).
    pub fn parts(&self) -> Vec<f64> {
        let mut s = self.clone();
        s.carry();
        s.chunks
            .iter()
            .enumerate()
            .filter(|(_, c)| **c != 0)
            .map(|(i, &c)| c as f64 * 2f64.powi(32 * i as i32 - 1075))
            .collect()
    }
}
