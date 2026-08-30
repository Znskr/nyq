//! Names of directories that workspace discovery skips by default.

#[allow(non_upper_case_globals)]
pub const ignored_directory_names: &[&str] = &[
    ".git",
    ".zig-cache",
    "zig-out",
    "node_modules",
    ".next",
    "dist",
    "build",
    "coverage",
];

/// Returns true when any path component exactly matches an ignored directory
/// name. Components are split on `/`, matching Zig's `std.fs.path`
/// component iteration on POSIX.
pub fn path_contains_ignored_directory(path: &str) -> bool {
    path.split('/').any(is_ignored_directory_name)
}

/// Returns true when `name` is exactly one of the ignored directory names.
pub fn is_ignored_directory_name(name: &str) -> bool {
    ignored_directory_names.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_contains_ignored_directory_detects_ignored_path_components() {
        assert!(path_contains_ignored_directory("node_modules/pkg/index.zig"));
        assert!(path_contains_ignored_directory("src/.git/config"));
        assert!(path_contains_ignored_directory(".zig-cache/o/file.o"));
        assert!(path_contains_ignored_directory("zig-out/bin/fx"));
        assert!(path_contains_ignored_directory(".next/server/app.js"));
        assert!(path_contains_ignored_directory("dist/app.js"));
        assert!(path_contains_ignored_directory("build/out.o"));
        assert!(path_contains_ignored_directory("coverage/lcov.info"));
    }

    #[test]
    fn path_contains_ignored_directory_ignores_non_matching_path_components() {
        assert!(!path_contains_ignored_directory("src/module/index.zig"));
        assert!(!path_contains_ignored_directory("src/node_modules_backup/index.zig"));
        assert!(!path_contains_ignored_directory("src/builders/output.zig"));
    }

    #[test]
    fn is_ignored_directory_name_detects_exact_ignored_basenames() {
        assert!(is_ignored_directory_name(".git"));
        assert!(is_ignored_directory_name("node_modules"));
        assert!(!is_ignored_directory_name("src/.git"));
        assert!(!is_ignored_directory_name("node_modules_backup"));
    }
}
