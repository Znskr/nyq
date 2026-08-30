//! Settings catalog: searchable, grouped, editable preference snapshots.
//!
//! Mirrors `zig-reference/src/core/config/settings_catalog.zig`.

use super::model_capabilities::ReasoningEffortOptions;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Category {
    All,
    Interface,
    Agent,
    Notifications,
    Advanced,
}

impl Default for Category {
    fn default() -> Self {
        Category::All
    }
}

impl Category {
    pub fn label(self) -> &'static str {
        match self {
            Category::All => "All",
            Category::Interface => "Interface",
            Category::Agent => "Agent",
            Category::Notifications => "Notifications",
            Category::Advanced => "Advanced",
        }
    }

    pub fn index(self) -> usize {
        match self {
            Category::All => 0,
            Category::Interface => 1,
            Category::Agent => 2,
            Category::Notifications => 3,
            Category::Advanced => 4,
        }
    }

    pub fn from_index(index: usize) -> Category {
        match index {
            0 => Category::All,
            1 => Category::Interface,
            2 => Category::Agent,
            3 => Category::Notifications,
            _ => Category::Advanced,
        }
    }
}

pub const CATEGORY_COUNT: usize = 5;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SettingId {
    StatuslineContext,
    StatuslineSession,
    StatuslineWorkspace,
    SlashMenuCategories,
    CollapseToolCalls,
    Model,
    Effort,
    FastMode,
    PermissionMode,
    SoundLevel,
    StartupScrollback,
    PromptHistory,
}

impl SettingId {
    pub fn tag_name(self) -> &'static str {
        match self {
            SettingId::StatuslineContext => "statusline_context",
            SettingId::StatuslineSession => "statusline_session",
            SettingId::StatuslineWorkspace => "statusline_workspace",
            SettingId::SlashMenuCategories => "slash_menu_categories",
            SettingId::CollapseToolCalls => "collapse_tool_calls",
            SettingId::Model => "model",
            SettingId::Effort => "effort",
            SettingId::FastMode => "fast_mode",
            SettingId::PermissionMode => "permission_mode",
            SettingId::SoundLevel => "sound_level",
            SettingId::StartupScrollback => "startup_scrollback",
            SettingId::PromptHistory => "prompt_history",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Spec {
    pub id: SettingId,
    pub category: Category,
    pub label: &'static str,
    pub description: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub model: &'static str,
    pub effort: &'static str,
    pub reasoning_efforts: ReasoningEffortOptions,
    pub fast_mode: bool,
    pub supports_fast_mode: bool,
    pub permission_mode: &'static str,
    pub statusline_context: bool,
    pub statusline_session: bool,
    pub statusline_workspace: bool,
    pub slash_menu_categories: bool,
    pub collapse_tool_calls: bool,
    pub startup_scrollback: bool,
    pub prompt_history: bool,
    pub sound_level: &'static str,
}

impl Default for Snapshot {
    fn default() -> Self {
        Snapshot {
            model: "",
            effort: "default",
            reasoning_efforts: ReasoningEffortOptions::default(),
            fast_mode: false,
            supports_fast_mode: false,
            permission_mode: "ask",
            statusline_context: false,
            statusline_session: false,
            statusline_workspace: false,
            slash_menu_categories: true,
            collapse_tool_calls: false,
            startup_scrollback: true,
            prompt_history: true,
            sound_level: "on",
        }
    }
}

impl Snapshot {
    pub fn value(&self, id: SettingId) -> &str {
        match id {
            SettingId::Model => self.model,
            SettingId::Effort => self.effort,
            SettingId::FastMode => on_off(self.fast_mode),
            SettingId::PermissionMode => self.permission_mode,
            SettingId::StatuslineContext => on_off(self.statusline_context),
            SettingId::StatuslineSession => on_off(self.statusline_session),
            SettingId::StatuslineWorkspace => on_off(self.statusline_workspace),
            SettingId::SlashMenuCategories => on_off(self.slash_menu_categories),
            SettingId::CollapseToolCalls => on_off(self.collapse_tool_calls),
            SettingId::StartupScrollback => on_off(self.startup_scrollback),
            SettingId::PromptHistory => on_off(self.prompt_history),
            SettingId::SoundLevel => self.sound_level,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Item<'a> {
    pub id: SettingId,
    pub category: Category,
    pub label: &'static str,
    pub description: &'static str,
    pub value: &'a str,
}

#[derive(Clone, Copy, Debug)]
pub struct Change<'a> {
    pub setting: SettingId,
    pub value: &'a str,
}

#[derive(Clone, Copy, Debug)]
pub struct StatuslineChoice {
    pub label: &'static str,
    pub description: &'static str,
    pub setting: SettingId,
}

impl StatuslineChoice {
    pub fn is_enabled(self, snapshot: &Snapshot) -> bool {
        snapshot.value(self.setting) == "on"
    }

    pub fn toggled_change(self, snapshot: &Snapshot) -> Change<'static> {
        Change {
            setting: self.setting,
            value: if self.is_enabled(snapshot) { "off" } else { "on" },
        }
    }
}

const STATUSLINE_CHOICES: [StatuslineChoice; 3] = [
    StatuslineChoice {
        label: "Context",
        description: "Show context-window usage",
        setting: SettingId::StatuslineContext,
    },
    StatuslineChoice {
        label: "Session",
        description: "Show the current session title",
        setting: SettingId::StatuslineSession,
    },
    StatuslineChoice {
        label: "Workspace",
        description: "Show the workspace path and Git branch",
        setting: SettingId::StatuslineWorkspace,
    },
];

#[derive(Clone, Debug, Default)]
pub struct StatuslineMenu {
    pub active: bool,
    pub selected_index: usize,
}

impl StatuslineMenu {
    pub fn open(&mut self) {
        *self = StatuslineMenu {
            active: true,
            ..StatuslineMenu::default()
        };
    }

    pub fn close(&mut self) {
        *self = StatuslineMenu::default();
    }

    pub fn move_selection(&mut self, delta: i32) -> bool {
        match move_compact_selection(
            self.active,
            self.selected_index,
            delta,
            STATUSLINE_CHOICES.len(),
        ) {
            Some(next) => {
                self.selected_index = next;
                true
            }
            None => false,
        }
    }

    pub fn selected_choice(&self) -> Option<StatuslineChoice> {
        if !self.active {
            return None;
        }
        Some(STATUSLINE_CHOICES[self.selected_index % STATUSLINE_CHOICES.len()])
    }

    pub fn selected_change(&self, snapshot: &Snapshot) -> Option<Change<'static>> {
        Some(self.selected_choice()?.toggled_change(snapshot))
    }

    pub fn change_selected_option<'a>(
        &self,
        snapshot: &'a Snapshot,
        delta: i32,
    ) -> Option<Change<'a>> {
        if delta == 0 {
            return None;
        }
        let choice = self.selected_choice()?;
        cycle_change(snapshot, choice.setting, delta)
    }
}

pub fn statusline_choice_count() -> usize {
    STATUSLINE_CHOICES.len()
}

pub fn statusline_choice_at(index: usize) -> Option<StatuslineChoice> {
    STATUSLINE_CHOICES.get(index).copied()
}

fn move_compact_selection(
    active: bool,
    selected_index: usize,
    delta: i32,
    count: usize,
) -> Option<usize> {
    if !active || delta == 0 || count == 0 {
        return None;
    }
    let count_signed = count as i32;
    let mut next = (selected_index % count) as i32 + delta;
    while next < 0 {
        next += count_signed;
    }
    while next >= count_signed {
        next -= count_signed;
    }
    Some(next as usize)
}

#[derive(Clone, Debug)]
pub struct Menu {
    pub active: bool,
    pub category: Category,
    pub selected_index: usize,
    pub window_start: usize,
    pub startup_scrollback: bool,
}

impl Default for Menu {
    fn default() -> Self {
        Menu {
            active: false,
            category: Category::All,
            selected_index: 0,
            window_start: 0,
            startup_scrollback: true,
        }
    }
}

impl Menu {
    pub fn open(&mut self) {
        *self = Menu {
            active: true,
            ..Menu::default()
        };
    }

    pub fn open_with_startup_scrollback(&mut self, enabled: bool) {
        *self = Menu {
            active: true,
            startup_scrollback: enabled,
            ..Menu::default()
        };
    }

    pub fn close(&mut self) {
        *self = Menu::default();
    }

    pub fn reset_for_query(&mut self) {
        self.selected_index = 0;
        self.window_start = 0;
    }

    pub fn cycle_category(&mut self, delta: i32) -> bool {
        if !self.active {
            return false;
        }
        let count = CATEGORY_COUNT as i32;
        let mut next = self.category.index() as i32 + delta;
        while next < 0 {
            next += count;
        }
        while next >= count {
            next -= count;
        }
        self.category = Category::from_index(next as usize);
        self.selected_index = 0;
        self.window_start = 0;
        true
    }

    pub fn move_selection(
        &mut self,
        snapshot: &Snapshot,
        query: &str,
        delta: i32,
        visible_items: u16,
    ) -> bool {
        let item_count = filtered_count(snapshot, self.category, query);
        if !self.active || item_count == 0 {
            return false;
        }

        let current = (self.selected_index % item_count) as i32;
        let mut next = current + delta;
        if next < 0 {
            next = item_count as i32 - 1;
        }
        if next >= item_count as i32 {
            next = 0;
        }
        self.selected_index = next as usize;
        self.window_start = update_window_start(
            self.window_start,
            item_count,
            self.selected_index,
            visible_items.max(1),
        );
        true
    }

    pub fn selected_item<'a>(
        &self,
        snapshot: &'a Snapshot,
        query: &str,
    ) -> Option<Item<'a>> {
        let item_count = filtered_count(snapshot, self.category, query);
        if !self.active || item_count == 0 {
            return None;
        }
        item_at(snapshot, self.category, query, self.selected_index % item_count)
    }

    pub fn change_selected_option<'a>(
        &self,
        snapshot: &'a Snapshot,
        query: &str,
        delta: i32,
    ) -> Option<Change<'a>> {
        if delta == 0 {
            return None;
        }
        let item = self.selected_item(snapshot, query)?;
        cycle_change(snapshot, item.id, delta)
    }
}

const SPECS: [Spec; 12] = [
    Spec { id: SettingId::StatuslineContext, category: Category::Interface, label: "Status line context", description: "Show context usage in the status line" },
    Spec { id: SettingId::StatuslineSession, category: Category::Interface, label: "Status line session", description: "Show the session title in the status line" },
    Spec { id: SettingId::StatuslineWorkspace, category: Category::Interface, label: "Status line workspace", description: "Show the workspace path and Git branch in the status line" },
    Spec { id: SettingId::SlashMenuCategories, category: Category::Interface, label: "Slash menu categories", description: "Show categories and skill sources in slash-command results" },
    Spec { id: SettingId::CollapseToolCalls, category: Category::Interface, label: "Collapse tool calls", description: "Show only a summary for each group of tool calls" },
    Spec { id: SettingId::Model, category: Category::Agent, label: "Model", description: "Choose the model used for new turns" },
    Spec { id: SettingId::Effort, category: Category::Agent, label: "Reasoning effort", description: "Control how much reasoning the model applies" },
    Spec { id: SettingId::FastMode, category: Category::Agent, label: "Fast mode", description: "Use faster inference when the model supports it" },
    Spec { id: SettingId::PermissionMode, category: Category::Agent, label: "Permission mode", description: "Choose when fx asks before taking actions" },
    Spec { id: SettingId::SoundLevel, category: Category::Notifications, label: "Sound level", description: "Choose off, on, or max sounds and terminal bells" },
    Spec { id: SettingId::StartupScrollback, category: Category::Advanced, label: "Startup scrollback", description: "Restore terminal output when fx starts" },
    Spec { id: SettingId::PromptHistory, category: Category::Advanced, label: "Prompt history", description: "Save accepted prompts and slash commands for composer history" },
];

const ON_OFF_OPTIONS: [&str; 2] = ["off", "on"];
const PERMISSION_OPTIONS: [&str; 3] = ["ask", "auto", "yolo"];
const SOUND_LEVEL_OPTIONS: [&str; 3] = ["off", "on", "max"];

pub fn filtered_count(snapshot: &Snapshot, category: Category, query: &str) -> usize {
    SPECS
        .iter()
        .filter(|spec| matches(snapshot, **spec, category, query))
        .count()
}

pub fn category_filtered_count(snapshot: &Snapshot, category: Category, query: &str) -> usize {
    if category == Category::All {
        return filtered_count(snapshot, Category::All, query);
    }
    SPECS
        .iter()
        .filter(|spec| spec.category == category && matches_query(snapshot, **spec, query))
        .count()
}

pub fn item_at<'a>(
    snapshot: &'a Snapshot,
    category: Category,
    query: &str,
    display_index: usize,
) -> Option<Item<'a>> {
    let mut match_index = 0;
    for spec in SPECS.iter() {
        if !matches(snapshot, *spec, category, query) {
            continue;
        }
        if match_index == display_index {
            return Some(Item {
                id: spec.id,
                category: spec.category,
                label: spec.label,
                description: spec.description,
                value: snapshot.value(spec.id),
            });
        }
        match_index += 1;
    }
    None
}

pub fn spec_for(id: SettingId) -> &'static Spec {
    SPECS.iter().find(|spec| spec.id == id).unwrap()
}

pub fn option_count(snapshot: &Snapshot, id: SettingId) -> usize {
    match id {
        SettingId::Effort => {
            if snapshot.reasoning_efforts.len > 0
                || !snapshot.effort.eq_ignore_ascii_case("default")
            {
                snapshot.reasoning_efforts.len + 1
            } else {
                0
            }
        }
        SettingId::FastMode => {
            if snapshot.supports_fast_mode || snapshot.fast_mode {
                ON_OFF_OPTIONS.len()
            } else {
                0
            }
        }
        _ => static_options_for(id).len(),
    }
}

pub fn option_at<'a>(snapshot: &'a Snapshot, id: SettingId, index: usize) -> Option<&'a str> {
    if id == SettingId::Effort {
        if index >= option_count(snapshot, id) {
            return None;
        }
        if index == 0 {
            return Some("default");
        }
        return Some(
            std::str::from_utf8(snapshot.reasoning_efforts.values[index - 1].display_label())
                .expect("reasoning effort labels are ascii"),
        );
    }
    if index >= option_count(snapshot, id) {
        return None;
    }
    let options = static_options_for(id);
    if index >= options.len() {
        return None;
    }
    Some(options[index])
}

fn cycle_change<'a>(snapshot: &'a Snapshot, id: SettingId, delta: i32) -> Option<Change<'a>> {
    let count = option_count(snapshot, id);
    if count == 0 {
        return None;
    }
    let current = selected_option_index(snapshot, id).unwrap_or(0) as i32;
    let count_signed = count as i32;
    let mut next = current + delta;
    while next < 0 {
        next += count_signed;
    }
    while next >= count_signed {
        next -= count_signed;
    }
    change_at(snapshot, id, next as usize)
}

pub fn selected_option_index(snapshot: &Snapshot, id: SettingId) -> Option<usize> {
    let current = snapshot.value(id);
    for index in 0..option_count(snapshot, id) {
        let option = option_at(snapshot, id, index)?;
        if option.eq_ignore_ascii_case(current) {
            return Some(index);
        }
    }
    None
}

pub fn change_at<'a>(
    snapshot: &'a Snapshot,
    id: SettingId,
    option_index: usize,
) -> Option<Change<'a>> {
    Some(Change {
        setting: id,
        value: option_at(snapshot, id, option_index)?,
    })
}

fn static_options_for(id: SettingId) -> &'static [&'static str] {
    match id {
        SettingId::Model | SettingId::Effort => &[],
        SettingId::FastMode
        | SettingId::StatuslineContext
        | SettingId::StatuslineSession
        | SettingId::StatuslineWorkspace
        | SettingId::SlashMenuCategories
        | SettingId::CollapseToolCalls
        | SettingId::StartupScrollback
        | SettingId::PromptHistory => &ON_OFF_OPTIONS,
        SettingId::SoundLevel => &SOUND_LEVEL_OPTIONS,
        SettingId::PermissionMode => &PERMISSION_OPTIONS,
    }
}

fn matches(snapshot: &Snapshot, spec: Spec, category: Category, query: &str) -> bool {
    (category == Category::All || spec.category == category) && matches_query(snapshot, spec, query)
}

fn matches_query(snapshot: &Snapshot, spec: Spec, query: &str) -> bool {
    for token in query.split(|c: char| matches!(c, ' ' | '\t' | '\r' | '\n')) {
        if token.is_empty() {
            continue;
        }
        if !token_matches(snapshot, spec, token) {
            return false;
        }
    }
    true
}

fn token_matches(snapshot: &Snapshot, spec: Spec, token: &str) -> bool {
    contains_ignore_ascii_case(spec.label, token)
        || contains_ignore_ascii_case(spec.description, token)
        || contains_ignore_ascii_case(snapshot.value(spec.id), token)
        || contains_ignore_ascii_case(spec.id.tag_name(), token)
}

fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let hay = haystack.as_bytes();
    let needle = needle.as_bytes();
    if hay.len() < needle.len() {
        return false;
    }
    (0..=hay.len() - needle.len()).any(|index| hay[index..index + needle.len()].eq_ignore_ascii_case(needle))
}

fn on_off(value: bool) -> &'static str {
    if value {
        "on"
    } else {
        "off"
    }
}

pub fn notification_level(turn_end: bool, attention_required: bool, max: bool) -> &'static str {
    if !turn_end && !attention_required {
        return "off";
    }
    if turn_end && attention_required {
        return if max { "max" } else { "on" };
    }
    "custom"
}

fn update_window_start(start: usize, count: usize, selected: usize, visible: u16) -> usize {
    let width = (visible as usize).max(1);
    if count <= width {
        return 0;
    }
    if selected < start {
        return selected;
    }
    if selected >= start + width {
        return selected - width + 1;
    }
    start.min(count - width)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::shared::types::ReasoningEffort;

    fn effort(raw: &[u8]) -> ReasoningEffort {
        ReasoningEffort::literal(raw)
    }

    #[test]
    fn settings_catalog_projects_grouped_searchable_preferences() {
        let snapshot = Snapshot {
            model: "zai/glm-5.2",
            effort: "default",
            fast_mode: true,
            permission_mode: "ask",
            statusline_context: true,
            statusline_workspace: false,
            startup_scrollback: true,
            prompt_history: true,
            sound_level: "on",
            ..Snapshot::default()
        };

        assert_eq!(12, filtered_count(&snapshot, Category::All, ""));
        assert_eq!(5, filtered_count(&snapshot, Category::Interface, ""));
        assert_eq!(4, filtered_count(&snapshot, Category::Agent, ""));
        assert_eq!(1, filtered_count(&snapshot, Category::Notifications, ""));
        assert_eq!(2, filtered_count(&snapshot, Category::Advanced, ""));

        let current_model = item_at(&snapshot, Category::All, "glm 5.2", 0).unwrap();
        assert_eq!(SettingId::Model, current_model.id);
        assert_eq!("zai/glm-5.2", current_model.value);

        let startup = item_at(&snapshot, Category::All, "restore startup", 0).unwrap();
        assert_eq!(SettingId::StartupScrollback, startup.id);
        assert_eq!("on", startup.value);
        assert!(item_at(&snapshot, Category::All, "missing preference", 0).is_none());
    }

    #[test]
    fn settings_catalog_returns_typed_edit_choices_without_effects() {
        let efforts = [effort(b"future-tier"), effort(b"high")];
        let snapshot = Snapshot {
            model: "zai/glm-5.2",
            effort: "future-tier",
            reasoning_efforts: ReasoningEffortOptions::from_slice(&efforts),
            fast_mode: true,
            supports_fast_mode: true,
            permission_mode: "ask",
            statusline_context: true,
            startup_scrollback: true,
            prompt_history: true,
            sound_level: "on",
            ..Snapshot::default()
        };

        assert!(change_at(&snapshot, SettingId::Model, 0).is_none());
        assert_eq!(3, option_count(&snapshot, SettingId::PermissionMode));
        assert_eq!(
            Some("yolo"),
            option_at(&snapshot, SettingId::PermissionMode, 2)
        );
        assert_eq!(3, option_count(&snapshot, SettingId::Effort));
        assert_eq!(Some("default"), option_at(&snapshot, SettingId::Effort, 0));
        assert_eq!(
            Some("future-tier"),
            option_at(&snapshot, SettingId::Effort, 1)
        );
        assert_eq!(Some("high"), option_at(&snapshot, SettingId::Effort, 2));
        assert_eq!(Some(1), selected_option_index(&snapshot, SettingId::Effort));
    }

    #[test]
    fn settings_catalog_exposes_collapse_tool_calls_as_an_interface_toggle() {
        let expanded = Snapshot {
            collapse_tool_calls: false,
            ..Snapshot::default()
        };
        let item = item_at(&expanded, Category::Interface, "collapse tool calls", 0).unwrap();

        assert_eq!(SettingId::CollapseToolCalls, item.id);
        assert_eq!("Collapse tool calls", item.label);
        assert_eq!("off", item.value);
        let collapse = change_at(&expanded, SettingId::CollapseToolCalls, 1).unwrap();
        assert_eq!(SettingId::CollapseToolCalls, collapse.setting);
        assert_eq!("on", collapse.value);
    }

    #[test]
    fn settings_catalog_exposes_slash_menu_categories_as_an_interface_toggle() {
        let shown = Snapshot {
            slash_menu_categories: true,
            ..Snapshot::default()
        };
        let item = item_at(&shown, Category::Interface, "slash menu categories", 0).unwrap();

        assert_eq!(SettingId::SlashMenuCategories, item.id);
        assert_eq!("Slash menu categories", item.label);
        assert_eq!("on", item.value);
        assert_eq!(2, option_count(&shown, SettingId::SlashMenuCategories));
        assert_eq!(
            Some("off"),
            option_at(&shown, SettingId::SlashMenuCategories, 0)
        );
        assert_eq!(
            Some("on"),
            option_at(&shown, SettingId::SlashMenuCategories, 1)
        );

        let hide = change_at(&shown, SettingId::SlashMenuCategories, 0).unwrap();
        assert_eq!(SettingId::SlashMenuCategories, hide.setting);
        assert_eq!("off", hide.value);
    }

    #[test]
    fn settings_menu_navigates_rows_and_changes_selected_values_inline() {
        let snapshot = Snapshot {
            model: "zai/glm-5.2",
            effort: "default",
            fast_mode: true,
            permission_mode: "ask",
            statusline_context: true,
            startup_scrollback: true,
            prompt_history: true,
            sound_level: "on",
            ..Snapshot::default()
        };

        let mut menu = Menu::default();
        menu.open();
        assert!(menu.active);
        assert_eq!(Category::All, menu.category);
        assert!(menu.move_selection(&snapshot, "", 1, 5));
        assert_eq!(
            SettingId::StatuslineSession,
            menu.selected_item(&snapshot, "").unwrap().id
        );

        assert!(menu.cycle_category(1));
        assert_eq!(Category::Interface, menu.category);
        assert_eq!(0, menu.selected_index);

        let previous = menu.change_selected_option(&snapshot, "", -1).unwrap();
        assert_eq!(SettingId::StatuslineContext, previous.setting);
        assert_eq!("off", previous.value);
        let next = menu.change_selected_option(&snapshot, "", 1).unwrap();
        assert_eq!("off", next.value);

        menu.category = Category::Agent;
        menu.selected_index = 0;
        assert!(menu.change_selected_option(&snapshot, "", 1).is_none());

        menu.open_with_startup_scrollback(false);
        assert!(!menu.startup_scrollback);
    }

    #[test]
    fn notification_level_keeps_the_existing_off_on_and_max_policy() {
        assert_eq!("off", notification_level(false, false, false));
        assert_eq!("on", notification_level(true, true, false));
        assert_eq!("max", notification_level(true, true, true));
        assert_eq!("custom", notification_level(true, false, false));
    }

    #[test]
    fn status_line_menu_describes_toggle_changes_without_performing_effects() {
        let mut menu = StatuslineMenu::default();
        menu.open();

        let disabled = Snapshot {
            statusline_context: false,
            statusline_workspace: false,
            ..Snapshot::default()
        };
        let enable_context = menu.selected_change(&disabled).unwrap();
        assert_eq!(SettingId::StatuslineContext, enable_context.setting);
        assert_eq!("on", enable_context.value);

        let enabled = Snapshot {
            statusline_context: true,
            ..Snapshot::default()
        };
        assert_eq!("off", menu.selected_change(&enabled).unwrap().value);

        assert!(menu.move_selection(1));
        let enable_session = menu.selected_change(&enabled).unwrap();
        assert_eq!(SettingId::StatuslineSession, enable_session.setting);
        assert_eq!("on", enable_session.value);

        assert!(menu.move_selection(1));
        let enable_workspace = menu.selected_change(&enabled).unwrap();
        assert_eq!(SettingId::StatuslineWorkspace, enable_workspace.setting);
        assert_eq!("on", enable_workspace.value);

        menu.close();
        assert!(menu.selected_change(&enabled).is_none());
    }
}
