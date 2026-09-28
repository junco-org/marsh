use std::io::IsTerminal;

use brush_core::Shell;

use crate::{
    Completions, InputBackend, ShellError, completion,
    input_backend::{InteractivePrompt, ReadResult},
};

use super::{non_term_line_reader, term_line_reader};

/// Represents a basic shell input backend capable of interactive usage, with primitive support
/// for completion and test-focused automation via pexpect and similar technologies.
#[derive(Default)]
pub struct BasicInputBackend;

impl InputBackend for BasicInputBackend {
    fn read_line(
        &mut self,
        shell: &crate::ShellRef<impl brush_core::ShellExtensions>,
        prompt: InteractivePrompt,
    ) -> Result<ReadResult, ShellError> {
        self.read_line_with_completion(shell, &prompt, |shell, line, cursor| {
            Self::generate_completions(shell, line, cursor)
        })
    }
}

impl BasicInputBackend {
    /// Reads a line of input exactly like [`InputBackend::read_line`], except that every
    /// completion request the editor makes is answered by `complete` rather than by
    /// [`Self::generate_completions`].
    ///
    /// `complete` receives the locked shell, the current line buffer and the cursor position; its
    /// error is returned from this call like any other input failure.
    ///
    /// # Arguments
    ///
    /// * `shell` - The shell instance for which input is being read.
    /// * `prompt` - The prompt to display to the user.
    /// * `complete` - Produces the completions for one request.
    pub fn read_line_with_completion<SE, F>(
        &mut self,
        shell: &crate::ShellRef<SE>,
        prompt: &InteractivePrompt,
        complete: F,
    ) -> Result<ReadResult, ShellError>
    where
        SE: brush_core::ShellExtensions,
        F: FnMut(&mut Shell<SE>, &str, usize) -> Result<Completions, ShellError>,
    {
        if std::io::stdin().is_terminal() {
            self.read_line_via(
                shell,
                &term_line_reader::TermLineReader::new()?,
                prompt,
                complete,
            )
        } else {
            self.read_line_via(
                shell,
                &non_term_line_reader::NonTermLineReader,
                prompt,
                complete,
            )
        }
    }

    fn read_line_via<R: super::LineReader, SE: brush_core::ShellExtensions, F>(
        &self,
        shell_ref: &crate::ShellRef<SE>,
        reader: &R,
        prompt: &InteractivePrompt,
        mut complete: F,
    ) -> Result<ReadResult, ShellError>
    where
        F: FnMut(&mut Shell<SE>, &str, usize) -> Result<Completions, ShellError>,
    {
        let mut prompt_to_use = self.should_display_prompt().then_some(&prompt);
        let mut result = String::new();

        loop {
            match reader.read_line(prompt_to_use.map(|p| p.prompt.as_str()), |line, cursor| {
                let mut shell = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(shell_ref.lock())
                });

                complete(&mut shell, line, cursor)
            })? {
                ReadResult::Input(s) => {
                    result.push_str(s.as_str());

                    let shell = tokio::task::block_in_place(|| {
                        tokio::runtime::Handle::current().block_on(shell_ref.lock())
                    });

                    if Self::is_valid_input(&shell, result.as_str()) {
                        break;
                    }

                    prompt_to_use = None;
                }
                ReadResult::BoundCommand(s) => {
                    result.push_str(s.as_str());
                    break;
                }
                ReadResult::Eof => {
                    if result.is_empty() {
                        return Ok(ReadResult::Eof);
                    }
                    break;
                }
                ReadResult::Interrupted => return Ok(ReadResult::Interrupted),
            }
        }

        Ok(ReadResult::Input(result))
    }

    #[expect(clippy::unused_self)]
    fn should_display_prompt(&self) -> bool {
        std::io::stdin().is_terminal()
    }

    fn is_valid_input(shell: &Shell<impl brush_core::ShellExtensions>, input: &str) -> bool {
        match shell.parse_string(input.to_owned()) {
            // Incomplete tokenizing (unclosed quotes, etc.) - need more input
            Err(brush_parser::ParseError::Tokenizing { inner, position: _ })
                if inner.is_incomplete() =>
            {
                false
            }
            // Parse error at end of input - could be incomplete
            Err(brush_parser::ParseError::ParsingAtEndOfInput) => false,
            // Parse error at a specific position OR successful parse - complete
            _ => true,
        }
    }

    /// Generates the completions the basic editor offers for `line` with the cursor at `cursor`.
    ///
    /// # Arguments
    ///
    /// * `shell` - The shell whose completion configuration answers the request.
    /// * `line` - The current line buffer.
    /// * `cursor` - The cursor position within `line`.
    pub fn generate_completions(
        shell: &mut Shell<impl brush_core::ShellExtensions>,
        line: &str,
        cursor: usize,
    ) -> Result<Completions, ShellError> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(Self::generate_completions_async(shell, line, cursor))
        })
    }

    async fn generate_completions_async(
        shell: &mut Shell<impl brush_core::ShellExtensions>,
        line: &str,
        cursor: usize,
    ) -> Result<Completions, ShellError> {
        Ok(completion::complete_async(shell, line, cursor).await)
    }
}
