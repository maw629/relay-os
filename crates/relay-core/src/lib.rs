#![no_std]

extern crate alloc;

#[cfg(test)]
extern crate std;

pub mod acpi;
pub mod block;
pub mod console;
pub mod ext2;
pub mod fs;
pub mod gpt;
pub mod memory;
pub mod pci;
pub mod shell;
pub mod vfs;
