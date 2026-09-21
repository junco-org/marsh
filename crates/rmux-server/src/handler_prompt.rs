use std::ops::{Deref, DerefMut};

use tokio::sync::oneshot;

use super::scripting_support::QueueExecutionContext;
use super::RequesterOrigin;
use crate::prompt_buffer::PromptBuffer;
use crate::renderer::RenderedPrompt;

#[path = "handler_prompt/events.rs"]
mod events;
#[path = "handler_prompt/history.rs"]
mod history;
#[path = "handler_prompt/input.rs"]
mod input;
#[path = "handler_prompt/lifecycle.rs"]
mod lifecycle;
#[path = "handler_prompt/render.rs"]
mod render;
#[path = "handler_prompt/substitution.rs"]
mod substitution;
#[path = "handler_prompt/types.rs"]
mod types;

pub(super) use self::history::PromptHistoryStore;
pub(crate) use self::input::{decode_prompt_key, PromptInputEvent};
pub(super) use self::substitution::substitute_prompt_template;
pub(super) use self::types::{
    CommandPromptPlan, ConfirmBeforePlan, PromptField, PromptQueueResult, PromptStartOutcome,
    PromptType,
};

pub(super) const PROMPT_FLAG_SINGLE: u8 = 0x01;
pub(super) const PROMPT_FLAG_NUMERIC: u8 = 0x02;
pub(super) const PROMPT_FLAG_INCREMENTAL: u8 = 0x04;
pub(super) const PROMPT_FLAG_KEY: u8 = 0x10;
pub(super) const PROMPT_FLAG_BSPACE_EXIT: u8 = 0x80;

#[derive(Debug)]
enum PromptCompletion {
    Foreground(oneshot::Sender<PromptQueueResult>),
    Background,
}

#[derive(Debug)]
enum PromptKind {
    Command {
        template: String,
        format_values: Vec<(String, String)>,
    },
    Confirm {
        template: String,
        format_values: Vec<(String, String)>,
        confirm_key: char,
        default_yes: bool,
    },
}

/// One client's prompt: what is being asked, what is being typed, and who is waiting for it.
///
/// The typing is [`PromptBuffer`]'s, shared with the shell prompt a commandless pane runs, and
/// reached through this type's [`Deref`]. That delegation is deliberate rather than decorative:
/// every editing key is an editor operation, so an inherent copy of each one here is the second
/// editor plan line 365 exists to prevent.
#[derive(Debug)]
pub(super) struct ClientPromptState {
    fields: Vec<PromptField>,
    current: usize,
    responses: Vec<String>,
    prompt: String,
    /// The line being edited, the caret, the kill slot and the history walk.
    editor: PromptBuffer,
    last_input: String,
    flags: u8,
    prompt_type: PromptType,
    origin: RequesterOrigin,
    context: QueueExecutionContext,
    kind: PromptKind,
    completion: PromptCompletion,
}

impl Deref for ClientPromptState {
    type Target = PromptBuffer;

    fn deref(&self) -> &Self::Target {
        &self.editor
    }
}

impl DerefMut for ClientPromptState {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.editor
    }
}

impl ClientPromptState {
    pub(in crate::handler) fn rename_session_targets(
        &mut self,
        old_name: &rmux_proto::SessionName,
        new_name: &rmux_proto::SessionName,
    ) {
        self.context.rename_session_targets(old_name, new_name);
    }

    fn new_command(plan: CommandPromptPlan, completion: PromptCompletion) -> Self {
        let first = plan.fields.first().cloned().unwrap_or(PromptField {
            prompt: ":".to_owned(),
            input: String::new(),
        });
        // An incremental prompt searches from nothing: the field's existing input is the *last*
        // search, offered by Ctrl-R rather than pre-typed into the line.
        let editor = if (plan.flags & PROMPT_FLAG_INCREMENTAL) != 0 {
            PromptBuffer::default()
        } else {
            PromptBuffer::with_text(first.input.clone())
        };

        Self {
            fields: plan.fields,
            current: 0,
            responses: Vec::new(),
            prompt: first.prompt,
            editor,
            last_input: first.input,
            flags: plan.flags,
            prompt_type: plan.prompt_type,
            origin: plan.origin,
            context: plan.context,
            kind: PromptKind::Command {
                template: plan.template,
                format_values: plan.format_values,
            },
            completion,
        }
    }

    fn new_confirm(plan: ConfirmBeforePlan, completion: PromptCompletion) -> Self {
        Self {
            fields: vec![PromptField {
                prompt: plan.prompt.clone(),
                input: String::new(),
            }],
            current: 0,
            responses: Vec::new(),
            prompt: plan.prompt,
            editor: PromptBuffer::default(),
            last_input: String::new(),
            flags: PROMPT_FLAG_SINGLE,
            prompt_type: PromptType::Command,
            origin: plan.origin,
            context: plan.context,
            kind: PromptKind::Confirm {
                template: plan.template,
                format_values: plan.format_values,
                confirm_key: plan.confirm_key,
                default_yes: plan.default_yes,
            },
            completion,
        }
    }

    fn apply_field(&mut self, field: &PromptField) {
        self.prompt = field.prompt.clone();
        self.last_input = field.input.clone();
        if (self.flags & PROMPT_FLAG_INCREMENTAL) != 0 {
            self.editor.clear();
        } else {
            self.editor.set_text(field.input.clone());
            self.editor.history_index = 0;
            self.editor.pre_history_buffer = None;
        }
    }

    fn submit_response(&mut self, response: String) -> Option<PromptFinalizeKind> {
        self.responses.push(response);
        self.current += 1;
        if let Some(field) = self.fields.get(self.current).cloned() {
            self.apply_field(&field);
            None
        } else {
            Some(PromptFinalizeKind::Command {
                responses: self.responses.clone(),
            })
        }
    }

    fn current_command_dispatch(&self, responses: Vec<String>) -> Option<PromptDispatch> {
        let PromptKind::Command {
            template,
            format_values,
        } = &self.kind
        else {
            return None;
        };

        Some(PromptDispatch {
            origin: self.origin.clone(),
            context: self.context.clone(),
            template: template.clone(),
            format_values: format_values.clone(),
            responses,
        })
    }

    fn initial_incremental_dispatch(&self) -> Option<PromptDispatch> {
        ((self.flags & PROMPT_FLAG_INCREMENTAL) != 0)
            .then(|| self.current_command_dispatch(vec!["=".to_owned()]))
            .flatten()
    }

    pub(super) fn rendered_prompt(&self) -> RenderedPrompt {
        RenderedPrompt {
            prompt: self.prompt.clone(),
            input: self.editor.buffer_string(),
            cursor: self.editor.cursor,
            // tmux opens command-prompt in PROMPT_ENTRY mode and only flips to
            // PROMPT_COMMAND after Escape in vi-style editing. rmux does not
            // model that mode switch yet, so the initial render must stay on
            // the non-command prompt style to match tmux.
            command_prompt: false,
        }
    }

    /// Inserts a whole run of typed text at once, unless this prompt reads one event at a time.
    ///
    /// The flag check is the reason this is not simply [`PromptBuffer::insert_text`]: a
    /// single-key, numeric, incremental or key-reading prompt answers *per event*, so collapsing a
    /// burst into one insertion would submit one response for what were several keystrokes.
    fn insert_batched_text(&mut self, text: &str) -> bool {
        const PER_EVENT_FLAGS: u8 =
            PROMPT_FLAG_SINGLE | PROMPT_FLAG_NUMERIC | PROMPT_FLAG_INCREMENTAL | PROMPT_FLAG_KEY;
        if self.flags & PER_EVENT_FLAGS != 0 {
            return false;
        }

        self.editor.insert_text(text)
    }

    fn into_finished(self, kind: PromptFinalizeKind) -> FinishedPrompt {
        let kind = match (self.kind, kind) {
            (_, PromptFinalizeKind::Cancel) => FinishedPromptKind::Cancel,
            (
                PromptKind::Command {
                    template,
                    format_values,
                },
                PromptFinalizeKind::Command { responses },
            ) => FinishedPromptKind::Command {
                template,
                format_values,
                responses,
            },
            (
                PromptKind::Confirm {
                    template,
                    format_values,
                    ..
                },
                PromptFinalizeKind::Confirm { accepted: true },
            ) => FinishedPromptKind::Command {
                template,
                format_values,
                responses: Vec::new(),
            },
            (PromptKind::Confirm { .. }, PromptFinalizeKind::Confirm { accepted: false }) => {
                FinishedPromptKind::Cancel
            }
            _ => FinishedPromptKind::Cancel,
        };

        FinishedPrompt {
            origin: self.origin,
            context: self.context,
            completion: self.completion,
            kind,
        }
    }
}

#[derive(Debug, Clone)]
enum PromptFinalizeKind {
    Cancel,
    Command { responses: Vec<String> },
    Confirm { accepted: bool },
}

#[derive(Debug)]
struct PromptAction {
    refresh: bool,
    dispatch: Option<PromptDispatch>,
    finalize: Option<PromptFinalizeKind>,
}

impl PromptAction {
    const fn none() -> Self {
        Self {
            refresh: false,
            dispatch: None,
            finalize: None,
        }
    }
}

#[derive(Debug)]
enum FinishedPromptKind {
    Cancel,
    Command {
        template: String,
        format_values: Vec<(String, String)>,
        responses: Vec<String>,
    },
}

#[derive(Debug)]
struct FinishedPrompt {
    origin: RequesterOrigin,
    context: QueueExecutionContext,
    completion: PromptCompletion,
    kind: FinishedPromptKind,
}

fn prompt_accept_should_dismiss_mode_tree(finished: &FinishedPrompt) -> bool {
    matches!(
        &finished.kind,
        FinishedPromptKind::Command { template, .. }
            if template.trim_start().starts_with("kill-pane")
    )
}

#[derive(Debug, Clone)]
struct PromptDispatch {
    origin: RequesterOrigin,
    context: QueueExecutionContext,
    template: String,
    format_values: Vec<(String, String)>,
    responses: Vec<String>,
}
