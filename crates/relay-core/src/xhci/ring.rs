use super::XhciError;
use alloc::vec::Vec;

pub const RING_TRBS: usize = 64;

pub struct Ring {
    base: u64,
    entries: Vec<[u8; 16]>,
    enqueue: usize,
    dequeue: usize,
    producer_cycle: bool,
    consumer_cycle: bool,
    live: usize,
}

impl Ring {
    pub fn new(base: u64) -> Self {
        Self {
            base,
            entries: alloc::vec![[0; 16]; RING_TRBS],
            enqueue: 0,
            dequeue: 0,
            producer_cycle: true,
            consumer_cycle: true,
            live: 0,
        }
    }

    pub fn push(&mut self, mut trb: [u8; 16]) -> Result<u64, XhciError> {
        if self.live >= RING_TRBS - 1 {
            return Err(XhciError::Allocation);
        }
        if self.enqueue == RING_TRBS - 1 {
            let link = super::encode_link(self.base, true, !self.producer_cycle as u8);
            self.entries[self.enqueue] = link;
            self.enqueue = 0;
            self.producer_cycle = !self.producer_cycle;
        }
        trb[15] = (trb[15] & 0xFE) | (self.producer_cycle as u8);
        let phys = self.base + self.enqueue as u64 * 16;
        self.entries[self.enqueue] = trb;
        self.enqueue += 1;
        self.live += 1;
        Ok(phys)
    }

    pub fn is_full(&self) -> bool {
        self.live >= RING_TRBS - 1
    }

    pub fn live_count(&self) -> usize {
        self.live
    }

    pub fn enqueue_index(&self) -> usize {
        self.enqueue
    }

    pub fn producer_cycle(&self) -> bool {
        self.producer_cycle
    }

    pub fn pop_for_test(&mut self) {
        if self.live == 0 {
            return;
        }
        self.live -= 1;
        self.dequeue = (self.dequeue + 1) % (RING_TRBS - 1);
        if self.dequeue == 0 {
            self.consumer_cycle = !self.consumer_cycle;
        }
        if self.enqueue == RING_TRBS - 1 {
            self.enqueue = 0;
            self.producer_cycle = !self.producer_cycle;
        }
        if self.live <= 1 {
            self.enqueue = 0;
            self.producer_cycle = false;
        }
    }

    pub fn consume_ready_for_test(&mut self, invert: bool) -> Option<[u8; 16]> {
        let cycle = if invert {
            !self.consumer_cycle
        } else {
            self.consumer_cycle
        };
        let entry = self.entries[self.dequeue];
        if (entry[15] & 0x01) != (cycle as u8) {
            return None;
        }
        Some(entry)
    }

    pub fn dequeue_phys_for_test(&self) -> u64 {
        self.base + self.dequeue as u64 * 16
    }

    pub fn advance_for_test(&mut self) {
        self.dequeue = (self.dequeue + 1) % (RING_TRBS - 1);
    }
}
