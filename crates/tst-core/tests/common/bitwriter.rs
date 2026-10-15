//! MSB-first bit writer for hand-built codec bitstreams in integration
//! tests. The in-crate twin is `codec::test_util::BitWriter`, which
//! integration tests cannot reach.

#[derive(Default)]
pub struct BitWriter {
    pub bytes: Vec<u8>,
    pub pos: u32,
}

impl BitWriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Write the lowest `n` bits of `value`, MSB first.
    pub fn write(&mut self, value: u32, n: u32) {
        for i in (0..n).rev() {
            let bit = ((value >> i) & 1) as u8;
            let byte_idx = (self.pos / 8) as usize;
            let bit_in_byte = 7 - (self.pos % 8);
            if byte_idx == self.bytes.len() {
                self.bytes.push(0);
            }
            self.bytes[byte_idx] |= bit << bit_in_byte;
            self.pos += 1;
        }
    }

    /// Exp-Golomb ue(v) (H.264 / H.265 / H.266 §9.x).
    pub fn write_ue(&mut self, value: u32) {
        let v = value + 1;
        let leading_zeros = 31 - v.leading_zeros();
        for _ in 0..leading_zeros {
            self.write(0, 1);
        }
        self.write(v, leading_zeros + 1);
    }

    /// rbsp_trailing_bits(): one '1' bit, then zero-pad to a byte boundary.
    pub fn end_rbsp(&mut self) {
        self.write(1, 1);
        while self.pos % 8 != 0 {
            self.write(0, 1);
        }
    }
}
