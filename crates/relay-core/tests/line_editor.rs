use relay_core::console::{
    TextOutput,
    line_editor::{EditAction, Key, LineEditor, redraw},
};

struct Recorder(Vec<u8>);

impl TextOutput for Recorder {
    fn write_bytes(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }
}

fn key(byte: u8) -> Key {
    Key::Printable(byte)
}

fn feed(editor: &mut LineEditor, keys: impl IntoIterator<Item = Key>) -> Vec<EditAction> {
    keys.into_iter()
        .map(|item| editor.handle_key(item))
        .collect()
}

#[test]
fn editor_inserts_and_moves_cursor() {
    let mut editor = LineEditor::new(128);
    feed(
        &mut editor,
        [key(b'a'), key(b'c'), Key::Left, key(b'b'), Key::Enter],
    );
    assert_eq!(editor.take_submitted(), Some("abc".to_string()));
    assert_eq!(editor.take_submitted(), None);
}

#[test]
fn editor_enforces_capacity_and_ignores_non_printable() {
    let mut editor = LineEditor::new(2);
    assert!(matches!(editor.handle_key(key(b'a')), EditAction::Redraw));
    assert!(matches!(editor.handle_key(key(b'b')), EditAction::Redraw));
    assert!(matches!(editor.handle_key(key(b'c')), EditAction::Ignored));
    assert!(matches!(
        editor.handle_key(Key::Printable(0x01)),
        EditAction::Ignored
    ));
    assert_eq!(editor.line(), "ab");
    assert!(matches!(
        editor.handle_key(Key::Backspace),
        EditAction::Redraw
    ));
    assert_eq!(editor.line(), "a");
    assert!(matches!(editor.handle_key(Key::Left), EditAction::Redraw));
    assert!(matches!(editor.handle_key(Key::Left), EditAction::Ignored));
    assert!(matches!(editor.handle_key(Key::Right), EditAction::Redraw));
    assert!(matches!(editor.handle_key(Key::Right), EditAction::Ignored));
    assert!(matches!(
        editor.handle_key(Key::Backspace),
        EditAction::Redraw
    ));
    assert_eq!(editor.line(), "");
    assert!(matches!(
        editor.handle_key(Key::Backspace),
        EditAction::Ignored
    ));
}

#[test]
fn redraw_writes_prompt_and_line_without_newline() {
    let mut output = Recorder(Vec::new());
    redraw(&mut output, b"$ ", "hi");
    assert_eq!(output.0, b"$ hi");
}
