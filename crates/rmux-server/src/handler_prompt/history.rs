use super::PromptType;
use crate::prompt_buffer;

#[derive(Debug, Default)]
pub(in super::super) struct PromptHistoryStore {
    items: [Vec<String>; 4],
}

impl PromptHistoryStore {
    /// The entries recorded for `prompt_type`, oldest first.
    ///
    /// This is what a [`PromptBuffer`](crate::prompt_buffer::PromptBuffer) walks: the store keeps
    /// one list per prompt type, and the walk itself belongs to whichever line is being edited.
    pub(super) fn entries(&self, prompt_type: PromptType) -> &[String] {
        &self.items[prompt_type.index()]
    }

    pub(super) fn push(&mut self, prompt_type: PromptType, line: &str, limit: usize) {
        prompt_buffer::history_push(&mut self.items[prompt_type.index()], line, limit);
    }

    /// Renders the history for the selected prompt type (or every type if `None`) using
    /// the tmux `show-prompt-history` output format.
    ///
    /// The exact shape of each section matches tmux's `cmdq_print` calls in
    /// `cmd-show-prompt-history.c`: a `History for <label>:` heading, a blank line,
    /// the `index: entry` rows, and a trailing blank line.
    pub(super) fn render(&self, selected: Option<PromptType>) -> String {
        use std::fmt::Write as _;

        let mut output = String::new();
        let types = selected
            .as_ref()
            .map(std::slice::from_ref)
            .unwrap_or(PromptType::ALL.as_slice());
        for prompt_type in types {
            let _ = writeln!(output, "History for {}:\n", prompt_type.label());
            for (idx, entry) in self.items[prompt_type.index()].iter().enumerate() {
                let _ = writeln!(output, "{}: {entry}", idx + 1);
            }
            output.push('\n');
        }
        output
    }

    /// Clears history for the selected prompt type (or every type if `None`).
    pub(super) fn clear(&mut self, selected: Option<PromptType>) {
        match selected {
            Some(prompt_type) => self.items[prompt_type.index()].clear(),
            None => self.items.iter_mut().for_each(Vec::clear),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Walks `store`'s entries back one step, the way an editing prompt walks them.
    fn up(store: &PromptHistoryStore, prompt_type: PromptType, index: &mut usize) -> Option<String> {
        prompt_buffer::history_up(store.entries(prompt_type), index)
    }

    /// Walks them forward one step.
    fn down(
        store: &PromptHistoryStore,
        prompt_type: PromptType,
        index: &mut usize,
    ) -> Option<String> {
        prompt_buffer::history_down(store.entries(prompt_type), index)
    }

    #[test]
    fn history_up_down_restores_original_buffer() {
        let mut store = PromptHistoryStore::default();
        store.push(PromptType::Command, "old-cmd", 100);

        let mut index = 0usize;
        let up = up(&store, PromptType::Command, &mut index);
        assert_eq!(up, Some("old-cmd".to_owned()));
        assert_eq!(index, 1);

        let down = down(&store, PromptType::Command, &mut index);
        assert_eq!(down, Some(String::new()));
        assert_eq!(index, 0);
    }

    #[test]
    fn history_push_deduplicates_consecutive() {
        let mut store = PromptHistoryStore::default();
        store.push(PromptType::Command, "abc", 100);
        store.push(PromptType::Command, "abc", 100);
        store.push(PromptType::Command, "abc", 100);

        let mut index = 0;
        assert!(up(&store, PromptType::Command, &mut index).is_some());
        assert!(up(&store, PromptType::Command, &mut index).is_none());
    }

    #[test]
    fn history_respects_limit() {
        let mut store = PromptHistoryStore::default();
        store.push(PromptType::Command, "a", 2);
        store.push(PromptType::Command, "b", 2);
        store.push(PromptType::Command, "c", 2);

        let mut index = 0;
        let first = up(&store, PromptType::Command, &mut index);
        assert_eq!(first, Some("c".to_owned()));
        let second = up(&store, PromptType::Command, &mut index);
        assert_eq!(second, Some("b".to_owned()));
        assert!(up(&store, PromptType::Command, &mut index).is_none());
    }

    #[test]
    fn history_zero_limit_clears() {
        let mut store = PromptHistoryStore::default();
        store.push(PromptType::Command, "a", 100);
        store.push(PromptType::Command, "b", 0);

        let mut index = 0;
        assert!(up(&store, PromptType::Command, &mut index).is_none());
    }

    #[test]
    fn history_types_are_independent() {
        let mut store = PromptHistoryStore::default();
        store.push(PromptType::Command, "cmd", 100);
        store.push(PromptType::Search, "search", 100);

        let mut index = 0;
        let cmd = up(&store, PromptType::Command, &mut index);
        assert_eq!(cmd, Some("cmd".to_owned()));

        let mut index = 0;
        let search = up(&store, PromptType::Search, &mut index);
        assert_eq!(search, Some("search".to_owned()));
    }
}
