use relay_core::acpi::{MemoryError, PhysicalMemory};

pub struct VecMemory {
    base: u64,
    bytes: Vec<u8>,
}

impl VecMemory {
    pub fn new(base: u64, bytes: Vec<u8>) -> Self {
        Self { base, bytes }
    }

    pub fn write_at(&mut self, physical: u64, data: &[u8]) {
        let offset = (physical - self.base) as usize;
        self.bytes[offset..offset + data.len()].copy_from_slice(data);
    }
}

impl PhysicalMemory for VecMemory {
    fn read_exact(&self, physical: u64, output: &mut [u8]) -> Result<(), MemoryError> {
        let offset = physical
            .checked_sub(self.base)
            .ok_or(MemoryError::OutOfRange)? as usize;
        let end = offset
            .checked_add(output.len())
            .ok_or(MemoryError::Overflow)?;
        let src = self.bytes.get(offset..end).ok_or(MemoryError::OutOfRange)?;
        output.copy_from_slice(src);
        Ok(())
    }
}
