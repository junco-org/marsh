//! What a keystroke, a split character and a pasted newline do to the line being typed.
//!
//! The editing half of the prompt is a pure function of the bytes that arrived, which is why it
//! is a separate type: these cases are the ones a pseudoterminal would only make slower and less
//! deterministic to observe.

use marsh_core::shellmux::repl;
use rmux_core::Utf8Config;

use super::{Action, Advance, Editing, caret, kill_line};

/// What the prompt would have done with a finished line.
#[derive(Debug, PartialEq, Eq)]
enum Submitted {
    /// A line was submitted, with the text the prompt would resolve.
    Line(String),
    /// Ctrl-C abandoned the line.
    Cancel,
    /// Ctrl-D ended input.
    Eof,
}

/// Feeds `bytes` in as one read and runs the loop the terminal half runs.
///
/// Every line is treated as complete, which is what `input_is_complete` answers for anything that
/// is not an unfinished construct; the continuation path is exercised on its own below.
fn feed(line: &mut Editing, bytes: &[u8]) -> Vec<Submitted> {
    line.pending.extend_from_slice(bytes);
    let mut submitted = Vec::new();
    loop {
        let step = line.advance();
        match step.action {
            None => return submitted,
            Some(Action::Submit) => submitted.push(Submitted::Line(line.take_submitted())),
            Some(Action::Cancel) => {
                line.cancel();
                submitted.push(Submitted::Cancel);
            }
            Some(Action::Eof) => {
                submitted.push(Submitted::Eof);
                return submitted;
            }
        }
    }
}

#[test]
fn a_character_split_across_two_reads_reassembles() {
    let mut line = Editing::default();

    // The lease is explicitly allowed to cut a read anywhere, including the middle of a
    // three-byte character.
    line.pending.extend_from_slice(&[0xe6, 0x97]);
    assert_eq!(
        line.advance(),
        Advance {
            redraw: false,
            action: None
        }
    );
    assert_eq!(
        line.editor.text(),
        "",
        "half a character must not reach the line"
    );
    assert_eq!(
        line.pending,
        vec![0xe6, 0x97],
        "the partial character is retained for the next read"
    );

    line.pending.push(0xa5);
    let step = line.advance();
    assert!(step.redraw);
    assert_eq!(line.editor.text(), "日");
    assert!(line.pending.is_empty());
}

#[test]
fn a_pasted_newline_is_inserted_rather_than_executed() {
    let mut line = Editing::default();

    let submitted = feed(&mut line, b"\x1b[200~echo a\necho b\x1b[201~");
    assert_eq!(
        submitted,
        Vec::new(),
        "a newline inside a paste must not run half the paste"
    );
    assert_eq!(line.editor.text(), "echo a\necho b");

    // The Enter that follows is the user's, and submits the whole pasted block at once.
    assert_eq!(
        feed(&mut line, b"\r"),
        vec![Submitted::Line("echo a\necho b".to_owned())]
    );
}

#[test]
fn a_paste_end_marker_split_across_reads_is_not_pasted_as_text() {
    let mut line = Editing::default();

    feed(&mut line, b"\x1b[200~ab\x1b[2");
    assert_eq!(
        line.editor.text(),
        "",
        "the body waits for a marker that may still complete"
    );

    feed(&mut line, b"01~");
    assert_eq!(line.editor.text(), "ab");
    assert!(line.pending.is_empty());
}

#[test]
fn editing_keys_and_history_run_through_the_shared_buffer() {
    let mut line = Editing::default();

    feed(&mut line, "echo ñ日本".as_bytes());
    assert_eq!(line.editor.cursor, 8);

    feed(&mut line, b"\x1b[D\x1b[D");
    assert_eq!(line.editor.cursor, 6);
    feed(&mut line, b"\x1b[3~");
    assert_eq!(line.editor.text(), "echo ñ本");
    feed(&mut line, b"\x08");
    assert_eq!(line.editor.text(), "echo 本");
    feed(&mut line, b"\x01");
    assert_eq!(line.editor.cursor, 0);
    feed(&mut line, b"\x0b");
    assert_eq!(line.editor.text(), "");

    // The kill slot Ctrl-W fills and Ctrl-Y empties is the command prompt's kill slot.
    feed(&mut line, b"make lib/thing");
    feed(&mut line, b"\x17");
    assert_eq!(line.editor.text(), "make lib/");
    assert_eq!(line.editor.saved, "thing");
    feed(&mut line, b"\x19");
    assert_eq!(line.editor.text(), "make lib/thing");

    // And the history walk is the same walk, including putting back what was being typed.
    line.editor.clear();
    line.history = vec!["first".to_owned(), "second".to_owned()];
    feed(&mut line, b"half");
    feed(&mut line, b"\x1b[A");
    assert_eq!(line.editor.text(), "second");
    feed(&mut line, b"\x1b[A");
    assert_eq!(line.editor.text(), "first");
    feed(&mut line, b"\x1b[B\x1b[B");
    assert_eq!(
        line.editor.text(),
        "half",
        "walking back down restores the line the walk started from"
    );
}

#[test]
fn ctrl_d_deletes_at_the_cursor_unless_the_line_is_empty() {
    let mut line = Editing::default();

    feed(&mut line, b"abc\x01");
    assert_eq!(
        feed(&mut line, b"\x04"),
        Vec::new(),
        "Ctrl-D on a typed line is a forward delete, not an end of input"
    );
    assert_eq!(line.editor.text(), "bc");

    // An unfinished construct is not an empty line either: closing here would lose it.
    line.editor.clear();
    line.continuation = "for i in 1 2 3\n".to_owned();
    assert_eq!(feed(&mut line, b"\x04"), Vec::new());
    assert_eq!(line.continuation, "for i in 1 2 3\n");

    line.continuation.clear();
    assert_eq!(feed(&mut line, b"\x04"), vec![Submitted::Eof]);
}

#[test]
fn ctrl_c_abandons_the_line_and_the_construct_above_it() {
    let mut line = Editing {
        continuation: "for i in 1 2 3\n".to_owned(),
        ..Default::default()
    };
    feed(&mut line, b"  do echo");

    assert_eq!(feed(&mut line, b"\x03"), vec![Submitted::Cancel]);
    assert_eq!(line.editor.text(), "");
    assert_eq!(line.continuation, "");
}

#[test]
fn input_after_a_submitted_line_is_handed_over_exactly_once() {
    let mut line = Editing::default();
    line.pending.extend_from_slice(b"echo a\recho b\r");

    let step = line.advance();
    assert_eq!(step.action, Some(Action::Submit));
    assert_eq!(line.take_submitted(), "echo a");

    // What arrived after the Enter, and none of what came before it.
    let suffix = std::mem::take(&mut line.pending);
    assert_eq!(suffix, b"echo b\r".to_vec());

    // Taken, so there is nothing left for a second hand-over.
    assert_eq!(
        line.advance(),
        Advance {
            redraw: false,
            action: None
        }
    );
    assert_eq!(line.editor.text(), "");

    // Fed back once the command has been reserved, it is the next line and runs once.
    line.pending = suffix;
    assert_eq!(line.advance().action, Some(Action::Submit));
    assert_eq!(line.take_submitted(), "echo b");
    assert!(line.pending.is_empty());
}

#[test]
fn an_unfinished_line_is_continued_as_it_was_typed() {
    // The parser trims, and the trimmed text is what runs — but it is not what the *next* line
    // is appended to, because the spaces are inside the quote.
    assert_eq!(
        repl::parse("echo 'a  "),
        repl::Input::Foreground("echo 'a".to_owned())
    );

    let mut line = Editing::default();
    line.pending.extend_from_slice(b"echo 'a  \r");
    assert_eq!(line.advance().action, Some(Action::Submit));

    let full = line.take_submitted();
    assert_eq!(full, "echo 'a  ");
    line.continue_with(full);

    line.pending.extend_from_slice(b"b'\r");
    assert_eq!(line.advance().action, Some(Action::Submit));
    assert_eq!(line.take_submitted(), "echo 'a  \nb'");
    assert_eq!(line.continuation, "");
}

#[test]
fn a_lone_escape_is_left_for_the_escape_time_timer() {
    let mut line = Editing::default();
    line.pending.push(0x1b);

    assert_eq!(
        line.advance(),
        Advance {
            redraw: false,
            action: None
        }
    );
    assert_eq!(
        line.pending,
        vec![0x1b],
        "an Escape that may still be the head of a sequence is retained"
    );

    // Completed before the timer expires, it is the arrow key it turned out to be.
    line.pending.extend_from_slice(b"[C");
    line.advance();
    assert!(line.pending.is_empty());
}

#[test]
fn kill_becomes_a_quoted_managed_invocation() {
    assert_eq!(
        kill_line(&["-9".to_owned(), "12 34".to_owned()]),
        "kill '-9' '12 34'",
        "each argument is quoted on its own so the shell does not re-split it"
    );
    assert_eq!(kill_line(&[]), "kill");
}

#[test]
fn the_caret_lands_where_the_line_puts_it() {
    let utf8 = Utf8Config::default();

    assert_eq!(caret("%main .> ", "ab", 1, &utf8), (0, 10));
    // A wide character is two cells, not two characters.
    assert_eq!(caret("> ", "日x", 1, &utf8), (0, 4));
    // A pasted newline moves the caret onto its own row, with no prompt indent.
    assert_eq!(caret("%main .> ", "ab\ncd", 4, &utf8), (1, 1));
}
