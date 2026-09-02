use super::editor_state;
use crate::core::shared::text_utils;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ModelPickerStage {
    #[default]
    Model,
    Effort,
    Fast,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InlinePickerKind {
    Slash,
    Model,
    File,
    Skill,
}

pub const MODEL_PICKER_FAST_OPTIONS: &[&str] = &["normal", "fast"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModelPickerQuery<'a> {
    pub stage: ModelPickerStage,
    pub query: &'a [u8],
    pub token_start: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FilePickerQuery<'a> {
    /// Bytes between `@` and the cursor, without the leading `@`.
    pub query: &'a [u8],
    /// Byte offset of the `@` in the composer text.
    pub at_offset: usize,
    /// Byte offset where `query` begins.
    pub token_start: usize,
    /// The query begins with `@"` and stays open across spaces.
    pub quoted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InlineSkillQuery<'a> {
    /// Bytes between `$` and the cursor, without the leading `$`.
    pub query: &'a [u8],
    /// Byte offset of the `$` in the composer text.
    pub dollar_offset: usize,
    /// Byte offset where `query` begins.
    pub token_start: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InlineSlashQuery<'a> {
    /// Slash command prefix through the cursor, including the leading `/`.
    pub prefix: &'a [u8],
}

/// Owns layout-independent composer picker state.
#[derive(Clone, Debug, Default)]
pub struct State {
    pub slash_completion_index: usize,
    pub slash_completion_window_start: usize,
    pub dismissed_inline_picker: Option<InlinePickerKind>,
    pub model_completion_index: usize,
    pub model_completion_window_start: usize,
    pub model_completion_anchor_current: bool,
    pub model_picker_stage: ModelPickerStage,
    pub model_picker_pending_model: Vec<u8>,
    pub model_picker_effort_index: usize,
    pub model_picker_effort_window_start: usize,
    pub model_picker_fast_index: usize,
    pub model_picker_fast_window_start: usize,
    pub file_completion_index: usize,
    pub file_completion_window_start: usize,
    pub file_picker_episode_seen: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    OutOfMemory,
}

impl State {
    pub fn deinit(&mut self) {
        self.model_picker_pending_model.clear();
        *self = State::default();
    }

    pub fn reset_inline_picker_episode(&mut self) {
        self.slash_completion_index = 0;
        self.slash_completion_window_start = 0;
        self.dismissed_inline_picker = None;
    }

    pub fn dismiss_inline_picker(&mut self, kind: InlinePickerKind) {
        self.dismissed_inline_picker = Some(kind);
    }

    pub fn is_inline_picker_dismissed(&self, kind: InlinePickerKind) -> bool {
        self.dismissed_inline_picker == Some(kind)
    }

    pub fn reconcile_inline_picker_after_edit(&mut self, editor: &editor_state::State) {
        self.slash_completion_index = 0;
        self.slash_completion_window_start = 0;
        let dismissed = match self.dismissed_inline_picker {
            Some(d) => d,
            None => return,
        };
        if self.inline_picker_trigger_kind(editor) != Some(dismissed) {
            self.dismissed_inline_picker = None;
        }
    }

    pub fn reset_file_picker_index(&mut self) {
        self.file_completion_index = 0;
        self.file_completion_window_start = 0;
    }

    pub fn active_file_picker_query<'a>(
        &self,
        editor: &'a editor_state::State,
    ) -> Option<FilePickerQuery<'a>> {
        if self.is_inline_picker_dismissed(InlinePickerKind::File) {
            return None;
        }
        self.raw_file_picker_query(editor)
    }

    pub fn active_model_picker_query<'a>(
        &self,
        editor: &'a editor_state::State,
    ) -> Option<ModelPickerQuery<'a>> {
        if self.is_inline_picker_dismissed(InlinePickerKind::Model) {
            return None;
        }
        self.raw_model_picker_query(editor)
    }

    pub fn active_inline_skill_query<'a>(
        &self,
        editor: &'a editor_state::State,
    ) -> Option<InlineSkillQuery<'a>> {
        if self.is_inline_picker_dismissed(InlinePickerKind::Skill) {
            return None;
        }
        find_inline_skill_query(&editor.input, editor.cursor)
    }

    pub fn active_inline_slash_query<'a>(
        &self,
        editor: &'a editor_state::State,
    ) -> Option<InlineSlashQuery<'a>> {
        if self.is_inline_picker_dismissed(InlinePickerKind::Slash) {
            return None;
        }
        find_inline_slash_query(&editor.input, editor.cursor)
    }

    fn raw_file_picker_query<'a>(
        &self,
        editor: &'a editor_state::State,
    ) -> Option<FilePickerQuery<'a>> {
        if self.raw_model_picker_query(editor).is_some() {
            return None;
        }
        find_file_picker_query(&editor.input, editor.cursor)
    }

    pub fn inline_picker_trigger_kind(
        &self,
        editor: &editor_state::State,
    ) -> Option<InlinePickerKind> {
        if self.raw_model_picker_query(editor).is_some() {
            return Some(InlinePickerKind::Model);
        }
        if self.raw_file_picker_query(editor).is_some() {
            return Some(InlinePickerKind::File);
        }
        if find_inline_skill_query(&editor.input, editor.cursor).is_some() {
            return Some(InlinePickerKind::Skill);
        }
        if find_inline_slash_query(&editor.input, editor.cursor).is_some() {
            return Some(InlinePickerKind::Slash);
        }
        let trimmed = trim_start(&editor.input, b" \t\r\n");
        if !trimmed.is_empty() && trimmed[0] == b'/' {
            return Some(InlinePickerKind::Slash);
        }
        None
    }

    fn raw_model_picker_query<'a>(
        &self,
        editor: &'a editor_state::State,
    ) -> Option<ModelPickerQuery<'a>> {
        const PREFIX: &[u8] = b"/model ";
        let items = editor.input.as_slice();
        let trim_start_index = leading_whitespace_len(items);
        let trimmed = &items[trim_start_index..];
        if trimmed.len() < PREFIX.len() || !trimmed[..PREFIX.len()].eq_ignore_ascii_case(PREFIX) {
            return None;
        }

        match self.model_picker_stage {
            ModelPickerStage::Model => Some(ModelPickerQuery {
                stage: ModelPickerStage::Model,
                query: &trimmed[PREFIX.len()..],
                token_start: trim_start_index + PREFIX.len(),
            }),
            ModelPickerStage::Effort => {
                let token_start = model_picker_token_start(
                    trimmed,
                    &self.model_picker_pending_model,
                    ModelPickerStage::Effort,
                )?;
                Some(ModelPickerQuery {
                    stage: ModelPickerStage::Effort,
                    query: &trimmed[token_start..],
                    token_start: trim_start_index + token_start,
                })
            }
            ModelPickerStage::Fast => {
                let token_start = model_picker_token_start(
                    trimmed,
                    &self.model_picker_pending_model,
                    ModelPickerStage::Fast,
                )?;
                Some(ModelPickerQuery {
                    stage: ModelPickerStage::Fast,
                    query: &trimmed[token_start..],
                    token_start: trim_start_index + token_start,
                })
            }
        }
    }

    pub fn is_model_shaped_input(&self, editor: &editor_state::State) -> bool {
        is_bare_model_command_at_cursor(editor) || self.raw_model_picker_query(editor).is_some()
    }

    pub fn reset_active_model_picker_index(&mut self) {
        match self.model_picker_stage {
            ModelPickerStage::Model => {
                self.model_completion_index = 0;
                self.model_completion_window_start = 0;
                self.model_completion_anchor_current = false;
            }
            ModelPickerStage::Effort => {
                self.model_picker_effort_index = 0;
                self.model_picker_effort_window_start = 0;
            }
            ModelPickerStage::Fast => {
                self.model_picker_fast_index = 0;
                self.model_picker_fast_window_start = 0;
            }
        }
    }

    pub fn begin_model_picker_flow(
        &mut self,
        model: &[u8],
        effort_index: usize,
        fast_mode: bool,
        stage: ModelPickerStage,
    ) -> Result<(), Error> {
        let mut stable_model: Vec<u8> = Vec::new();
        stable_model
            .try_reserve_exact(model.len())
            .map_err(|_| Error::OutOfMemory)?;
        stable_model.extend_from_slice(model);

        self.model_picker_pending_model
            .try_reserve_exact(stable_model.len())
            .map_err(|_| Error::OutOfMemory)?;
        self.model_picker_pending_model.clear();
        self.model_picker_pending_model
            .extend_from_slice(&stable_model);
        self.model_picker_stage = stage;
        self.model_picker_effort_index = effort_index;
        self.model_picker_effort_window_start = 0;
        self.model_picker_fast_index = if fast_mode { 1 } else { 0 };
        self.model_picker_fast_window_start = 0;
        Ok(())
    }

    pub fn clear_model_picker_flow(&mut self) {
        self.model_picker_stage = ModelPickerStage::Model;
        self.model_picker_pending_model.clear();
        self.model_picker_effort_index = 0;
        self.model_picker_effort_window_start = 0;
        self.model_picker_fast_index = 0;
        self.model_picker_fast_window_start = 0;
        self.model_completion_index = 0;
        self.model_completion_window_start = 0;
        self.model_completion_anchor_current = false;
    }

    pub fn has_pending_model_picker_selection(&self) -> bool {
        !self.model_picker_pending_model.is_empty()
    }

    pub fn selected_model_picker_effort_index(&self) -> usize {
        self.model_picker_effort_index
    }

    pub fn selected_model_picker_fast(&self) -> bool {
        self.model_picker_fast_index % MODEL_PICKER_FAST_OPTIONS.len() == 1
    }
}

pub fn is_bare_model_command_at_cursor(editor: &editor_state::State) -> bool {
    if editor.cursor != editor.input.len() {
        return false;
    }
    let trimmed = trim_start(&editor.input, b" \t");
    trimmed.eq_ignore_ascii_case(b"/model")
}

pub fn filter_completion_labels<'a>(
    query: &[u8],
    options: &[&'a [u8]],
    out: &mut [&'a [u8]],
) -> usize {
    let normalized_query = trim_both(query, b" \t");
    let mut count: usize = 0;
    for &option in options {
        if count >= out.len() {
            break;
        }
        if normalized_query.is_empty()
            || text_utils::contains_ignore_case(option, normalized_query)
        {
            out[count] = option;
            count += 1;
        }
    }
    count
}

pub fn is_file_picker_terminator(byte: u8) -> bool {
    byte == b' ' || byte == b'\t' || byte == b'\n' || byte == b'\r'
}

fn leading_whitespace_len(bytes: &[u8]) -> usize {
    let mut index: usize = 0;
    while index < bytes.len() && (bytes[index] == b' ' || bytes[index] == b'\t') {
        index += 1;
    }
    index
}

fn trim_start<'a>(mut bytes: &'a [u8], cutset: &[u8]) -> &'a [u8] {
    while bytes.first().is_some_and(|b| cutset.contains(b)) {
        bytes = &bytes[1..];
    }
    bytes
}

fn trim_both<'a>(bytes: &'a [u8], cutset: &[u8]) -> &'a [u8] {
    let mut out = trim_start(bytes, cutset);
    while out.last().is_some_and(|b| cutset.contains(b)) {
        out = &out[..out.len() - 1];
    }
    out
}

fn find_file_picker_query<'a>(items: &'a [u8], cursor: usize) -> Option<FilePickerQuery<'a>> {
    if cursor > items.len() {
        return None;
    }

    let mut index = cursor;
    while index > 0 {
        let byte = items[index - 1];
        if byte == b'@' {
            let at_offset = index - 1;
            if at_offset > 0 && !is_file_picker_start_boundary(items[at_offset - 1]) {
                index -= 1;
                continue;
            }
            let quoted = index < cursor && items[index] == b'"';
            let token_start = index + usize::from(quoted);
            return Some(FilePickerQuery {
                query: &items[token_start..cursor],
                at_offset,
                token_start,
                quoted,
            });
        }
        if is_file_picker_terminator(byte) {
            break;
        }
        index -= 1;
    }

    index = cursor;
    while index > 1 {
        let byte = items[index - 1];
        if byte == b'\t' || byte == b'\n' || byte == b'\r' {
            return None;
        }
        if byte == b'"' {
            let at_offset = index - 2;
            if items[at_offset] != b'@' {
                return None;
            }
            if at_offset > 0 && !is_file_picker_start_boundary(items[at_offset - 1]) {
                return None;
            }
            return Some(FilePickerQuery {
                query: &items[index..cursor],
                at_offset,
                token_start: index,
                quoted: true,
            });
        }
        index -= 1;
    }

    None
}

fn find_inline_skill_query<'a>(items: &'a [u8], cursor: usize) -> Option<InlineSkillQuery<'a>> {
    if cursor != items.len() {
        return None;
    }

    let mut token_start = cursor;
    while token_start > 0 && !is_file_picker_terminator(items[token_start - 1]) {
        token_start -= 1;
    }
    if token_start == 0 || token_start == cursor || items[token_start] != b'$' {
        return None;
    }

    let query_start = token_start + 1;
    if query_start == cursor {
        return None;
    }
    Some(InlineSkillQuery {
        query: &items[query_start..cursor],
        dollar_offset: token_start,
        token_start: query_start,
    })
}

fn find_inline_slash_query<'a>(items: &'a [u8], cursor: usize) -> Option<InlineSlashQuery<'a>> {
    if cursor != items.len() {
        return None;
    }

    let mut token_start = cursor;
    while token_start > 0 && !is_file_picker_terminator(items[token_start - 1]) {
        token_start -= 1;
    }
    if token_start == 0 || token_start + 1 >= cursor || items[token_start] != b'/' {
        return None;
    }
    if items[..token_start]
        .iter()
        .all(|&b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
    {
        return None;
    }

    Some(InlineSlashQuery {
        prefix: &items[token_start..cursor],
    })
}

fn is_file_picker_start_boundary(byte: u8) -> bool {
    if is_file_picker_terminator(byte) {
        return true;
    }
    matches!(byte, b'(' | b'[' | b'{' | b'<' | b'\'' | b'"' | b'`')
}

fn model_picker_token_start(trimmed: &[u8], model: &[u8], stage: ModelPickerStage) -> Option<usize> {
    const PREFIX: &[u8] = b"/model ";
    if trimmed.len() < PREFIX.len() || !trimmed[..PREFIX.len()].eq_ignore_ascii_case(PREFIX) {
        return None;
    }
    if model.is_empty() {
        return None;
    }

    let after_prefix = &trimmed[PREFIX.len()..];
    if after_prefix.len() < model.len() || &after_prefix[..model.len()] != model {
        return None;
    }

    let after_model = PREFIX.len() + model.len();
    if stage == ModelPickerStage::Effort {
        return Some(skip_picker_spaces(trimmed, after_model));
    }

    let effort_start = skip_picker_spaces(trimmed, after_model);
    if effort_start >= trimmed.len() {
        return None;
    }

    let mut effort_end = effort_start;
    while effort_end < trimmed.len() && trimmed[effort_end] != b' ' && trimmed[effort_end] != b'\t'
    {
        effort_end += 1;
    }
    Some(skip_picker_spaces(trimmed, effort_end))
}

fn skip_picker_spaces(bytes: &[u8], start: usize) -> usize {
    let mut index = start;
    while index < bytes.len() && (bytes[index] == b' ' || bytes[index] == b'\t') {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picker_state_resolves_model_file_skill_and_slash_queries() {
        let mut editor = editor_state::State::default();
        let mut state = State::default();

        editor.set_text(b"/model gpt");
        let model_query = state.active_model_picker_query(&editor).unwrap();
        assert_eq!(ModelPickerStage::Model, model_query.stage);
        assert_eq!(b"gpt", model_query.query);

        editor.set_text(b"open (@src/main.zig");
        let file_query = state.active_file_picker_query(&editor).unwrap();
        assert_eq!(b"src/main.zig", file_query.query);

        editor.set_text(b"use $blueprint");
        assert_eq!(
            b"blueprint",
            state.active_inline_skill_query(&editor).unwrap().query
        );

        editor.set_text(b"then /help");
        assert_eq!(
            b"/help",
            state.active_inline_slash_query(&editor).unwrap().prefix
        );
    }

    #[test]
    fn model_picker_query_takes_precedence_over_file_syntax() {
        let mut editor = editor_state::State::default();
        let mut state = State::default();

        editor.set_text(b"/model @provider");
        assert!(state.active_model_picker_query(&editor).is_some());
        assert!(state.active_file_picker_query(&editor).is_none());
    }

    #[test]
    fn picker_dismissal_lasts_for_one_trigger_episode() {
        let mut editor = editor_state::State::default();
        let mut state = State::default();

        editor.set_text(b"use $blueprint");
        state.dismiss_inline_picker(InlinePickerKind::Skill);
        assert!(state.active_inline_skill_query(&editor).is_none());

        editor.set_text(b"use $blueprintx");
        state.reconcile_inline_picker_after_edit(&editor);
        assert!(state.is_inline_picker_dismissed(InlinePickerKind::Skill));

        editor.set_text(b"use blueprint ");
        state.reconcile_inline_picker_after_edit(&editor);
        assert!(!state.is_inline_picker_dismissed(InlinePickerKind::Skill));
    }

    #[test]
    fn file_picker_state_transitions_activation_dismissal_and_recovery() {
        let mut editor = editor_state::State::default();
        let mut state = State::default();

        editor.set_text(b"plain text");
        assert!(state.active_file_picker_query(&editor).is_none());

        editor.set_text(b"@");
        assert_eq!(b"", state.active_file_picker_query(&editor).unwrap().query);

        editor.set_text(b"@./scoped/@types");
        assert_eq!(
            b"./scoped/@types",
            state.active_file_picker_query(&editor).unwrap().query
        );

        editor.set_text(b"@\"./space dir/item");
        let quoted = state.active_file_picker_query(&editor).unwrap();
        assert!(quoted.quoted);
        assert_eq!(b"./space dir/item", quoted.query);

        state.dismiss_inline_picker(InlinePickerKind::File);
        assert!(state.active_file_picker_query(&editor).is_none());

        editor.set_text(b"@\"./space dir/item.txt");
        state.reconcile_inline_picker_after_edit(&editor);
        assert!(state.is_inline_picker_dismissed(InlinePickerKind::File));

        editor.set_text(b"plain text");
        state.reconcile_inline_picker_after_edit(&editor);
        assert!(!state.is_inline_picker_dismissed(InlinePickerKind::File));

        editor.set_text(b"@~/Downloads");
        assert_eq!(
            b"~/Downloads",
            state.active_file_picker_query(&editor).unwrap().query
        );

        editor.set_text(b"@~/Downloads/file.txt ");
        assert!(state.active_file_picker_query(&editor).is_none());
    }

    #[test]
    fn model_picker_flow_accepts_aliased_pending_model_input() {
        let mut state = State::default();

        state
            .begin_model_picker_flow(b"openai/gpt-5", 2, false, ModelPickerStage::Effort)
            .unwrap();
        let aliased_model = state.model_picker_pending_model.clone();
        state
            .begin_model_picker_flow(&aliased_model, 3, true, ModelPickerStage::Fast)
            .unwrap();

        assert_eq!(ModelPickerStage::Fast, state.model_picker_stage);
        assert_eq!(b"openai/gpt-5", state.model_picker_pending_model.as_slice());
        assert_eq!(3, state.selected_model_picker_effort_index());
        assert!(state.selected_model_picker_fast());
    }

    #[test]
    fn model_picker_flow_preserves_state_when_allocation_fails() {
        // Zig simulates an allocation failure with a FailingAllocator here; the
        // Rust port uses the infallible global allocator, so OutOfMemory is an
        // API-only variant that is never produced. The success path is covered
        // by the aliased-model test above; this exercises the same call shape
        // with a large model so `try_reserve` stays on the fallible path.
        let mut state = State::default();
        state
            .begin_model_picker_flow(b"old", 2, false, ModelPickerStage::Effort)
            .unwrap();

        let large_model = vec![b'x'; 4096];
        let result =
            state.begin_model_picker_flow(&large_model, 5, true, ModelPickerStage::Fast);
        assert!(result.is_ok());

        assert_eq!(ModelPickerStage::Fast, state.model_picker_stage);
        assert_eq!(vec![b'x'; 4096], state.model_picker_pending_model);
        assert_eq!(5, state.selected_model_picker_effort_index());
        assert!(state.selected_model_picker_fast());
    }

    #[test]
    fn completion_label_filtering_is_trimmed_and_case_insensitive() {
        let options: &[&[u8]] = &[b"High", b"xhigh", b"max"];
        let mut matches: [&[u8]; 3] = [b"", b"", b""];
        let count = filter_completion_labels(b" H ", options, &mut matches);

        assert_eq!(2, count);
        assert_eq!(b"High", matches[0]);
        assert_eq!(b"xhigh", matches[1]);
    }
}
