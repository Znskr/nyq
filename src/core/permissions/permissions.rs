//! Permission grant bookkeeping and the configured rule engine.
//!
//! Ported from `zig-reference/src/core/permissions/permissions.zig`
//! (grant/rule matching core; the file-mutation target walker is a later
//! slice of the same file).

use crate::core::execution::command_environment::{
    command_from_permission_identity, is_explicit_permission_command_identity,
};
use crate::core::shared::types::{
    PermissionAction, PermissionGrant, PermissionMode, PermissionRule, PermissionRuleSet,
    RuleDecision, ToolPermissionDecision,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionCallTarget {
    pub role: Vec<u8>,
    pub path: Vec<u8>,
}

pub const WEB_SEARCH_PERMISSION: &[u8] = b"web_search";
pub const WEB_FETCH_PERMISSION: &[u8] = b"web_fetch";
pub const YOLO_WARNING_TEXT: &[u8] = b"YOLO enabled: fx permission checks disabled";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionTargetKind {
    None,
    PathExisting,
    PathOptionalExisting,
    PathCreateParent,
    PathExistingParent,
    CommandCwd,
    Url,
}

/// Session permission state: mode, grants, and configured rules.
pub struct PermissionEngine {
    pub mode: PermissionMode,
    pub grants: Vec<PermissionGrant>,
    pub rules: PermissionRuleSet,
}

impl Default for PermissionEngine {
    fn default() -> Self {
        Self {
            mode: PermissionMode::Ask,
            grants: Vec::new(),
            rules: PermissionRuleSet::default(),
        }
    }
}

impl PermissionEngine {
    pub fn clear(&mut self) {
        clear_allowed_tools(&mut self.grants);
    }

    pub fn is_allowed(&self, tool_name: &[u8], target_path: &[u8]) -> bool {
        is_tool_allowed(&self.grants, tool_name, target_path)
    }

    pub fn allow(&mut self, permission_name: &[u8], pattern: &[u8]) {
        allow_tool_for_session(&mut self.grants, permission_name, pattern);
    }

    pub fn replace_rules(&mut self, next_rules: PermissionRuleSet) {
        self.rules = next_rules;
    }

    pub fn configured_rule_decision(
        &self,
        workspace_root: &[u8],
        tool_name: &[u8],
        target_path: &[u8],
        target_kind: PermissionTargetKind,
    ) -> RuleDecision {
        rule_decision_for(
            &self.rules,
            workspace_root,
            tool_name,
            target_path,
            target_kind,
        )
    }
}

pub fn is_web_search_tool_name(tool_name: &[u8]) -> bool {
    tool_name == WEB_SEARCH_PERMISSION
}

pub fn is_web_fetch_tool_name(tool_name: &[u8]) -> bool {
    tool_name == WEB_FETCH_PERMISSION
}

pub fn permission_mode_label(mode: PermissionMode) -> &'static [u8] {
    match mode {
        PermissionMode::Ask => b"ask",
        PermissionMode::Auto => b"auto",
        PermissionMode::Yolo => b"yolo",
    }
}

pub fn permission_decision_from_index(index: u8) -> ToolPermissionDecision {
    match index {
        0 => ToolPermissionDecision::Once,
        1 => ToolPermissionDecision::Always,
        _ => ToolPermissionDecision::Deny,
    }
}

/// Drops every grant and leaves the list ready for reuse. The Zig source
/// frees each owned pair; the Rust port simply clears the vector.
pub fn clear_allowed_tools(grants: &mut Vec<PermissionGrant>) {
    grants.clear();
}

pub fn is_tool_allowed(
    grants: &[PermissionGrant],
    tool_name: &[u8],
    target_path: &[u8],
) -> bool {
    grants
        .iter()
        .any(|entry| entry.tool_name == tool_name && entry.target_path == target_path)
}

pub fn allow_tool_for_session(
    grants: &mut Vec<PermissionGrant>,
    permission_name: &[u8],
    pattern: &[u8],
) {
    if is_tool_allowed(grants, permission_name, pattern) {
        return;
    }
    grants.push(PermissionGrant {
        tool_name: permission_name.to_vec(),
        target_path: pattern.to_vec(),
    });
}

pub fn is_filesystem_mutation_tool(tool_name: &[u8]) -> bool {
    tool_name == b"write_file" || tool_name == b"edit_file"
}

pub fn display_target_for_policy(
    workspace_root: &[u8],
    target_path: &[u8],
    target_kind: PermissionTargetKind,
) -> Vec<u8> {
    match target_kind {
        PermissionTargetKind::CommandCwd => display_command_target(workspace_root, target_path),
        PermissionTargetKind::PathExisting
        | PermissionTargetKind::PathOptionalExisting
        | PermissionTargetKind::PathCreateParent
        | PermissionTargetKind::PathExistingParent => {
            display_path_target(workspace_root, target_path)
        }
        _ => target_path.to_vec(),
    }
}

pub fn rule_decision_for(
    rules: &PermissionRuleSet,
    workspace_root: &[u8],
    tool_name: &[u8],
    target_path: &[u8],
    target_kind: PermissionTargetKind,
) -> RuleDecision {
    let permission = permission_name_for_tool(tool_name);
    let pattern = pattern_for_rule_match(workspace_root, tool_name, target_path, target_kind);
    evaluate_ruleset_for_tool(&rules.rules, permission, Some(tool_name), &pattern)
        .unwrap_or(RuleDecision::None)
}

pub fn rule_decision_for_permission_pattern(
    rules: &PermissionRuleSet,
    permission: &[u8],
    pattern: &[u8],
    fallback: RuleDecision,
) -> RuleDecision {
    evaluate_ruleset(&rules.rules, permission, pattern).unwrap_or(fallback)
}

pub fn rules_deny_all_targets_for_permission(
    rules: &PermissionRuleSet,
    permission: &[u8],
) -> bool {
    rules_deny_all_targets_for_permission_and_tool(rules, permission, None)
}

pub fn rules_deny_all_targets_for_tool(rules: &PermissionRuleSet, tool_name: &[u8]) -> bool {
    if is_web_fetch_tool_name(tool_name) {
        return false;
    }
    rules_deny_all_targets_for_permission_and_tool(
        rules,
        permission_name_for_tool(tool_name),
        Some(tool_name),
    )
}

fn rules_deny_all_targets_for_permission_and_tool(
    rules: &PermissionRuleSet,
    permission: &[u8],
    tool_name: Option<&[u8]>,
) -> bool {
    let mut last_global_deny_index: Option<usize> = None;
    for (index, rule) in rules.rules.iter().enumerate() {
        if !rule_matches_permission(&rule.permission, permission, tool_name) {
            continue;
        }
        if !is_global_target_pattern(&rule.pattern) {
            continue;
        }
        last_global_deny_index = match rule.action {
            PermissionAction::Allow | PermissionAction::Ask => None,
            PermissionAction::Deny => Some(index),
        };
    }

    let Some(deny_index) = last_global_deny_index else {
        return false;
    };
    for rule in &rules.rules[deny_index + 1..] {
        if !rule_matches_permission(&rule.permission, permission, tool_name) {
            continue;
        }
        if let PermissionAction::Allow | PermissionAction::Ask = rule.action {
            let decision = evaluate_ruleset_for_tool(
                &rules.rules,
                permission,
                tool_name,
                &rule.pattern,
            )
            .unwrap_or(RuleDecision::None);
            if decision == RuleDecision::Allow || decision == RuleDecision::Ask {
                return false;
            }
        }
    }

    true
}

pub fn session_grant_allowed(
    grants: &[PermissionGrant],
    tool_name: &[u8],
    target_path: &[u8],
) -> bool {
    let permission = permission_name_for_tool(tool_name);
    let pattern = pattern_for_session_grant_match(tool_name, target_path);

    if permission == WEB_FETCH_PERMISSION {
        if !is_canonical_web_fetch_domain_pattern(pattern) {
            return false;
        }
        for grant in grants {
            if grant.tool_name != WEB_FETCH_PERMISSION {
                continue;
            }
            if !is_canonical_web_fetch_domain_pattern(&grant.target_path) {
                continue;
            }
            if grant.target_path == pattern {
                return true;
            }
        }
        return false;
    }

    for grant in grants {
        if !permission_pattern_matches_tool(&grant.tool_name, permission, Some(tool_name)) {
            continue;
        }
        if permission == b"bash" {
            if grant.target_path != pattern {
                continue;
            }
        } else if !permission_pattern_matches_target(&grant.target_path, pattern) {
            continue;
        }
        return true;
    }
    false
}

const PATH_ALWAYS_PERMISSIONS: [&[u8]; 4] = [b"edit", b"read", b"glob", b"grep"];

pub fn suggested_session_grants(
    workspace_root: &[u8],
    tool_name: &[u8],
    target_path: &[u8],
    target_kind: PermissionTargetKind,
) -> Vec<PermissionGrant> {
    let permission = permission_name_for_tool(tool_name);
    let mut grants: Vec<PermissionGrant> = Vec::new();

    if permission == b"bash" {
        let command = pattern_for_session_grant_match(tool_name, target_path);
        append_owned_grant(&mut grants, b"bash", command);
        return grants;
    }

    if is_path_based_tool(target_kind)
        && !workspace_root.is_empty()
        && crate::core::workspace::pathing::path_inside(workspace_root, target_path)
    {
        let workspace_pattern = directory_tree_pattern(workspace_root);
        for path_permission in PATH_ALWAYS_PERMISSIONS {
            append_owned_grant(&mut grants, path_permission, &workspace_pattern);
        }
        return grants;
    }

    if is_path_based_tool(target_kind) {
        if let Some(external_root) = external_path_grant_root(workspace_root, target_path) {
            let external_pattern = directory_tree_pattern(&external_root);
            append_owned_grant(&mut grants, permission, &external_pattern);
            return grants;
        }
    }

    append_owned_grant(
        &mut grants,
        permission,
        pattern_for_session_grant_match(tool_name, target_path),
    );
    grants
}

fn is_structured_file_permission(permission: &[u8]) -> bool {
    PATH_ALWAYS_PERMISSIONS
        .iter()
        .any(|candidate| permission == *candidate)
}

fn append_normalized_file_grant(
    grants: &mut Vec<PermissionGrant>,
    permission: &[u8],
    pattern: &[u8],
) {
    let mut index = 0;
    while index < grants.len() {
        let current = &grants[index];
        if current.tool_name != permission {
            index += 1;
            continue;
        }
        if current.target_path == pattern
            || directory_tree_pattern_contains(&current.target_path, pattern)
        {
            return;
        }
        if directory_tree_pattern_contains(pattern, &current.target_path) {
            grants.remove(index);
            continue;
        }
        index += 1;
    }
    append_owned_grant(grants, permission, pattern);
}

fn directory_tree_pattern_contains(broader_pattern: &[u8], narrower_pattern: &[u8]) -> bool {
    let Some(broader_root) = directory_tree_root(broader_pattern) else {
        return false;
    };
    let Some(narrower_root) = directory_tree_root(narrower_pattern) else {
        return false;
    };
    broader_root == narrower_root
        || crate::core::workspace::pathing::path_inside(broader_root, narrower_root)
}

fn directory_tree_root(pattern: &[u8]) -> Option<&[u8]> {
    let root = pattern.strip_suffix(b"/**")?;
    Some(if root.is_empty() { b"/" } else { root })
}

fn literal_directory_tree_root(pattern: &[u8]) -> Option<&[u8]> {
    let root = directory_tree_root(pattern)?;
    if root.contains(&b'*') || root.contains(&b'?') {
        return None;
    }
    Some(root)
}

fn append_owned_grant(grants: &mut Vec<PermissionGrant>, permission_name: &[u8], pattern: &[u8]) {
    grants.push(PermissionGrant {
        tool_name: permission_name.to_vec(),
        target_path: pattern.to_vec(),
    });
}

pub fn permission_name_for_tool(tool_name: &[u8]) -> &[u8] {
    match tool_name {
        b"read_file" => b"read",
        b"write_file" | b"edit_file" => b"edit",
        b"glob_files" => b"glob",
        b"grep_files" => b"grep",
        b"run_command" => b"bash",
        name if is_web_fetch_tool_name(name) => WEB_FETCH_PERMISSION,
        b"skill" | b"install_skill" => b"skill",
        other => other,
    }
}

pub fn permission_rule_category_for_grant(permission_name: &[u8]) -> &[u8] {
    permission_name_for_tool(permission_name)
}

/// Returns the persistent rule pattern for a session grant.
pub fn permission_rule_pattern_for_grant(
    workspace_root: &[u8],
    permission_name: &[u8],
    pattern: &[u8],
) -> Vec<u8> {
    let permission = permission_name_for_tool(permission_name);
    if is_web_search_tool_name(permission) {
        return b"*".to_vec();
    }
    if permission == WEB_FETCH_PERMISSION {
        return canonical_web_fetch_domain_pattern(pattern).unwrap_or_else(|_| pattern.to_vec());
    }
    if permission == b"bash" {
        if is_explicit_permission_command_identity(pattern) {
            return pattern.to_vec();
        }
        if let Some(separator) = find(pattern, b"::") {
            return pattern[separator + 2..].to_vec();
        }
        return pattern.to_vec();
    }

    if permission == b"url" || permission == b"skill" {
        return pattern.to_vec();
    }

    if is_absolute(pattern) && crate::core::workspace::pathing::path_inside(workspace_root, pattern)
    {
        if directory_tree_pattern_matches(pattern, workspace_root) {
            return b"*".to_vec();
        }

        if let Some(relative) = relative_path(workspace_root, pattern) {
            if relative.is_empty() {
                return b"*".to_vec();
            }
            return relative;
        }
        return pattern.to_vec();
    }

    pattern.to_vec()
}

fn pattern_for_rule_match(
    workspace_root: &[u8],
    tool_name: &[u8],
    target_path: &[u8],
    target_kind: PermissionTargetKind,
) -> Vec<u8> {
    let permission = permission_name_for_tool(tool_name);
    if permission == WEB_FETCH_PERMISSION {
        return target_path.to_vec();
    }
    if permission == b"bash" {
        let identity = if is_explicit_permission_command_identity(target_path) {
            target_path
        } else if let Some(separator) = find(target_path, b"::") {
            &target_path[separator + 2..]
        } else {
            target_path
        };
        return command_from_permission_identity(identity).to_vec();
    }
    display_target_for_policy(workspace_root, target_path, target_kind)
}

pub fn pattern_for_session_grant_match<'a>(tool_name: &[u8], target_path: &'a [u8]) -> &'a [u8] {
    let permission = permission_name_for_tool(tool_name);
    if permission == b"bash" {
        if is_explicit_permission_command_identity(target_path) {
            return target_path;
        }
        if let Some(separator) = find(target_path, b"::") {
            return &target_path[separator + 2..];
        }
    }
    target_path
}

fn is_path_based_tool(target_kind: PermissionTargetKind) -> bool {
    matches!(
        target_kind,
        PermissionTargetKind::PathExisting
            | PermissionTargetKind::PathOptionalExisting
            | PermissionTargetKind::PathCreateParent
            | PermissionTargetKind::PathExistingParent
    )
}

fn external_path_grant_root(workspace_root: &[u8], target_path: &[u8]) -> Option<Vec<u8>> {
    if !is_absolute(target_path) {
        return None;
    }
    if !workspace_root.is_empty()
        && crate::core::workspace::pathing::path_inside(workspace_root, target_path)
    {
        return None;
    }

    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        if std::fs::metadata(std::ffi::OsStr::from_bytes(target_path))
            .ok()
            .is_some_and(|m| m.is_dir())
        {
            return Some(target_path.to_vec());
        }
    }
    #[cfg(not(unix))]
    {
        let _ = target_path;
    }

    let parent = parent_path(target_path)?;
    Some(parent.to_vec())
}

fn directory_tree_pattern(dir_path: &[u8]) -> Vec<u8> {
    if dir_path.ends_with(b"/") {
        let mut out = dir_path.to_vec();
        out.extend_from_slice(b"**");
        return out;
    }
    let mut out = dir_path.to_vec();
    out.extend_from_slice(b"/**");
    out
}

fn evaluate_ruleset(
    rules: &[PermissionRule],
    permission: &[u8],
    pattern: &[u8],
) -> Option<RuleDecision> {
    evaluate_ruleset_for_tool(rules, permission, None, pattern)
}

fn evaluate_ruleset_for_tool(
    rules: &[PermissionRule],
    permission: &[u8],
    tool_name: Option<&[u8]>,
    pattern: &[u8],
) -> Option<RuleDecision> {
    if permission == WEB_FETCH_PERMISSION {
        return evaluate_web_fetch_ruleset(rules, pattern);
    }

    let mut matched: Option<RuleDecision> = None;
    for rule in rules {
        if !rule_matches_permission(&rule.permission, permission, tool_name) {
            continue;
        }
        let action: RuleDecision = match rule.action {
            PermissionAction::Allow => RuleDecision::Allow,
            PermissionAction::Ask => RuleDecision::Ask,
            PermissionAction::Deny => RuleDecision::Deny,
        };
        if !rule_target_matches(permission, action, &rule.pattern, pattern) {
            continue;
        }
        matched = Some(action);
    }
    matched
}

fn evaluate_web_fetch_ruleset(rules: &[PermissionRule], pattern: &[u8]) -> Option<RuleDecision> {
    if !is_canonical_web_fetch_domain_pattern(pattern) {
        return None;
    }

    let mut matched: Option<RuleDecision> = None;
    for rule in rules {
        if rule.permission != WEB_FETCH_PERMISSION {
            continue;
        }
        if !is_canonical_web_fetch_domain_pattern(&rule.pattern) {
            continue;
        }
        if rule.pattern != pattern {
            continue;
        }
        matched = Some(match rule.action {
            PermissionAction::Allow => RuleDecision::Allow,
            PermissionAction::Ask => RuleDecision::Ask,
            PermissionAction::Deny => RuleDecision::Deny,
        });
    }
    matched
}

pub fn web_fetch_rule_warning_count(rules: &[PermissionRule]) -> usize {
    rules
        .iter()
        .filter(|rule| rule.permission == WEB_FETCH_PERMISSION)
        .filter(|rule| !is_canonical_web_fetch_domain_pattern(&rule.pattern))
        .count()
}

fn rule_matches_permission(
    rule_permission: &[u8],
    permission: &[u8],
    tool_name: Option<&[u8]>,
) -> bool {
    if wildcard_match(rule_permission, permission) {
        return true;
    }
    let Some(name) = tool_name else {
        return false;
    };
    wildcard_match(rule_permission, name)
}

fn permission_pattern_matches_tool(
    pattern: &[u8],
    permission: &[u8],
    tool_name: Option<&[u8]>,
) -> bool {
    rule_matches_permission(pattern, permission, tool_name)
}

fn permission_pattern_matches_target(pattern: &[u8], candidate: &[u8]) -> bool {
    if directory_tree_pattern_matches(pattern, candidate) {
        return true;
    }
    wildcard_match(pattern, candidate)
}

fn rule_target_matches(
    permission: &[u8],
    action: RuleDecision,
    pattern: &[u8],
    candidate: &[u8],
) -> bool {
    if action != RuleDecision::Allow || permission != b"bash" {
        return permission_pattern_matches_target(pattern, candidate);
    }

    if !contains_wildcard(pattern) {
        return pattern == candidate;
    }
    if !is_static_command(pattern, true) || !is_static_command(candidate, false) {
        return false;
    }
    static_command_wildcard_match(pattern, candidate)
}

fn contains_wildcard(pattern: &[u8]) -> bool {
    pattern.contains(&b'*') || pattern.contains(&b'?')
}

fn is_static_command(command: &[u8], allow_wildcards: bool) -> bool {
    if command.is_empty() {
        return false;
    }
    if first_word_is_assignment(command) {
        return false;
    }

    let mut index = 0;
    let mut in_word = false;
    while index < command.len() {
        let ch = command[index];
        if ch == b' ' || ch == b'\t' {
            if !in_word {
                return false;
            }
            while index < command.len() && (command[index] == b' ' || command[index] == b'\t') {
                index += 1;
            }
            if index == command.len() {
                return false;
            }
            in_word = false;
            continue;
        }

        if ch == b'\'' {
            let literal_start = index + 1;
            let Some(literal_end) = find_from(command, literal_start, b"'") else {
                return false;
            };
            let literal = &command[literal_start..literal_end];
            if std::str::from_utf8(literal).is_err() {
                return false;
            }
            if literal.contains(&0)
                || literal.contains(&b'\r')
                || literal.contains(&b'\n')
            {
                return false;
            }
            in_word = true;
            index = literal_end + 1;
            continue;
        }

        if !is_static_unquoted_byte(ch)
            && !(allow_wildcards && (ch == b'*' || ch == b'?'))
        {
            return false;
        }
        in_word = true;
        index += 1;
    }
    in_word
}

fn first_word_is_assignment(command: &[u8]) -> bool {
    let first_word_end = command
        .iter()
        .position(|&c| c == b' ' || c == b'\t')
        .unwrap_or(command.len());
    let first_word = &command[..first_word_end];
    let Some(equals) = first_word.iter().position(|&c| c == b'=') else {
        return false;
    };
    let name = &first_word[..equals];
    if name.is_empty()
        || !(name[0].is_ascii_alphabetic() || name[0] == b'_')
    {
        return false;
    }
    name[1..]
        .iter()
        .all(|&c| c.is_ascii_alphanumeric() || c == b'_')
}

fn is_static_unquoted_byte(ch: u8) -> bool {
    ch.is_ascii_alphanumeric()
        || matches!(
            ch,
            b'_' | b'.' | b'/' | b',' | b':' | b'+' | b'=' | b'%' | b'@' | b'-'
        )
}

fn static_command_wildcard_match(pattern: &[u8], candidate: &[u8]) -> bool {
    let mut pattern_index = 0;
    let mut candidate_index = 0;
    let mut in_single_quote = false;
    let mut star_index: Option<usize> = None;
    let mut star_candidate_index = 0;

    while candidate_index < candidate.len() {
        if pattern_index < pattern.len() {
            let ch = pattern[pattern_index];
            if !in_single_quote && ch == b'*' {
                star_index = Some(pattern_index);
                pattern_index += 1;
                star_candidate_index = candidate_index;
                continue;
            }
            if (!in_single_quote && ch == b'?') || ch == candidate[candidate_index] {
                if ch == b'\'' {
                    in_single_quote = !in_single_quote;
                }
                pattern_index += 1;
                candidate_index += 1;
                continue;
            }
        }

        let Some(star) = star_index else {
            return false;
        };
        star_candidate_index += 1;
        candidate_index = star_candidate_index;
        pattern_index = star + 1;
        in_single_quote = false;
    }

    while pattern_index < pattern.len()
        && !in_single_quote
        && pattern[pattern_index] == b'*'
    {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

fn directory_tree_pattern_matches(pattern: &[u8], candidate: &[u8]) -> bool {
    if !pattern.ends_with(b"/**") {
        return false;
    }
    if pattern == b"/**" {
        return candidate.starts_with(b"/");
    }

    let dir = &pattern[..pattern.len() - b"/**".len()];
    if candidate == dir {
        return true;
    }
    candidate.len() > dir.len()
        && candidate.starts_with(dir)
        && candidate[dir.len()] == b'/'
}

fn wildcard_match(pattern: &[u8], candidate: &[u8]) -> bool {
    let mut pattern_index = 0;
    let mut candidate_index = 0;
    let mut star_index: Option<usize> = None;
    let mut star_candidate_index = 0;

    while candidate_index < candidate.len() {
        if pattern_index < pattern.len()
            && (pattern[pattern_index] == b'?'
                || pattern[pattern_index] == candidate[candidate_index])
        {
            pattern_index += 1;
            candidate_index += 1;
            continue;
        }
        if pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
            star_index = Some(pattern_index);
            pattern_index += 1;
            star_candidate_index = candidate_index;
            continue;
        }

        let Some(star) = star_index else {
            return false;
        };
        star_candidate_index += 1;
        candidate_index = star_candidate_index;
        pattern_index = star + 1;
    }

    while pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

fn is_global_target_pattern(pattern: &[u8]) -> bool {
    !pattern.is_empty() && pattern.iter().all(|&c| c == b'*')
}

pub fn web_fetch_domain_target_for_url(url: &[u8]) -> Result<Vec<u8>, ()> {
    let Some(scheme_end) = find(url, b"://") else {
        return Err(());
    };
    let authority_start = scheme_end + b"://".len();
    let authority_end = authority_end(url, authority_start);
    if authority_end == authority_start {
        return Err(());
    }

    let authority = &url[authority_start..authority_end];
    if authority.contains(&b'@') {
        return Err(());
    }
    let host = host_from_authority(authority).ok_or(())?;
    canonical_web_fetch_domain_pattern(host)
}

pub fn canonical_web_fetch_domain_pattern(raw: &[u8]) -> Result<Vec<u8>, ()> {
    let host_raw = raw.strip_prefix(b"domain:").unwrap_or(raw);
    if find(host_raw, b"://").is_some() {
        return Err(());
    }
    if host_raw.contains(&b'/') {
        return Err(());
    }
    if host_raw.contains(&b'*') {
        return Err(());
    }
    if host_raw.contains(&b'?') {
        return Err(());
    }

    let host = strip_one_root_dot(host_raw);
    if !is_canonicalizable_web_fetch_host(host) {
        return Err(());
    }

    let mut target = Vec::with_capacity(b"domain:".len() + host.len());
    target.extend_from_slice(b"domain:");
    for &ch in host {
        target.push(ch.to_ascii_lowercase());
    }
    Ok(target)
}

pub fn is_canonical_web_fetch_domain_pattern(pattern: &[u8]) -> bool {
    let Some(host) = pattern.strip_prefix(b"domain:") else {
        return false;
    };
    if !is_canonicalizable_web_fetch_host(host) {
        return false;
    }
    if host.ends_with(b".") {
        return false;
    }
    !host.iter().any(|&c| c.is_ascii_alphabetic() && c.is_ascii_uppercase())
}

fn strip_one_root_dot(host: &[u8]) -> &[u8] {
    if host.len() > 1 && host[host.len() - 1] == b'.' {
        return &host[..host.len() - 1];
    }
    host
}

fn is_canonicalizable_web_fetch_host(host: &[u8]) -> bool {
    if host.is_empty() {
        return false;
    }
    if host[0] == b'[' {
        return is_canonicalizable_web_fetch_ipv6_literal(host);
    }
    if host.contains(&b':') {
        return false;
    }
    let mut label_len = 0;
    for &ch in host {
        if ch <= 0x20 || ch >= 0x7f {
            return false;
        }
        if matches!(ch, b'*' | b'?' | b'/' | b'\\') {
            return false;
        }
        if ch == b'.' {
            if label_len == 0 {
                return false;
            }
            label_len = 0;
            continue;
        }
        if !ch.is_ascii_alphanumeric() && ch != b'-' {
            return false;
        }
        label_len += 1;
    }
    label_len > 0
}

fn is_canonicalizable_web_fetch_ipv6_literal(host: &[u8]) -> bool {
    if host.len() < 3 || host[host.len() - 1] != b']' {
        return false;
    }
    let inner = &host[1..host.len() - 1];
    if inner.contains(&b'%') {
        return false;
    }
    for &ch in inner {
        if ch <= 0x20 || ch >= 0x7f {
            return false;
        }
        if ch.is_ascii_alphabetic() && ch.is_ascii_uppercase() {
            return false;
        }
        if !ch.is_ascii_hexdigit() && ch != b':' && ch != b'.' {
            return false;
        }
    }
    // The Zig source validates through std.Io.net.Ip6Address.parse; the port
    // checks the same shape without pulling in an address parser.
    let mut groups = inner.split(|&c| c == b':').count();
    if inner.contains(&b'.') {
        // IPv4-mapped tail adds one group.
        groups = groups.saturating_sub(1);
    }
    (2..=8).contains(&groups) && inner.iter().all(|&c| c.is_ascii_hexdigit() || matches!(c, b':' | b'.'))
}

fn authority_end(url: &[u8], start: usize) -> usize {
    let mut index = start;
    while index < url.len() {
        if matches!(url[index], b'/' | b'?' | b'#') {
            return index;
        }
        index += 1;
    }
    url.len()
}

fn host_from_authority(authority: &[u8]) -> Option<&[u8]> {
    if authority.is_empty() {
        return None;
    }
    if authority[0] == b'[' {
        let end = authority.iter().position(|&c| c == b']')?;
        if end == 1 {
            return None;
        }
        if end + 1 < authority.len() && authority[end + 1] != b':' {
            return None;
        }
        return Some(&authority[..end + 1]);
    }

    let colon = authority.iter().rposition(|&c| c == b':').unwrap_or(authority.len());
    if colon == 0 {
        return None;
    }
    Some(&authority[..colon])
}

fn display_path_target(workspace_root: &[u8], target_path: &[u8]) -> Vec<u8> {
    if is_absolute(target_path)
        && crate::core::workspace::pathing::path_inside(workspace_root, target_path)
    {
        if let Some(relative) = relative_path(workspace_root, target_path) {
            if relative.is_empty() {
                return b".".to_vec();
            }
            return relative;
        }
    }
    target_path.to_vec()
}

fn display_command_target(workspace_root: &[u8], target_path: &[u8]) -> Vec<u8> {
    let Some(separator) = find(target_path, b"::") else {
        return target_path.to_vec();
    };
    let cwd = &target_path[..separator];
    let command = command_from_permission_identity(&target_path[separator + 2..]);
    let display_cwd = display_path_target(workspace_root, cwd);
    let mut out = display_cwd;
    out.extend_from_slice(b"::");
    out.extend_from_slice(command);
    out
}

// ---------------------------------------------------------------------------
// Byte helpers
// ---------------------------------------------------------------------------

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn find_from(haystack: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if from >= haystack.len() {
        return None;
    }
    find(&haystack[from..], needle).map(|index| index + from)
}

fn is_absolute(path: &[u8]) -> bool {
    path.first() == Some(&b'/')
}

fn parent_path(path: &[u8]) -> Option<&[u8]> {
    let index = path.iter().rposition(|&c| c == b'/')?;
    if index == 0 {
        return Some(b"/");
    }
    Some(&path[..index])
}

/// Lexical relative path from `root` to `path`, mirroring
/// `std.fs.path.relative` for the in-workspace absolute case.
fn relative_path(root: &[u8], path: &[u8]) -> Option<Vec<u8>> {
    let root_norm = normalize_components(root);
    let path_norm = normalize_components(path);
    let mut common = 0;
    while common < root_norm.len()
        && common < path_norm.len()
        && root_norm[common] == path_norm[common]
    {
        common += 1;
    }
    let mut out: Vec<u8> = Vec::new();
    for _ in common..root_norm.len() {
        if !out.is_empty() {
            out.push(b'/');
        }
        out.extend_from_slice(b"..");
    }
    for component in &path_norm[common..] {
        if !out.is_empty() {
            out.push(b'/');
        }
        out.extend_from_slice(component);
    }
    Some(out)
}

fn normalize_components(path: &[u8]) -> Vec<&[u8]> {
    path.split(|&c| c == b'/')
        .filter(|c| !c.is_empty() && *c != b".")
        .collect()
}

#[cfg(test)]
mod tests;
