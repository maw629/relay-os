use relay_core::block::{BlockDevice, BlockError, BlockGeometry};
use std::cell::Cell;
use std::rc::Rc;

pub struct FaultDevice<D> {
    inner: D,
    writes: Rc<Cell<usize>>,
    flushes: Rc<Cell<usize>>,
    fail_write: Option<usize>,
    fail_flush: Option<usize>,
}

impl<D> FaultDevice<D> {
    pub fn fail_write(inner: D, index: usize) -> Self {
        Self {
            inner,
            writes: Rc::new(Cell::new(0)),
            flushes: Rc::new(Cell::new(0)),
            fail_write: Some(index),
            fail_flush: None,
        }
    }

    pub fn fail_flush(inner: D, index: usize) -> Self {
        Self {
            inner,
            writes: Rc::new(Cell::new(0)),
            flushes: Rc::new(Cell::new(0)),
            fail_write: None,
            fail_flush: Some(index),
        }
    }

    pub fn write_count(&self) -> usize {
        self.writes.get()
    }

    pub fn flush_count(&self) -> usize {
        self.flushes.get()
    }
}

impl<D: BlockDevice> BlockDevice for FaultDevice<D> {
    fn geometry(&self) -> BlockGeometry {
        self.inner.geometry()
    }

    fn read_sectors(&mut self, first_lba: u64, dst: &mut [u8]) -> Result<(), BlockError> {
        self.inner.read_sectors(first_lba, dst)
    }

    fn write_sectors(&mut self, first_lba: u64, src: &[u8]) -> Result<(), BlockError> {
        // Indices are zero-based: the first mutating call after construction
        // is index 0. A read-write mount consumes write index 0 when it
        // clears the dirty state, so faulting index 1 fails the first
        // mutation write (see the `ext2_files` poison test).
        let index = self.writes.get();
        self.writes.set(index + 1);
        if self.fail_write == Some(index) {
            return Err(BlockError::Transport);
        }
        self.inner.write_sectors(first_lba, src)
    }

    fn flush(&mut self) -> Result<(), BlockError> {
        let index = self.flushes.get();
        self.flushes.set(index + 1);
        if self.fail_flush == Some(index) {
            return Err(BlockError::Flush);
        }
        self.inner.flush()
    }
}
