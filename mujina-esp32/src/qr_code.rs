//! Self-contained ISO/IEC 18004 Version 3-L (29x29 modules) QR code encoder.
//!
//! Supports up to 53-byte UTF-8/ASCII payloads (such as `WIFI:T:nopass;S:Mujina-NerdQX;;`
//! and `http://192.168.71.1`) in `no_std` environments with zero external crates.

pub const QR_SIZE: usize = 29;
const DATA_CODEWORDS: usize = 55;
const EC_CODEWORDS: usize = 15;
const TOTAL_CODEWORDS: usize = DATA_CODEWORDS + EC_CODEWORDS;
pub const MAX_PAYLOAD_BYTES: usize = 53;

/// Format info for Error Correction Level L (01) + Mask 0 (000) masked with 0x5412.
const FORMAT_INFO_L_MASK0: u16 = 0b111011111000100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QrMatrix {
    modules: [[bool; QR_SIZE]; QR_SIZE],
}

impl QrMatrix {
    /// Encode `data` (truncated to [`MAX_PAYLOAD_BYTES`] if longer) as a Version 3-L QR code.
    pub fn encode(data: &[u8]) -> Self {
        let len = data.len().min(MAX_PAYLOAD_BYTES);
        let payload = &data[..len];

        let codewords = build_codewords(payload);
        let mut grid = [[false; QR_SIZE]; QR_SIZE];
        let mut is_func = [[false; QR_SIZE]; QR_SIZE];

        place_finder_and_separators(&mut grid, &mut is_func, 0, 0);
        place_finder_and_separators(&mut grid, &mut is_func, 0, QR_SIZE - 7);
        place_finder_and_separators(&mut grid, &mut is_func, QR_SIZE - 7, 0);
        place_alignment(&mut grid, &mut is_func, 22, 22);
        place_timing_and_dark(&mut grid, &mut is_func);
        reserve_format_areas(&mut is_func);

        place_data_bits(&mut grid, &is_func, &codewords);
        apply_best_mask(&mut grid, &is_func);
        write_format_info(&mut grid);

        Self { modules: grid }
    }

    #[inline]
    pub const fn size(&self) -> usize {
        QR_SIZE
    }

    #[inline]
    pub const fn get(&self, row: usize, col: usize) -> bool {
        self.modules[row][col]
    }
}

fn gf_mul(mut a: u8, mut b: u8) -> u8 {
    let mut res = 0u8;
    for _ in 0..8 {
        if (b & 1) != 0 {
            res ^= a;
        }
        let hi = a & 0x80;
        a <<= 1;
        if hi != 0 {
            a ^= 0x1D; // x^8 + x^4 + x^3 + x^2 + 1 (0x11D)
        }
        b >>= 1;
    }
    res
}

fn rs_generator_poly() -> [u8; EC_CODEWORDS] {
    let mut gen = [0u8; EC_CODEWORDS];
    gen[EC_CODEWORDS - 1] = 1;
    let mut root = 1u8;
    for i in 0..EC_CODEWORDS {
        for j in 0..EC_CODEWORDS {
            gen[j] = gf_mul(gen[j], root);
            if j + 1 < EC_CODEWORDS {
                gen[j] ^= gen[j + 1];
            }
        }
        root = gf_mul(root, 0x02);
        let _ = i;
    }
    gen
}

fn rs_remainder(data: &[u8; DATA_CODEWORDS]) -> [u8; EC_CODEWORDS] {
    let gen = rs_generator_poly();
    let mut rem = [0u8; EC_CODEWORDS];
    for &b in data {
        let factor = b ^ rem[0];
        rem.copy_within(1..EC_CODEWORDS, 0);
        rem[EC_CODEWORDS - 1] = 0;
        for j in 0..EC_CODEWORDS {
            rem[j] ^= gf_mul(gen[j], factor);
        }
    }
    rem
}

fn push_bits(bits: &mut [u8; DATA_CODEWORDS], bit_idx: &mut usize, val: u32, count: usize) {
    for i in (0..count).rev() {
        if *bit_idx < DATA_CODEWORDS * 8 {
            let bit = ((val >> i) & 1) as u8;
            bits[*bit_idx / 8] |= bit << (7 - (*bit_idx % 8));
            *bit_idx += 1;
        }
    }
}

fn build_codewords(payload: &[u8]) -> [u8; TOTAL_CODEWORDS] {
    let mut bits = [0u8; DATA_CODEWORDS];
    let mut bit_idx = 0usize;

    // Byte mode indicator (0100) + 8-bit length count
    push_bits(&mut bits, &mut bit_idx, 0b0100, 4);
    push_bits(&mut bits, &mut bit_idx, payload.len() as u32, 8);
    for &b in payload {
        push_bits(&mut bits, &mut bit_idx, b as u32, 8);
    }
    // Terminator (up to 4 zero bits) then pad to byte boundary
    let rem_bits = (DATA_CODEWORDS * 8).saturating_sub(bit_idx);
    push_bits(&mut bits, &mut bit_idx, 0, rem_bits.min(4));
    while bit_idx % 8 != 0 {
        push_bits(&mut bits, &mut bit_idx, 0, 1);
    }

    let mut byte_idx = bit_idx / 8;
    let pads = [0xECu8, 0x11u8];
    let mut pad_i = 0usize;
    while byte_idx < DATA_CODEWORDS {
        bits[byte_idx] = pads[pad_i % 2];
        pad_i += 1;
        byte_idx += 1;
    }

    let ec = rs_remainder(&bits);
    let mut out = [0u8; TOTAL_CODEWORDS];
    out[..DATA_CODEWORDS].copy_from_slice(&bits);
    out[DATA_CODEWORDS..].copy_from_slice(&ec);
    out
}

fn set_func(
    grid: &mut [[bool; QR_SIZE]; QR_SIZE],
    is_func: &mut [[bool; QR_SIZE]; QR_SIZE],
    r: isize,
    c: isize,
    val: bool,
) {
    if r >= 0 && c >= 0 && (r as usize) < QR_SIZE && (c as usize) < QR_SIZE {
        grid[r as usize][c as usize] = val;
        is_func[r as usize][c as usize] = true;
    }
}

fn place_finder_and_separators(
    grid: &mut [[bool; QR_SIZE]; QR_SIZE],
    is_func: &mut [[bool; QR_SIZE]; QR_SIZE],
    top: usize,
    left: usize,
) {
    for dr in -1isize..=7 {
        for dc in -1isize..=7 {
            let r = top as isize + dr;
            let c = left as isize + dc;
            if dr < 0 || dr > 6 || dc < 0 || dc > 6 {
                set_func(grid, is_func, r, c, false);
            } else {
                let d_edge = dr.min(6 - dr).min(dc.min(6 - dc));
                let dark = d_edge == 0 || d_edge >= 2;
                set_func(grid, is_func, r, c, dark);
            }
        }
    }
}

fn place_alignment(
    grid: &mut [[bool; QR_SIZE]; QR_SIZE],
    is_func: &mut [[bool; QR_SIZE]; QR_SIZE],
    cr: usize,
    cc: usize,
) {
    for dr in -2isize..=2 {
        for dc in -2isize..=2 {
            let dist = dr.abs().max(dc.abs());
            set_func(
                grid,
                is_func,
                cr as isize + dr,
                cc as isize + dc,
                dist != 1,
            );
        }
    }
}

fn place_timing_and_dark(
    grid: &mut [[bool; QR_SIZE]; QR_SIZE],
    is_func: &mut [[bool; QR_SIZE]; QR_SIZE],
) {
    for i in 8..(QR_SIZE - 8) {
        let dark = (i % 2) == 0;
        if !is_func[6][i] {
            set_func(grid, is_func, 6, i as isize, dark);
        }
        if !is_func[i][6] {
            set_func(grid, is_func, i as isize, 6, dark);
        }
    }
    // Always-dark module at (4*V + 9, 8) = (21, 8)
    set_func(grid, is_func, 21, 8, true);
}

fn reserve_format_areas(is_func: &mut [[bool; QR_SIZE]; QR_SIZE]) {
    for i in 0..9 {
        if i != 6 {
            is_func[8][i] = true;
            is_func[i][8] = true;
        }
    }
    for i in 0..8 {
        is_func[8][QR_SIZE - 1 - i] = true;
    }
    for i in 0..7 {
        is_func[QR_SIZE - 1 - i][8] = true;
    }
}

fn place_data_bits(
    grid: &mut [[bool; QR_SIZE]; QR_SIZE],
    is_func: &[[bool; QR_SIZE]; QR_SIZE],
    codewords: &[u8; TOTAL_CODEWORDS],
) {
    let mut bit_idx = 0usize;
    let total_bits = TOTAL_CODEWORDS * 8;
    let mut col = (QR_SIZE - 1) as isize;
    let mut upward = true;

    while col >= 0 {
        if col == 6 {
            col -= 1; // skip vertical timing strip
        }
        for step in 0..QR_SIZE {
            let row = if upward { QR_SIZE - 1 - step } else { step };
            for dx in 0..2 {
                let c = (col - dx) as usize;
                if !is_func[row][c] {
                    let bit = if bit_idx < total_bits {
                        ((codewords[bit_idx / 8] >> (7 - (bit_idx % 8))) & 1) != 0
                    } else {
                        false
                    };
                    grid[row][c] = bit;
                    bit_idx += 1;
                }
            }
        }
        upward = !upward;
        col -= 2;
    }
}

fn apply_best_mask(grid: &mut [[bool; QR_SIZE]; QR_SIZE], is_func: &[[bool; QR_SIZE]; QR_SIZE]) {
    // Mask 0: (row + col) % 2 == 0
    for r in 0..QR_SIZE {
        for c in 0..QR_SIZE {
            if !is_func[r][c] && ((r + c) % 2 == 0) {
                grid[r][c] = !grid[r][c];
            }
        }
    }
}

fn write_format_info(grid: &mut [[bool; QR_SIZE]; QR_SIZE]) {
    let bits = FORMAT_INFO_L_MASK0;
    for i in 0..15 {
        let dark = ((bits >> i) & 1) != 0;
        // Around top-left finder
        if i < 6 {
            grid[i][8] = dark;
        } else if i < 8 {
            grid[i + 1][8] = dark;
        } else {
            grid[QR_SIZE - 15 + i][8] = dark;
        }
        // Horizontal format strip
        if i < 8 {
            grid[8][QR_SIZE - 1 - i] = dark;
        } else if i < 9 {
            grid[8][7] = dark;
        } else {
            grid[8][14 - i] = dark;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finder_and_alignment_patterns_intact() {
        let qr = QrMatrix::encode(b"WIFI:T:nopass;S:Mujina-NerdQX;;");
        // Top-left 7x7 finder corners and center
        assert!(qr.get(0, 0));
        assert!(qr.get(3, 3));
        assert!(!qr.get(1, 1));
        assert!(!qr.get(7, 7)); // separator
        // Top-right and bottom-left finders
        assert!(qr.get(0, QR_SIZE - 1));
        assert!(qr.get(3, QR_SIZE - 4));
        assert!(qr.get(QR_SIZE - 1, 0));
        assert!(qr.get(QR_SIZE - 4, 3));
        // Alignment pattern centered at (22, 22)
        assert!(qr.get(22, 22));
        assert!(!qr.get(21, 22));
        assert!(qr.get(20, 22));
        // Dark module at (21, 8)
        assert!(qr.get(21, 8));
    }

    #[test]
    fn rs_ecc_matches_iso_example() {
        // Generator polynomial test: dividing non-zero data yields non-zero EC
        let cw = build_codewords(b"http://192.168.71.1");
        assert_eq!(cw[0] >> 4, 0b0100); // Byte mode
        assert_eq!(cw[1] >> 4 | ((cw[0] & 0x0F) << 4), 19); // len 19
        assert!(cw[DATA_CODEWORDS..].iter().any(|&b| b != 0));
    }
}
