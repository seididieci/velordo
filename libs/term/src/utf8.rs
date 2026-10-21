//! Decoder UTF-8 **incrementale** (D3).
//!
//! Lo stream della console e' a byte e arriva a chunk (`DEV_WRITE`): un
//! carattere UTF-8 puo' essere spaccato tra due chunk. Il decoder tiene lo
//! stato (i byte parziali) tra le chiamate e accoda i codepoint decodificati.
//!
//! Sequenze invalide: non perdiamo il sync — emettiamo U+FFFD e ripartiamo
//! dal byte che non poteva continuare la sequenza.

extern crate alloc;

use alloc::vec::Vec;

/// Numero massimo di byte di una sequenza UTF-8.
const MAX_SEQ: usize = 4;

/// U+FFFD: il carattere di sostituzione.
pub const REPLACEMENT: u32 = 0xFFFD;

/// Decoder UTF-8 con stato (i byte parziali della sequenza in corso).
#[derive(Default)]
pub struct Decoder {
    /// Byte parziali della sequenza in corso.
    buf: [u8; MAX_SEQ],
    /// Quanti byte della sequenza sono arrivati.
    len: usize,
    /// Byte attesi in totale per la sequenza in corso (0 = nessuna).
    need: usize,
}

impl Decoder {
    /// Nuovo decoder (stato vuoto).
    pub fn new() -> Self {
        Self::default()
    }

    /// Reset: nessuna sequenza in corso.
    fn reset(&mut self) {
        self.len = 0;
        self.need = 0;
    }

    /// Decodifica i byte e accoda i codepoint in `out`.
    ///
    /// Un codepoint non valido diventa U+FFFD (il stream resta sincronizzato).
    pub fn push(&mut self, bytes: &[u8], out: &mut Vec<u32>) {
        for &b in bytes {
            if self.need == 0 {
                // Byte di inizio.
                if b < 0x80 {
                    out.push(b as u32);
                    continue;
                }
                let need = match b {
                    0xC0..=0xDF => 2,
                    0xE0..=0xEF => 3,
                    0xF0..=0xF7 => 4,
                    _ => {
                        out.push(REPLACEMENT);
                        continue;
                    }
                };
                self.buf[0] = b;
                self.len = 1;
                self.need = need;
                continue;
            }
            // Byte di continuazione atteso.
            if b & 0xC0 != 0x80 {
                // Sequenza rotta: U+FFFD e il byte riparte come nuovo.
                self.reset();
                out.push(REPLACEMENT);
                self.push(&[b], out);
                continue;
            }
            self.buf[self.len] = b;
            self.len += 1;
            if self.len == self.need {
                let (cp, ok) = decode(&self.buf[..self.len]);
                self.reset();
                out.push(if ok { cp } else { REPLACEMENT });
            }
        }
    }
}

/// Decodifica una sequenza completa: `(codepoint, valido)`.
fn decode(seq: &[u8]) -> (u32, bool) {
    match seq.len() {
        1 => (seq[0] as u32, true),
        2 => {
            let cp = (((seq[0] & 0x1F) as u32) << 6) | ((seq[1] & 0x3F) as u32);
            (cp, cp >= 0x80)
        }
        3 => {
            let cp = (((seq[0] & 0x0F) as u32) << 12)
                | (((seq[1] & 0x3F) as u32) << 6)
                | ((seq[2] & 0x3F) as u32);
            (cp, cp >= 0x800 && !(0xD800..0xE000).contains(&cp))
        }
        4 => {
            let cp = (((seq[0] & 0x07) as u32) << 18)
                | (((seq[1] & 0x3F) as u32) << 12)
                | (((seq[2] & 0x3F) as u32) << 6)
                | ((seq[3] & 0x3F) as u32);
            (cp, cp >= 0x10000 && cp <= 0x10FFFF)
        }
        _ => (REPLACEMENT, false),
    }
}
