use alloc::{string::String, vec::Vec};

use super::TextOutput;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Key {
    Printable(u8),
    Backspace,
    Enter,
    Left,
    Right,
}

#[derive(Debug, Eq, PartialEq)]
pub enum EditAction {
    Redraw,
    Submitted(String),
    Ignored,
}

pub struct LineEditor {
    buffer: Vec<u8>,
    cursor: usize,
    capacity: usize,
    submitted: Option<String>,
}

impl LineEditor {
    pub fn new(capacity: usize) -> Self {
        Self {
            buffer: Vec::new(),
            cursor: 0,
            capacity,
            submitted: None,
        }
    }

    pub fn handle_key(&mut self, key: Key) -> EditAction {
        match key {
            Key::Printable(byte) => {
                if !matches!(byte, 0x20..=0x7E) || self.buffer.len() >= self.capacity {
                    return EditAction::Ignored;
                }
                if self.buffer.try_reserve(1).is_err() {
                    return EditAction::Ignored;
                }
                self.buffer.insert(self.cursor, byte);
                self.cursor += 1;
                EditAction::Redraw
            }
            Key::Backspace => {
                if self.cursor == 0 {
                    EditAction::Ignored
                } else {
                    self.cursor -= 1;
                    self.buffer.remove(self.cursor);
                    EditAction::Redraw
                }
            }
            Key::Left => {
                if self.cursor == 0 {
                    EditAction::Ignored
                } else {
                    self.cursor -= 1;
                    EditAction::Redraw
                }
            }
            Key::Right => {
                if self.cursor == self.buffer.len() {
                    EditAction::Ignored
                } else {
                    self.cursor += 1;
                    EditAction::Redraw
                }
            }
            Key::Enter => {
                let text = String::from_utf8(core::mem::take(&mut self.buffer))
                    .expect("editor stores ASCII only");
                self.cursor = 0;
                self.submitted = Some(text.clone());
                EditAction::Submitted(text)
            }
        }
    }

    pub fn line(&self) -> &str {
        core::str::from_utf8(&self.buffer).expect("editor stores ASCII only")
    }

    pub fn take_submitted(&mut self) -> Option<String> {
        self.submitted.take()
    }
}

pub fn redraw<O: TextOutput>(output: &mut O, prompt: &[u8], line: &str) {
    output.write_bytes(prompt);
    output.write_bytes(line.as_bytes());
}
