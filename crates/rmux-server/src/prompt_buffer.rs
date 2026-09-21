//! The one line editor behind every prompt this daemon draws.
//!
//! Two surfaces read a keyboard and edit a line of text: rmux's own command prompt, which an
//! attached client drives through decoded key events, and the shell prompt a commandless pane
//! runs on its idle-terminal lease. They render in completely different places — a status line
//! and a pseudoterminal — but the *editing* is the same editing, and a second implementation of
//! it would be a prompt that behaves like neither.
//!
//! So the text, the cursor, the kill slot and the history cursor live here, and both prompts own
//! one of these. What stays with each prompt is what genuinely differs: which keys are accepted,
//! what a submitted line means, and how the result is painted.
//!
//! # Why the cursor is a character index
//!
//! Every editing key moves by *characters*, and a byte cursor would let Backspace cut a multi-byte
//! character in half. The byte offset is computed where it is needed — inserting and draining a
//! `String` need one — from [`byte_index_for_char`], which is the only place the two
//! representations meet.

/// The text one prompt is editing, and everything an editing key changes about it.
///
/// `buffer` and `cursor` are the line and the caret; `saved` is the single kill slot Ctrl-W fills
/// and Ctrl-Y pastes; `history_index` and `pre_history_buffer` are where a history walk stands and
/// what it will put back. Every mutating method answers whether it changed anything, so a caller
/// can decide whether a repaint is owed without comparing before and after.
#[derive(Debug, Default)]
pub(crate) struct PromptBuffer {
    /// The line as typed.
    pub(crate) buffer: String,
    /// The caret, as a count of characters from the start of [`Self::buffer`].
    pub(crate) cursor: usize,
    /// What the last word kill removed, for the paste that puts it back.
    pub(crate) saved: String,
    /// How far up the history this prompt has walked; `0` is the line being edited.
    pub(crate) history_index: usize,
    /// The line the history walk started from, restored when it walks back down to `0`.
    pub(crate) pre_history_buffer: Option<String>,
}

impl PromptBuffer {
    /// A prompt starting from `text`, with the caret at its end.
    pub(crate) fn with_text(text: String) -> Self {
        let cursor = text.chars().count();
        Self {
            buffer: text,
            cursor,
            ..Self::default()
        }
    }

    /// The line being edited.
    pub(crate) fn text(&self) -> &str {
        &self.buffer
    }

    /// The line being edited, as an owned copy for a caller that keeps it.
    pub(crate) fn buffer_string(&self) -> String {
        self.buffer.clone()
    }

    /// Whether nothing has been typed.
    pub(crate) fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// Replaces the line and puts the caret at its end.
    ///
    /// The history walk is deliberately *not* reset: this is how a history entry is installed, and
    /// resetting the index here would make the next Up start over from the newest entry.
    pub(crate) fn set_text(&mut self, value: String) {
        self.buffer = value;
        self.cursor = self.buffer.chars().count();
    }

    /// Empties the line and forgets where a history walk stood.
    ///
    /// Used where a prompt is starting over rather than editing: a cancelled line, or a line that
    /// has just been submitted.
    pub(crate) fn clear(&mut self) {
        self.buffer.clear();
        self.cursor = 0;
        self.history_index = 0;
        self.pre_history_buffer = None;
    }

    /// Inserts one character at the caret.
    pub(crate) fn push_char(&mut self, ch: char) {
        let byte = byte_index_for_char(&self.buffer, self.cursor);
        self.buffer.insert(byte, ch);
        self.cursor += 1;
        self.history_index = 0;
    }

    /// Inserts `text` at the caret, answering whether there was anything to insert.
    ///
    /// One insertion rather than one per character: a paste is a single edit, and feeding it
    /// through [`Self::push_char`] would repaint the line once per pasted byte.
    pub(crate) fn insert_text(&mut self, text: &str) -> bool {
        if text.is_empty() {
            return false;
        }
        let byte = byte_index_for_char(&self.buffer, self.cursor);
        self.buffer.insert_str(byte, text);
        self.cursor += text.chars().count();
        self.history_index = 0;
        true
    }

    /// Removes the character before the caret.
    pub(crate) fn delete_left(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let end = byte_index_for_char(&self.buffer, self.cursor);
        self.cursor -= 1;
        let start = byte_index_for_char(&self.buffer, self.cursor);
        self.buffer.drain(start..end);
        self.history_index = 0;
        true
    }

    /// Removes the character the caret is on.
    pub(crate) fn delete_at_cursor(&mut self) -> bool {
        let start = byte_index_for_char(&self.buffer, self.cursor);
        if start == self.buffer.len() {
            return false;
        }
        let end = next_char_boundary(&self.buffer, start);
        self.buffer.drain(start..end);
        self.history_index = 0;
        true
    }

    /// Moves the caret one character left.
    pub(crate) fn move_left(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.cursor -= 1;
        true
    }

    /// Moves the caret one character right.
    pub(crate) fn move_right(&mut self) -> bool {
        if self.cursor >= self.buffer.chars().count() {
            return false;
        }
        self.cursor += 1;
        true
    }

    /// Moves the caret to the start of the line.
    pub(crate) fn move_home(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.cursor = 0;
        true
    }

    /// Moves the caret to the end of the line.
    pub(crate) fn move_end(&mut self) -> bool {
        let end = self.buffer.chars().count();
        if self.cursor == end {
            return false;
        }
        self.cursor = end;
        true
    }

    /// Discards the whole line, leaving the history walk where it was.
    pub(crate) fn clear_buffer(&mut self) -> bool {
        if self.buffer.is_empty() {
            return false;
        }
        self.buffer.clear();
        self.cursor = 0;
        self.history_index = 0;
        true
    }

    /// Discards everything from the caret to the end of the line.
    pub(crate) fn delete_to_end(&mut self) -> bool {
        let start = byte_index_for_char(&self.buffer, self.cursor);
        if start == self.buffer.len() {
            return false;
        }
        self.buffer.truncate(start);
        self.history_index = 0;
        true
    }

    /// Kills the word before the caret into [`Self::saved`].
    ///
    /// A run of `separators` counts as its own word, which is what makes a second Ctrl-W over
    /// `path/to/file` take `file` and then `/`, rather than the whole path at once.
    pub(crate) fn delete_word_left(&mut self, separators: &str) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let chars = self.buffer.chars().collect::<Vec<_>>();
        let mut index = self.cursor;

        while index > 0 && chars[index - 1].is_whitespace() {
            index -= 1;
        }
        let separator_word = index > 0 && separators.contains(chars[index - 1]);
        while index > 0 {
            let ch = chars[index - 1];
            if ch.is_whitespace() || separator_word != separators.contains(ch) {
                break;
            }
            index -= 1;
        }

        let start = byte_index_for_char(&self.buffer, index);
        let end = byte_index_for_char(&self.buffer, self.cursor);
        self.saved = self.buffer[start..end].to_owned();
        self.buffer.drain(start..end);
        self.cursor = index;
        self.history_index = 0;
        true
    }

    /// Puts the last killed word back at the caret.
    pub(crate) fn paste_saved(&mut self) -> bool {
        if self.saved.is_empty() {
            return false;
        }
        let byte = byte_index_for_char(&self.buffer, self.cursor);
        self.buffer.insert_str(byte, &self.saved);
        self.cursor += self.saved.chars().count();
        self.history_index = 0;
        true
    }

    /// Walks one entry further back through `entries`.
    ///
    /// The line being edited is saved on the first step, so walking back down to the bottom
    /// restores what was typed rather than an empty line.
    pub(crate) fn history_up(&mut self, entries: &[String]) -> bool {
        if self.history_index == 0 {
            self.pre_history_buffer = Some(self.buffer.clone());
        }
        match history_up(entries, &mut self.history_index) {
            Some(value) => {
                self.set_text(value);
                true
            }
            None => false,
        }
    }

    /// Walks one entry back towards the line that started the walk.
    pub(crate) fn history_down(&mut self, entries: &[String]) -> bool {
        match history_down(entries, &mut self.history_index) {
            Some(value) => {
                let restored = if self.history_index == 0 {
                    self.pre_history_buffer.take().unwrap_or(value)
                } else {
                    value
                };
                self.set_text(restored);
                true
            }
            None => false,
        }
    }
}

/// Appends `line` to `entries`, bounded by `limit`, skipping an immediate repeat.
///
/// A `limit` of zero is a configured request for no history at all, so it empties what is there
/// rather than quietly keeping the last entries a larger limit had collected.
pub(crate) fn history_push(entries: &mut Vec<String>, line: &str, limit: usize) {
    if entries.last().is_some_and(|existing| existing == line) {
        return;
    }

    if limit == 0 {
        entries.clear();
        return;
    }

    entries.push(line.to_owned());
    if entries.len() > limit {
        let excess = entries.len() - limit;
        entries.drain(..excess);
    }
}

/// The entry one step further back from `index`, or `None` at the oldest entry.
///
/// `index` counts back from the end, `0` meaning the line being edited rather than any entry.
pub(crate) fn history_up(entries: &[String], index: &mut usize) -> Option<String> {
    if entries.is_empty() || *index == entries.len() {
        return None;
    }
    *index += 1;
    entries.get(entries.len().saturating_sub(*index)).cloned()
}

/// The entry one step forward from `index`, or `None` when already at the line being edited.
///
/// Reaching `0` answers an empty line: the caller owns whatever was typed before the walk began
/// and puts that back instead.
pub(crate) fn history_down(entries: &[String], index: &mut usize) -> Option<String> {
    if entries.is_empty() || *index == 0 {
        return None;
    }
    *index -= 1;
    if *index == 0 {
        return Some(String::new());
    }
    entries.get(entries.len().saturating_sub(*index)).cloned()
}

/// The byte offset of character `index`, or the length when the line is shorter than that.
pub(crate) fn byte_index_for_char(value: &str, index: usize) -> usize {
    value
        .char_indices()
        .nth(index)
        .map_or(value.len(), |(byte, _)| byte)
}

/// The byte offset of the character after the one starting at `index`.
fn next_char_boundary(value: &str, index: usize) -> usize {
    value[index..]
        .char_indices()
        .nth(1)
        .map_or(value.len(), |(next, _)| index + next)
}
