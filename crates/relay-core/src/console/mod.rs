pub mod line_editor;

pub use line_editor::{EditAction, Key, LineEditor};
mod framebuffer;

pub use framebuffer::{
    Console, Framebuffer, FramebufferConsole, FramebufferInfo, PixelFormat, SliceFramebuffer,
};

pub trait TextOutput {
    fn write_bytes(&mut self, bytes: &[u8]);
}

pub struct Mirror<A, B> {
    pub primary: A,
    pub diagnostic: B,
}

impl<A: TextOutput, B: TextOutput> TextOutput for Mirror<A, B> {
    fn write_bytes(&mut self, bytes: &[u8]) {
        self.primary.write_bytes(bytes);
        self.diagnostic.write_bytes(bytes);
    }
}
