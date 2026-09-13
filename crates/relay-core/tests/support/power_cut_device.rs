use relay_core::block::{BlockDevice, BlockError, BlockGeometry};
use std::collections::BTreeMap;

pub struct PowerCutDevice<D> {
    inner: D,
    overlay: BTreeMap<u64, Vec<u8>>,
}

impl<D> PowerCutDevice<D> {
    pub fn new(inner: D) -> Self {
        Self {
            inner,
            overlay: BTreeMap::new(),
        }
    }

    pub fn power_cut(&mut self) {
        self.overlay.clear();
    }
}

impl<D: BlockDevice> BlockDevice for PowerCutDevice<D> {
    fn geometry(&self) -> BlockGeometry {
        self.inner.geometry()
    }

    fn read_sectors(&mut self, first_lba: u64, dst: &mut [u8]) -> Result<(), BlockError> {
        let geometry = self.inner.geometry();
        validate_request(geometry, first_lba, dst.len())?;
        let sector_size = usize::try_from(geometry.logical_sector_size)
            .map_err(|_| BlockError::InvalidRequest)?;
        let sectors = dst.len() / sector_size;
        let mut assembled = Vec::new();
        assembled
            .try_reserve_exact(dst.len())
            .map_err(|_| BlockError::Transport)?;
        assembled.resize(dst.len(), 0);
        for index in 0..sectors {
            let lba = first_lba
                .checked_add(index as u64)
                .ok_or(BlockError::OutOfRange)?;
            let Some(sector) = sector_from_overlay(&self.overlay, sector_size, lba) else {
                return self.inner.read_sectors(first_lba, dst);
            };
            let start = index * sector_size;
            assembled[start..start + sector_size].copy_from_slice(sector);
        }
        dst.copy_from_slice(&assembled);
        Ok(())
    }

    fn write_sectors(&mut self, first_lba: u64, src: &[u8]) -> Result<(), BlockError> {
        validate_request(self.inner.geometry(), first_lba, src.len())?;
        self.overlay.insert(first_lba, src.to_vec());
        Ok(())
    }

    fn flush(&mut self) -> Result<(), BlockError> {
        for (first_lba, bytes) in &self.overlay {
            self.inner.write_sectors(*first_lba, bytes)?;
        }
        self.inner.flush()?;
        self.overlay.clear();
        Ok(())
    }
}

fn validate_request(
    geometry: BlockGeometry,
    first_lba: u64,
    bytes: usize,
) -> Result<(), BlockError> {
    let sector_size =
        usize::try_from(geometry.logical_sector_size).map_err(|_| BlockError::InvalidRequest)?;
    if sector_size == 0 || bytes == 0 || !bytes.is_multiple_of(sector_size) {
        return Err(BlockError::InvalidRequest);
    }
    let count = u64::try_from(bytes / sector_size).map_err(|_| BlockError::InvalidRequest)?;
    let end_lba = first_lba.checked_add(count).ok_or(BlockError::OutOfRange)?;
    if end_lba > geometry.sector_count {
        return Err(BlockError::OutOfRange);
    }
    Ok(())
}

fn sector_from_overlay(
    overlay: &BTreeMap<u64, Vec<u8>>,
    sector_size: usize,
    lba: u64,
) -> Option<&[u8]> {
    // The entry with the greatest starting LBA at or below the wanted sector
    // wins when entries overlap.
    let (start, bytes) = overlay.range(..=lba).next_back()?;
    let offset = usize::try_from(lba - start)
        .ok()?
        .checked_mul(sector_size)?;
    let end = offset.checked_add(sector_size)?;
    bytes.get(offset..end)
}
