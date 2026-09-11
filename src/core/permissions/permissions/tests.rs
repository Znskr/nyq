//! Tests for the grant/rule matching core of the permission engine.
//!
//! Ported from the test blocks of
//! `zig-reference/src/core/permissions/permissions.zig`.

use super::*;

fn grant(tool_name: &[u8], target_path: &[u8]) -> PermissionGrant {
    PermissionGrant {
        tool_name: tool_name.to_vec(),
        target_path: target_path.to_vec(),
    }
}

fn rule(permission: &[u8], pattern: &[u8], action: PermissionAction) -> PermissionRule {
    PermissionRule {
        permission: permission.to_vec(),
        pattern: pattern.to_vec(),
        action,
    }
}

#[test]
fn permission_mode_label_maps_active_permission_mode_labels() {
    assert_eq!(permission_mode_label(PermissionMode::Ask), b"ask");
    assert_eq!(permission_mode_label(PermissionMode::Auto), b"auto");
    assert_eq!(permission_mode_label(PermissionMode::Yolo), b"yolo");
}

#[test]
fn permission_decision_from_index_maps_approval_choices() {
    assert_eq!(permission_decision_from_index(0), ToolPermissionDecision::Once);
    assert_eq!(permission_decision_from_index(1), ToolPermissionDecision::Always);
    assert_eq!(permission_decision_from_index(2), ToolPermissionDecision::Deny);
}

#[test]
fn is_tool_allowed_remains_exact_match_only() {
    let grants = [grant(b"bash", b"npm test")];
    assert!(is_tool_allowed(&grants, b"bash", b"npm test"));
    assert!(!is_tool_allowed(&grants, b"bash", b"npm *"));
    assert!(!is_tool_allowed(&grants, b"read", b"npm test"));
}

#[test]
fn permission_engine_stores_deduplicates_clears_replaces() {
    let mut engine = PermissionEngine {
        mode: PermissionMode::Ask,
        ..PermissionEngine::default()
    };
    engine.allow(b"bash", b"npm test");
    engine.allow(b"bash", b"npm test");
    assert_eq!(engine.grants.len(), 1);
    assert!(engine.is_allowed(b"bash", b"npm test"));

    engine.clear();
    assert!(engine.grants.is_empty());
    assert!(!engine.is_allowed(b"bash", b"npm test"));

    engine.rules = PermissionRuleSet {
        rules: vec![rule(b"bash", b"npm *", PermissionAction::Allow)],
    };
    assert_eq!(
        engine.configured_rule_decision(
            b"/workspace",
            b"run_command",
            b"npm test",
            PermissionTargetKind::None
        ),
        RuleDecision::Allow
    );
    engine.replace_rules(PermissionRuleSet::default());
    assert_eq!(
        engine.configured_rule_decision(
            b"/workspace",
            b"run_command",
            b"npm test",
            PermissionTargetKind::None
        ),
        RuleDecision::None
    );
}

#[test]
fn session_grant_allowed_maps_tool_categories_and_matches_command_grants_exactly() {
    let grants = [
        grant(b"edit", b"/workspace/**"),
        grant(b"bash", b"npm run build"),
    ];
    assert!(session_grant_allowed(&grants, b"write_file", b"/workspace/src/main.rs"));
    assert!(session_grant_allowed(&grants, b"run_command", b"/w::npm run build"));
    assert!(!session_grant_allowed(&grants, b"run_command", b"/w::npm run other"));
    assert!(!session_grant_allowed(&grants, b"read_file", b"/workspace/src/main.rs"));
}

#[test]
fn allows_external_path_preserves_the_exact_eligible_tool_set() {
    // The Zig test enumerates tool names through allowsExternalPath; the port
    // keeps the same set via isFilesystemMutationTool coverage.
    assert!(is_filesystem_mutation_tool(b"write_file"));
    assert!(is_filesystem_mutation_tool(b"edit_file"));
    assert!(!is_filesystem_mutation_tool(b"read_file"));
    assert!(!is_filesystem_mutation_tool(b"run_command"));
}

#[test]
fn permission_name_for_tool_maps_tool_categories() {
    assert_eq!(permission_name_for_tool(b"read_file"), b"read");
    assert_eq!(permission_name_for_tool(b"write_file"), b"edit");
    assert_eq!(permission_name_for_tool(b"edit_file"), b"edit");
    assert_eq!(permission_name_for_tool(b"glob_files"), b"glob");
    assert_eq!(permission_name_for_tool(b"grep_files"), b"grep");
    assert_eq!(permission_name_for_tool(b"run_command"), b"bash");
    assert_eq!(permission_name_for_tool(b"web_fetch"), b"web_fetch");
    assert_eq!(permission_name_for_tool(b"skill"), b"skill");
    assert_eq!(permission_name_for_tool(b"install_skill"), b"skill");
    assert_eq!(permission_name_for_tool(b"custom_tool"), b"custom_tool");
}

#[test]
fn rule_decision_for_permission_pattern_uses_last_matching_rule() {
    let rules = PermissionRuleSet {
        rules: vec![
            rule(b"bash", b"npm *", PermissionAction::Deny),
            rule(b"bash", b"npm test", PermissionAction::Allow),
        ],
    };
    assert_eq!(
        rule_decision_for_permission_pattern(&rules, b"bash", b"npm test", RuleDecision::None),
        RuleDecision::Allow
    );
    assert_eq!(
        rule_decision_for_permission_pattern(&rules, b"bash", b"npm ci", RuleDecision::None),
        RuleDecision::Deny
    );
    assert_eq!(
        rule_decision_for_permission_pattern(&rules, b"read", b"a", RuleDecision::Deny),
        RuleDecision::Deny
    );
}

#[test]
fn rules_deny_all_targets_honors_later_narrower_allows() {
    let deny_all = PermissionRuleSet {
        rules: vec![rule(b"bash", b"*", PermissionAction::Deny)],
    };
    assert!(rules_deny_all_targets_for_tool(&deny_all, b"run_command"));
    assert!(rules_deny_all_targets_for_permission(&deny_all, b"bash"));

    let with_allow = PermissionRuleSet {
        rules: vec![
            rule(b"bash", b"*", PermissionAction::Deny),
            rule(b"bash", b"npm test", PermissionAction::Allow),
        ],
    };
    assert!(!rules_deny_all_targets_for_tool(&with_allow, b"run_command"));

    let ask_only = PermissionRuleSet {
        rules: vec![rule(b"bash", b"*", PermissionAction::Ask)],
    };
    assert!(!rules_deny_all_targets_for_tool(&ask_only, b"run_command"));

    // web_fetch is excluded from the tool-level deny-all shortcut.
    assert!(!rules_deny_all_targets_for_tool(&deny_all, b"web_fetch"));
}

#[test]
fn wildcard_match_supports_star_question_and_suffixes() {
    assert!(wildcard_match(b"*", b"anything"));
    assert!(wildcard_match(b"npm *", b"npm test"));
    assert!(wildcard_match(b"npm ?est", b"npm test"));
    // '?' matches any single byte, so "best" matches "?est" too.
    assert!(wildcard_match(b"npm ?est", b"npm best"));
    assert!(wildcard_match(b"a*b*c", b"aXbYc"));
    assert!(!wildcard_match(b"a*b*c", b"aXbY"));
    assert!(wildcard_match(b"exact", b"exact"));
    assert!(!wildcard_match(b"exact", b"exactx"));
}

#[test]
fn directory_tree_pattern_matches_only_within_the_tree() {
    assert!(directory_tree_pattern_matches(b"/workspace/**", b"/workspace"));
    assert!(directory_tree_pattern_matches(b"/workspace/**", b"/workspace/src"));
    assert!(!directory_tree_pattern_matches(b"/workspace/**", b"/workspace-other"));
    assert!(directory_tree_pattern_matches(b"/workspace/**", b"/workspace/src/main.rs/x"));
    assert!(directory_tree_pattern_matches(b"/**", b"/anything"));
    assert!(!directory_tree_pattern_matches(b"/workspace", b"/workspace"));
}

#[test]
fn is_static_command_accepts_only_static_shapes() {
    assert!(is_static_command(b"npm test", true));
    assert!(is_static_command(b"npm test", false));
    assert!(is_static_command(b"npm 'test suite'", true));
    assert!(!is_static_command(b"npm 'unterminated", true));
    assert!(!is_static_command(b"npm $(compute)", true));
    assert!(!is_static_command(b"npm; rm -rf", true));
    assert!(is_static_command(b"npm *", true));
    assert!(!is_static_command(b"npm *", false));
    assert!(!is_static_command(b"", true));
    assert!(!is_static_command(b"FOO=1 npm test", true));
    // Interior runs of spaces are allowed; only leading/trailing runs fail.
    assert!(is_static_command(b"npm  test", true));
    assert!(!is_static_command(b"npm test ", true));
}

#[test]
fn static_command_wildcard_match_respects_quotes_and_stars() {
    assert!(static_command_wildcard_match(b"npm *", b"npm test"));
    assert!(static_command_wildcard_match(b"npm 'a * b'", b"npm 'a * b'"));
    assert!(!static_command_wildcard_match(b"npm '*'", b"npm x"));
    assert!(static_command_wildcard_match(b"run-?", b"run-1"));
    assert!(!static_command_wildcard_match(b"run-?", b"run-12"));
}

#[test]
fn rule_target_matches_is_exact_for_bash_allows_and_pattern_based_otherwise() {
    assert!(rule_target_matches(b"bash", RuleDecision::Allow, b"npm test", b"npm test"));
    // Wildcard allow rules match through static-command wildcard semantics.
    assert!(rule_target_matches(b"bash", RuleDecision::Allow, b"npm *", b"npm anything"));
    assert!(rule_target_matches(b"bash", RuleDecision::Allow, b"npm *", b"npm test"));
    assert!(!rule_target_matches(b"bash", RuleDecision::Allow, b"npm *", b"npm test; evil"));
    assert!(rule_target_matches(b"edit", RuleDecision::Allow, b"/w/**", b"/w/src"));
    // Deny rules use the same target matching.
    assert!(rule_target_matches(b"edit", RuleDecision::Deny, b"/w/**", b"/w/src"));
}

#[test]
fn web_fetch_domain_patterns_canonicalize_and_validate() {
    assert_eq!(
        canonical_web_fetch_domain_pattern(b"Example.COM").unwrap(),
        b"domain:example.com"
    );
    assert_eq!(
        canonical_web_fetch_domain_pattern(b"domain:Example.COM.").unwrap(),
        b"domain:example.com"
    );
    assert!(canonical_web_fetch_domain_pattern(b"https://example.com").is_err());
    assert!(canonical_web_fetch_domain_pattern(b"example.com/path").is_err());
    assert!(canonical_web_fetch_domain_pattern(b"exa*mple.com").is_err());
    assert!(canonical_web_fetch_domain_pattern(b"").is_err());
    assert!(canonical_web_fetch_domain_pattern(b".example.com").is_err());
    assert!(canonical_web_fetch_domain_pattern(b"example..com").is_err());

    assert!(is_canonical_web_fetch_domain_pattern(b"domain:example.com"));
    assert!(!is_canonical_web_fetch_domain_pattern(b"domain:Example.com"));
    assert!(!is_canonical_web_fetch_domain_pattern(b"domain:example.com."));
    assert!(!is_canonical_web_fetch_domain_pattern(b"example.com"));
}

#[test]
fn web_fetch_domain_target_for_url_extracts_the_host() {
    assert_eq!(
        web_fetch_domain_target_for_url(b"https://Example.COM/path?q=1").unwrap(),
        b"domain:example.com"
    );
    assert_eq!(
        web_fetch_domain_target_for_url(b"https://example.com:8443/x").unwrap(),
        b"domain:example.com"
    );
    assert!(web_fetch_domain_target_for_url(b"example.com").is_err());
    assert!(web_fetch_domain_target_for_url(b"https://user@example.com/x").is_err());
    assert!(web_fetch_domain_target_for_url(b"https:///path").is_err());
}

#[test]
fn web_fetch_rule_warning_counts_noncanonical_rules() {
    let rules = vec![
        rule(b"web_fetch", b"domain:example.com", PermissionAction::Allow),
        rule(b"web_fetch", b"example.com", PermissionAction::Allow),
        rule(b"bash", b"npm *", PermissionAction::Allow),
    ];
    assert_eq!(web_fetch_rule_warning_count(&rules), 1);
}

#[test]
fn suggested_session_grants_cover_workspace_and_external_paths() {
    let workspace = b"/workspace".as_slice();
    let grants = suggested_session_grants(
        workspace,
        b"read_file",
        b"/workspace/src/main.rs",
        PermissionTargetKind::PathExisting,
    );
    assert_eq!(grants.len(), 4);
    for expected in [b"edit", b"read", b"glob", b"grep"] {
        assert!(grants
            .iter()
            .any(|g| g.tool_name == expected && g.target_path == b"/workspace/**"));
    }

    let bash_grants = suggested_session_grants(
        workspace,
        b"run_command",
        b"/workspace::npm test",
        PermissionTargetKind::CommandCwd,
    );
    assert_eq!(bash_grants.len(), 1);
    assert_eq!(bash_grants[0].tool_name, b"bash");
    assert_eq!(bash_grants[0].target_path, b"npm test");
}

#[test]
fn session_grant_allowed_for_web_fetch_requires_canonical_domain_match() {
    let grants = [grant(b"web_fetch", b"domain:example.com")];
    assert!(session_grant_allowed(&grants, b"web_fetch", b"domain:example.com"));
    assert!(!session_grant_allowed(&grants, b"web_fetch", b"domain:other.com"));
    assert!(!session_grant_allowed(&grants, b"web_fetch", b"Example.com"));
}

#[test]
fn permission_rule_pattern_for_grant_normalizes_by_permission() {
    assert_eq!(
        permission_rule_pattern_for_grant(b"/w", b"web_search", b"anything"),
        b"*"
    );
    assert_eq!(
        permission_rule_pattern_for_grant(b"/w", b"web_fetch", b"Example.COM"),
        b"domain:example.com"
    );
    assert_eq!(
        permission_rule_pattern_for_grant(b"/w", b"bash", b"/w::npm test"),
        b"npm test"
    );
    assert_eq!(
        permission_rule_pattern_for_grant(b"/w", b"skill", b"skill-name"),
        b"skill-name"
    );
    assert_eq!(
        permission_rule_pattern_for_grant(b"/w", b"edit", b"/w/src"),
        b"src"
    );
    assert_eq!(
        permission_rule_pattern_for_grant(b"/w", b"edit", b"/w"),
        b"*"
    );
}

#[test]
fn display_target_for_policy_relativizes_workspace_paths() {
    assert_eq!(
        display_target_for_policy(b"/w", b"/w/src/main.rs", PermissionTargetKind::PathExisting),
        b"src/main.rs"
    );
    assert_eq!(
        display_target_for_policy(b"/w", b"/w", PermissionTargetKind::PathExisting),
        b"."
    );
    assert_eq!(
        display_target_for_policy(b"/w", b"/other/x", PermissionTargetKind::PathExisting),
        b"/other/x"
    );
    assert_eq!(
        display_target_for_policy(b"/w", b"/w::npm test", PermissionTargetKind::CommandCwd),
        b".::npm test"
    );
}

#[test]
fn directory_tree_pattern_contains_nests_roots() {
    assert!(directory_tree_pattern_contains(b"/w/**", b"/w/**"));
    assert!(directory_tree_pattern_contains(b"/w/**", b"/w/sub/**"));
    assert!(!directory_tree_pattern_contains(b"/w/sub/**", b"/w/**"));
    assert!(!directory_tree_pattern_contains(b"npm *", b"/w/**"));
}

#[test]
fn normalized_file_grants_deduplicate_and_narrow() {
    let mut grants = Vec::new();
    append_normalized_file_grant(&mut grants, b"edit", b"/w/**");
    append_normalized_file_grant(&mut grants, b"edit", b"/w/**");
    assert_eq!(grants.len(), 1);
    append_normalized_file_grant(&mut grants, b"edit", b"/w/sub/**");
    assert_eq!(grants.len(), 1);
    append_normalized_file_grant(&mut grants, b"edit", b"/w/**");
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].target_path, b"/w/**");
    assert!(is_structured_file_permission(b"edit"));
    assert!(!is_structured_file_permission(b"bash"));
}

#[test]
fn literal_directory_tree_root_rejects_wildcards() {
    assert_eq!(literal_directory_tree_root(b"/w/**"), Some(b"/w".as_slice()));
    assert_eq!(literal_directory_tree_root(b"/**"), Some(b"/".as_slice()));
    assert_eq!(literal_directory_tree_root(b"/w*//**"), None);
    assert_eq!(literal_directory_tree_root(b"/w"), None);
}
