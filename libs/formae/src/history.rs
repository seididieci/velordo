//! `History<N>` — una serie storica ad anello, a capacita' fissa.
//!
//! Sopravvive ai resize (il campione piu' vecchio esce, il resto resta) e non
//! alloca: e' un array che gira. Il grafico vuole i campioni dal piu' vecchio
//! al piu' recente, quindi si leggono in un buffer del chiamante (`read`).

/// Serie storica ad anello.
pub struct History<const N: usize> {
    buf: [u64; N],
    len: usize,
    /// Indice del prossimo campione da scrivere.
    head: usize,
}

impl<const N: usize> History<N> {
    pub const fn new() -> History<N> {
        History {
            buf: [0; N],
            len: 0,
            head: 0,
        }
    }

    /// Aggiunge un campione (a capacita' piena scarta il piu' vecchio).
    pub fn push(&mut self, v: u64) {
        if N == 0 {
            return;
        }
        self.buf[self.head] = v;
        self.head = (self.head + 1) % N;
        if self.len < N {
            self.len += 1;
        }
    }

    /// Riempie la storia con `v` (grafico piatto all'avvio, niente buco).
    pub fn fill(&mut self, v: u64) {
        for _ in 0..N {
            self.push(v);
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Il campione piu' recente.
    pub fn last(&self) -> Option<u64> {
        if self.len == 0 || N == 0 {
            return None;
        }
        Some(self.buf[(self.head + N - 1) % N])
    }

    /// Il massimo (0 se vuota).
    pub fn max(&self) -> u64 {
        let mut m = 0;
        for i in 0..self.len {
            let v = self.buf[(self.head + N - self.len + i) % N];
            if v > m {
                m = v;
            }
        }
        m
    }

    /// Copia i campioni dal **piu' vecchio** al **piu' recente** in `out`
    /// (troncato a `out.len()`) e ritorna quanti ne ha scritti.
    pub fn read(&self, out: &mut [u64]) -> usize {
        let n = self.len.min(out.len());
        if n == 0 || N == 0 {
            return 0;
        }
        // I piu' recenti sono gli ultimi `n` della storia.
        let first = (self.head + N - self.len) % N;
        let skip = self.len - n;
        for i in 0..n {
            out[i] = self.buf[(first + skip + i) % N];
        }
        n
    }
}

impl<const N: usize> Default for History<N> {
    fn default() -> Self {
        History::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anello_e_lettura_in_ordine() {
        let mut h: History<4> = History::new();
        assert_eq!(h.len(), 0);
        assert_eq!(h.last(), None);
        h.push(10);
        h.push(20);
        h.push(30);
        assert_eq!(h.len(), 3);
        assert_eq!(h.last(), Some(30));
        let mut out = [0u64; 4];
        assert_eq!(h.read(&mut out), 3);
        assert_eq!(&out[..3], &[10, 20, 30]);
        h.push(40);
        h.push(50);
        // Pieno: escono 10 e 20.
        assert_eq!(h.max(), 50);
        assert_eq!(h.read(&mut out), 4);
        assert_eq!(out, [20, 30, 40, 50]);
    }

    #[test]
    fn fill_e_buffer_corto() {
        let mut h: History<3> = History::new();
        h.fill(7);
        let mut out = [0u64; 8];
        assert_eq!(h.read(&mut out), 3);
        assert_eq!(&out[..3], &[7, 7, 7]);
        // Un buffer piu' corto prende i campioni **recenti**.
        let mut two = [0u64; 2];
        h.push(1);
        assert_eq!(h.read(&mut two), 2);
        assert_eq!(two, [7, 1]);
        // `History<0>` non esplode.
        let mut z: History<0> = History::new();
        z.push(1);
        assert_eq!(z.len(), 0);
        assert_eq!(z.read(&mut out), 0);
    }
}
