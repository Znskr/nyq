//! Agent Skills: directories holding a `SKILL.md` (YAML frontmatter with
//! `name` and `description`, then instructions). Discovered from the
//! usual skill roots, listed in the system prompt, and loaded on demand by the
//! `skill` tool (SKILL.md or one text resource of the skill, in parts of
//! `skill_chunk_bytes`). Skill text is untrusted: invisible and control
//! characters that could hide instructions are removed before the model sees it.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::tools::{slash_path, Tool, ToolCtx, ToolOutput};
use crate::types::ToolSpec;

const MAX_PROMPT_SKILLS: usize = 100;
const MAX_LISTED_FILES: usize = 50;

#[derive(Clone, Debug, PartialEq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// Path of the SKILL.md file.
    pub path: PathBuf,
    /// The skill's directory (its base for relative references).
    pub dir: PathBuf,
    /// Canonical files must stay under this directory.
    root: PathBuf,
    /// "project" or "user".
    pub scope: &'static str,
    /// The skill root it was found in, e.g. `.claude/skills` or `~/.config/opencode/skills`.
    pub location: String,
    /// How the skill is named to the model and in `/skills`: its name, or
    /// `name@location` when a skill of the same name was found earlier.
    pub id: String,
}

/// One skill root: `(scan directory, containment root, scope, location label)`.
type Root = (PathBuf, PathBuf, &'static str, String);

/// Project skill roots, relative to the workspace, in precedence order.
pub(crate) const PROJECT_ROOTS: &[&str] =
    &[".agents/skills", ".codex/skills", ".claude/skills", ".nyq/skills", ".opencode/skills", "skills", ".claw/skills"];
/// Other agents' user skill roots, relative to the user's home, after `~/.nyq/skills`.
pub(crate) const USER_ROOTS: &[&str] =
    &[".agents/skills", ".codex/skills", ".claude/skills", ".config/opencode/skills", ".claw/skills"];

/// Where skills come from, as `/skills` groups them.
pub const SOURCES: &[&str] = &["nyq", "Agents", "Claude Code", "Codex", "OpenCode", "Claw", "Workspace"];

/// The [`SOURCES`] group of the root a skill was found in.
pub fn source(skill: &Skill) -> &'static str {
    match skill.location.trim_start_matches("~/") {
        ".agents/skills" => "Agents",
        ".claude/skills" => "Claude Code",
        ".codex/skills" => "Codex",
        ".opencode/skills" | ".config/opencode/skills" => "OpenCode",
        ".claw/skills" => "Claw",
        "skills" if skill.scope == "project" => "Workspace",
        // `.nyq/skills` and the profile's `skills/`.
        _ => "nyq",
    }
}

/// Skill roots in precedence order (for same-named skills, the first keeps
/// the plain name).
fn roots(home: &Path, user_home: Option<&Path>, cwd: &Path) -> Vec<Root> {
    // Started in the home directory, the project roots are the user roots.
    let same = |a: &Path, b: &Path| match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    };
    let at_home = same(&cwd.join(".nyq"), home) || user_home.is_some_and(|h| same(cwd, h));
    let mut roots: Vec<Root> = if at_home { Vec::new() } else { PROJECT_ROOTS.to_vec() }
        .iter()
        .map(|d| (cwd.join(d), cwd.to_path_buf(), "project", d.to_string()))
        .collect();
    let user_skills = home.join("skills");
    roots.push((user_skills.clone(), user_skills.clone(), "user", slash_path(&user_skills)));
    if let Some(user_home) = user_home {
        for d in USER_ROOTS {
            let dir = user_home.join(d);
            roots.push((dir.clone(), dir, "user", format!("~/{d}")));
        }
    }
    roots
}

/// Discover the skills a session offers: every skill but those named in
/// the `disabled_skills` setting. `home` is the nyq profile dir (~/.nyq).
pub fn discover(home: &Path, cwd: &Path) -> Vec<Skill> {
    let mut skills = discover_report(home, cwd).0;
    skills.retain(|s| !is_disabled(s));
    skills
}

/// Whether the `disabled_skills` setting turns `skill` off (by its id).
pub fn is_disabled(skill: &Skill) -> bool {
    crate::settings::current().disabled_skills.contains(&skill.id)
}

/// Every skill found, disabled ones included, plus problems with SKILL.md
/// files (for `/skills`).
pub fn discover_report(home: &Path, cwd: &Path) -> (Vec<Skill>, Vec<String>) {
    discover_in(&roots(home, dirs::home_dir().as_deref(), cwd))
}

/// [`discover`] with an explicit user home (None skips the user's other tools' skills).
#[cfg(test)]
pub(crate) fn discover_with(home: &Path, user_home: Option<&Path>, cwd: &Path) -> Vec<Skill> {
    discover_in(&roots(home, user_home, cwd)).0
}

fn discover_in(roots: &[Root]) -> (Vec<Skill>, Vec<String>) {
    let mut skills: Vec<Skill> = Vec::new();
    let mut problems = Vec::new();
    for (scan, root, scope, location) in roots {
        let Ok(entries) = std::fs::read_dir(scan) else { continue };
        let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        dirs.sort();
        for dir in dirs {
            let path = dir.join("SKILL.md");
            if std::fs::symlink_metadata(&path).is_err() {
                continue;
            }
            let text = match crate::tools::read_rooted_text(root, &path) {
                Ok(text) => text,
                Err(e) => {
                    problems.push(format!("{}: skipped: {e}", slash_path(&path)));
                    continue;
                }
            };
            if let Err(e) = front_matter(&text) {
                problems.push(format!("{}: {e}", slash_path(&path)));
            }
            let (meta, _) = split_frontmatter(&text);
            if meta.description.as_deref().is_none_or(|d| d.trim().is_empty()) {
                problems.push(format!("{}: no `description` in the frontmatter", slash_path(&path)));
            }
            let dir_name = dir.file_name().unwrap_or_default().to_string_lossy().into_owned();
            let name = meta.name.filter(|n| !n.is_empty()).unwrap_or(dir_name);
            // Same-named skills from different roots are all kept; later ones
            // are told apart by where they live.
            let id = if skills.iter().any(|s| s.name == name) { format!("{name}@{location}") } else { name.clone() };
            // The same SKILL.md reached twice (a symlinked directory) is one skill.
            let real = std::fs::canonicalize(&path).ok();
            if skills.iter().any(|s| s.id == id || (real.is_some() && std::fs::canonicalize(&s.path).ok() == real)) {
                continue;
            }
            let description = sanitize(&meta.description.unwrap_or_default()).0;
            skills.push(Skill {
                name,
                description,
                path,
                dir,
                root: root.clone(),
                scope,
                location: location.clone(),
                id,
            });
        }
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    (skills, problems)
}

#[derive(Default, Debug, PartialEq)]
struct Meta {
    name: Option<String>,
    description: Option<String>,
}

/// SKILL.md's `name` (if any) and body; an error when frontmatter is opened
/// but never closed.
pub(crate) fn front_matter(text: &str) -> Result<(Option<String>, &str), String> {
    let mut lines = text.strip_prefix('\u{feff}').unwrap_or(text).lines();
    if lines.next().map(str::trim_end) == Some("---") && !lines.any(|l| l.trim_end() == "---") {
        return Err("frontmatter is missing its closing `---`".into());
    }
    let (meta, body) = split_frontmatter(text);
    Ok((meta.name, body))
}

/// Split SKILL.md into its frontmatter fields and the body after it.
fn split_frontmatter(text: &str) -> (Meta, &str) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.split_inclusive('\n');
    if lines.next().map(str::trim_end) != Some("---") {
        return (Meta::default(), text);
    }
    let mut offset = text.find('\n').map_or(text.len(), |i| i + 1);
    let mut header = Vec::new();
    let mut closed = false;
    for line in lines {
        offset += line.len();
        if line.trim_end() == "---" {
            closed = true;
            break;
        }
        header.push(line.trim_end_matches(['\n', '\r']));
    }
    if !closed {
        return (Meta::default(), text);
    }
    (parse_header(&header), &text[offset..])
}

fn parse_header(lines: &[&str]) -> Meta {
    let mut meta = Meta::default();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        if line.starts_with([' ', '\t']) || line.trim_start().starts_with('#') {
            continue;
        }
        let Some((key, raw)) = line.split_once(':') else { continue };
        // Indented (or blank) lines that follow belong to this value.
        let start = i;
        while i < lines.len() && (lines[i].trim().is_empty() || lines[i].starts_with([' ', '\t'])) {
            i += 1;
        }
        let value = parse_value(raw.trim(), &lines[start..i]);
        match key.trim() {
            "name" => meta.name = Some(value),
            "description" => meta.description = Some(value),
            _ => {}
        }
    }
    meta
}

/// A scalar value: quoted, block (`>`/`|` with chomping indicators) or plain
/// (possibly continued on indented lines, which fold into one line).
fn parse_value(raw: &str, continuation: &[&str]) -> String {
    let cont: Vec<&str> = continuation.iter().map(|l| l.trim()).collect();
    if let Some(style) = raw.chars().next().filter(|c| matches!(c, '>' | '|')) {
        if raw[1..].trim_start_matches(|c: char| c == '-' || c == '+' || c.is_ascii_digit()).trim().is_empty() {
            return if style == '|' { cont.join("\n").trim().to_string() } else { fold(&cont) };
        }
    }
    let mut parts = vec![raw];
    parts.extend(cont.iter().copied());
    let joined = fold(&parts);
    if joined.len() >= 2 && joined.starts_with('"') && joined.ends_with('"') {
        unescape_double(&joined[1..joined.len() - 1])
    } else if joined.len() >= 2 && joined.starts_with('\'') && joined.ends_with('\'') {
        joined[1..joined.len() - 1].replace("''", "'")
    } else {
        joined
    }
}

/// YAML folding: lines join with spaces, blank lines become newlines.
fn fold(lines: &[&str]) -> String {
    let mut out = String::new();
    for line in lines {
        if line.is_empty() {
            out.push('\n');
        } else {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push(' ');
            }
            out.push_str(line);
        }
    }
    out.trim().to_string()
}

fn unescape_double(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// `text` without characters that render invisibly or reorder what is shown
/// (C0/C1 controls other than newline and tab, zero-width and bidi
/// formatting characters, tag characters), so a skill cannot hide
/// instructions from someone reviewing it. CRLF line ends become LF
/// silently. The flag tells whether anything else was removed.
pub(crate) fn sanitize(text: &str) -> (String, bool) {
    let hidden = |c: char| {
        matches!(c,
            '\u{0}'..='\u{8}' | '\u{b}'..='\u{1f}' | '\u{7f}'..='\u{9f}'
            | '\u{ad}' | '\u{61c}' | '\u{180e}'
            | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}' | '\u{feff}' | '\u{e0000}'..='\u{e007f}')
    };
    let text = text.replace("\r\n", "\n");
    if !text.chars().any(hidden) {
        return (text, false);
    }
    (text.chars().filter(|c| !hidden(*c)).collect(), true)
}

/// Appended to skill text that [`sanitize`] changed.
const SANITIZED: &str = "[context] Skill content was sanitized before delivery.";

/// One-line, length-capped description.
fn short(description: &str, max: usize) -> String {
    let flat = description.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.len() <= max {
        return flat;
    }
    let mut end = max.saturating_sub(3);
    while !flat.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", flat[..end].trim_end())
}

/// System-prompt section listing the available skills ("" when none).
pub fn prompt_section(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "# Skills\n\nSkills are packaged instructions for specific tasks. When a task matches a skill, \
         call the `skill` tool with its name before starting, then follow its instructions.\n\n",
    );
    let max_description = crate::limits::get("skill_description_bytes").unwrap_or(usize::MAX);
    for skill in skills.iter().take(MAX_PROMPT_SKILLS) {
        match short(&skill.description, max_description) {
            d if d.is_empty() => out.push_str(&format!("- {}\n", skill.id)),
            d => out.push_str(&format!("- {}: {d}\n", skill.id)),
        }
    }
    if skills.len() > MAX_PROMPT_SKILLS {
        out.push_str(&format!("- ... and {} more\n", skills.len() - MAX_PROMPT_SKILLS));
    }
    crate::limits::clip("skill_catalog_bytes", &out)
}

#[cfg(test)]
pub fn tool(skills: Vec<Skill>) -> Box<dyn Tool> {
    Box::new(SkillTool { skills, reload: None })
}

/// Add the `skill` tool (which also finds skills installed after it was
/// made, by looking again in the skill roots) and `install_skill`.
pub fn register(tools: &mut crate::tools::Registry, skills: Vec<Skill>, home: &Path, cwd: &Path) {
    tools.remove_names(&["skill".to_string(), "install_skill".to_string()]);
    tools.set_skills(skills.clone());
    tools.add(Box::new(SkillTool { skills, reload: Some((home.to_path_buf(), cwd.to_path_buf())) }));
    tools.add(Box::new(crate::skill_install::InstallSkill { home: home.to_path_buf() }));
}

struct SkillTool {
    skills: Vec<Skill>,
    reload: Option<(PathBuf, PathBuf)>,
}

fn find_in<'a>(skills: &'a [Skill], name: &str) -> Option<&'a Skill> {
    skills
        .iter()
        .find(|s| s.id == name)
        .or_else(|| skills.iter().find(|s| s.name == name))
        .or_else(|| skills.iter().find(|s| s.name.eq_ignore_ascii_case(name)))
}

impl SkillTool {
    fn find(&self, name: &str) -> Option<Skill> {
        if let Some(s) = find_in(&self.skills, name) {
            return Some(s.clone());
        }
        let (home, cwd) = self.reload.as_ref()?;
        find_in(&discover(home, cwd), name).cloned()
    }

    fn unknown(&self, name: &str) -> String {
        if self.skills.is_empty() {
            return format!("Unknown skill \"{name}\"; no skills are installed.");
        }
        let lower = name.to_lowercase();
        let close: Vec<&str> = self
            .skills
            .iter()
            .filter(|s| {
                let n = s.name.to_lowercase();
                n.contains(&lower) || lower.contains(&n) || edit_distance(&n, &lower) <= 3
            })
            .map(|s| s.id.as_str())
            .collect();
        if close.is_empty() {
            let all: Vec<&str> = self.skills.iter().take(30).map(|s| s.id.as_str()).collect();
            format!("Unknown skill \"{name}\". Available skills: {}", all.join(", "))
        } else {
            format!("Unknown skill \"{name}\". Did you mean: {}?", close.join(", "))
        }
    }
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            cur.push((prev[j] + usize::from(ca != *cb)).min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// Files in a skill directory (relative, sorted), excluding SKILL.md and
/// hidden entries. Returns at most `max` plus the total count.
pub(crate) fn skill_files(dir: &Path, max: usize) -> (Vec<String>, usize) {
    fn walk(root: &Path, dir: &Path, depth: usize, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            if path.is_dir() {
                if depth < 4 {
                    walk(root, &path, depth + 1, out);
                }
            } else if let Ok(rel) = path.strip_prefix(root) {
                if rel != Path::new("SKILL.md") {
                    out.push(slash_path(rel));
                }
            }
        }
    }
    let mut files = Vec::new();
    walk(dir, dir, 0, &mut files);
    files.sort();
    let total = files.len();
    files.truncate(max);
    (files, total)
}

/// A skill's instructions as the model sees them: name, base directory,
/// the SKILL.md body and the other files in the skill, sanitized (with a
/// notice when that removed anything).
pub fn load_text(skill: &Skill) -> Result<String, String> {
    let (mut out, sanitized) = document(skill, None)?;
    if sanitized {
        out.push_str(&format!("\n{SANITIZED}\n"));
    }
    Ok(out)
}

/// The sanitized text of a skill (SKILL.md with its header and file list)
/// or of one of its resources, and whether sanitizing removed anything.
/// Errors when it is over `skill_file_bytes`.
fn document(skill: &Skill, resource: Option<&str>) -> Result<(String, bool), String> {
    let (text, what) = match resource {
        Some(rel) => (read_resource(skill, rel)?, format!("resource `{rel}` of skill {}", skill.id)),
        None => (skill_md(skill)?, format!("skill {}", skill.id)),
    };
    if let Some(max) = crate::limits::get("skill_file_bytes").filter(|max| text.len() > *max) {
        return Err(format!(
            "The {what} is {} bytes, over the skill_file_bytes limit of {max} bytes; nothing was loaded \
             (the user can override with --context-limit skill_file_bytes=BYTES|off).",
            text.len()
        ));
    }
    Ok(sanitize(&text))
}

/// The whole SKILL.md (frontmatter included), sanitized, for viewing it.
pub fn raw_text(skill: &Skill) -> Result<String, String> {
    crate::tools::read_rooted_text(&skill.root, &skill.path)
        .map(|text| sanitize(&text).0)
        .map_err(|e| format!("reading {}: {e}", slash_path(&skill.path)))
}

fn skill_md(skill: &Skill) -> Result<String, String> {
    let text = match crate::tools::read_rooted_text(&skill.root, &skill.path) {
        Ok(t) => t,
        Err(e) => return Err(format!("reading {}: {e}", slash_path(&skill.path))),
    };
    let (_, body) = split_frontmatter(&text);
    let mut out = format!("Skill: {}\nBase directory: {}\n\n{}\n", skill.name, slash_path(&skill.dir), body.trim());
    let (files, total) = skill_files(&skill.dir, MAX_LISTED_FILES);
    if !files.is_empty() {
        out.push_str("\nOther files in this skill (relative to the base directory; read one with the `skill` tool's `resource`):\n");
        for f in &files {
            out.push_str(&format!("- {f}\n"));
        }
        if total > files.len() {
            out.push_str(&format!("- ... and {} more\n", total - files.len()));
        }
    }
    Ok(out)
}

/// A text file inside the skill's directory, by a relative path without
/// `..` or hidden components. Symlinks are followed only when they stay in
/// the skill directory (a link as the file itself is refused).
fn read_resource(skill: &Skill, rel: &str) -> Result<String, String> {
    use std::path::Component;
    let invalid = |why: &str| Err(format!("Invalid resource path `{rel}` for skill {}: {why}.", skill.id));
    let path = Path::new(rel);
    if path.is_absolute() || rel.starts_with(['/', '\\']) {
        return invalid("use a path relative to the skill directory");
    }
    for c in path.components() {
        match c {
            Component::Normal(n) if !n.to_string_lossy().starts_with('.') => {}
            Component::CurDir => {}
            Component::Normal(_) => return invalid("hidden files are not skill resources"),
            _ => return invalid("it must stay inside the skill directory"),
        }
    }
    // The skill directory itself must still be where discovery found it.
    let dir = std::fs::canonicalize(&skill.dir).map_err(|e| format!("skill {}: {e}", skill.id))?;
    let root = std::fs::canonicalize(&skill.root).map_err(|e| format!("skill {}: {e}", skill.id))?;
    if !dir.starts_with(&root) {
        return Err(format!("skill {}: its directory left its skill root", skill.id));
    }
    let full = dir.join(path);
    if std::fs::symlink_metadata(&full).is_err() {
        return Err(format!(
            "Skill {} has no resource `{rel}`; use a path from the files listed when the skill loads.",
            skill.id
        ));
    }
    let binary = || format!("Resource `{rel}` of skill {} is a binary file; only text resources can be loaded.", skill.id);
    match crate::tools::read_rooted_text(&dir, &full) {
        Ok(text) if text.contains('\0') => Err(binary()),
        Ok(text) => Ok(text),
        Err(e) if e == "not UTF-8 text" => Err(binary()),
        Err(e) => Err(format!("reading resource `{rel}` of skill {}: {e}", skill.id)),
    }
}

/// The part of `text` from `offset`: at most `max` bytes, ending on a line
/// break when one is in the second half, never inside a character (but at
/// least one character). Returns the part and where the next one starts.
fn chunk(text: &str, offset: usize, max: Option<usize>) -> (&str, usize) {
    let rest = &text[offset..];
    let Some(max) = max.filter(|max| rest.len() > *max) else {
        return (rest, text.len());
    };
    let mut end = max;
    while !rest.is_char_boundary(end) {
        end -= 1;
    }
    if end == 0 {
        end = rest.chars().next().map_or(0, char::len_utf8);
    }
    if let Some(nl) = rest[..end].rfind('\n').filter(|nl| *nl + 1 >= end / 2) {
        end = nl + 1;
    }
    (&rest[..end], offset + end)
}

#[async_trait::async_trait]
impl Tool for SkillTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "skill".into(),
            description: "Load a skill's instructions by name, or one text resource of the skill completely. \
                          Call this before starting a task that matches one of the available skills, then follow \
                          the instructions it returns. Paths mentioned in a skill's instructions are relative to \
                          that skill's directory, not the workspace: read the files it needs with the same name \
                          and `resource` set to the relative path. Long content comes in parts; continue with the \
                          `offset` it gives."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "The skill name"},
                    "resource": {"type": "string", "description": "Optional relative path of a text file in the skill's directory; omit (or empty) for SKILL.md"},
                    "offset": {"type": "integer", "minimum": 0, "description": "Byte offset to continue from (the next_offset of a truncated part)"}
                },
                "required": ["name"],
            }),
        }
    }

    fn summary(&self, args: &Value) -> String {
        let name = args["name"].as_str().unwrap_or_default();
        match args["resource"].as_str().map(str::trim).filter(|r| !r.is_empty()) {
            Some(resource) => format!("{name} {resource}"),
            None => name.to_string(),
        }
    }

    async fn run(&self, args: Value, _ctx: &ToolCtx) -> ToolOutput {
        let Some(name) = args["name"].as_str().map(str::trim) else {
            return ToolOutput::err("skill: \"name\" is required");
        };
        let resource = match &args["resource"] {
            Value::Null => None,
            Value::String(r) => Some(r.trim()).filter(|r| !r.is_empty() && *r != "SKILL.md"),
            _ => return ToolOutput::err("skill: \"resource\" must be a string"),
        };
        let offset = match &args["offset"] {
            Value::Null => 0,
            v => match v.as_u64() {
                Some(n) => n as usize,
                None => return ToolOutput::err("skill: \"offset\" must be a non-negative integer"),
            },
        };
        let Some(skill) = self.find(name) else {
            return ToolOutput::err(self.unknown(name));
        };
        let (text, sanitized) = match document(&skill, resource) {
            Ok(doc) => doc,
            Err(e) => return ToolOutput::err(e),
        };
        if offset > text.len() || !text.is_char_boundary(offset) {
            return ToolOutput::err(format!(
                "skill: offset {offset} is not a character boundary within the {} bytes of this content; use the next_offset it gave.",
                text.len()
            ));
        }
        let (part, next) = chunk(&text, offset, crate::limits::get("skill_chunk_bytes"));
        let mut out = part.to_string();
        if next < text.len() {
            let resource = resource.map(|r| format!(", resource \"{r}\"")).unwrap_or_default();
            out.push_str(&format!(
                "\n<context_limit name=\"skill_chunk_bytes\" action=\"truncated\" observed_bytes=\"{}\" next_offset=\"{next}\" \
                 override=\"--context-limit skill_chunk_bytes=BYTES|off\" />\n\
                 Call skill with name \"{}\"{resource} and offset {next} for the rest before relying on it.\n",
                text.len(),
                skill.id
            ));
        }
        if sanitized {
            out.push_str(&format!("\n{SANITIZED}\n"));
        }
        ToolOutput::ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_skill(root: &Path, dir: &str, content: &str) {
        let d = root.join(dir);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("SKILL.md"), content).unwrap();
    }

    fn md(name: &str, desc: &str) -> String {
        format!("---\nname: {name}\ndescription: {desc}\n---\n# {name}\nBody of {name}.\n")
    }

    #[test]
    fn frontmatter_variants() {
        let (m, body) = split_frontmatter("---\nname: \"pdf\"\ndescription: 'It''s great: \"yes\"'\n---\nhello\n");
        assert_eq!(m.name.as_deref(), Some("pdf"));
        assert_eq!(m.description.as_deref(), Some("It's great: \"yes\""));
        assert_eq!(body, "hello\n");

        let folded = "---\nname: x\ndescription: >-\n  first line\n  second line\n\n  para\nlicense: MIT\n---\nb";
        let (m, body) = split_frontmatter(folded);
        assert_eq!(m.description.as_deref(), Some("first line second line\npara"));
        assert_eq!(body, "b");

        let (m, _) = split_frontmatter("---\ndescription: |\n  a\n  b\n---\n");
        assert_eq!(m.description.as_deref(), Some("a\nb"));
        assert_eq!(m.name, None);

        let (m, _) = split_frontmatter("---\r\nname: plain\r\ndescription: one\r\n  two\r\n---\r\nbody");
        assert_eq!(m.name.as_deref(), Some("plain"));
        assert_eq!(m.description.as_deref(), Some("one two"));

        let (m, _) = split_frontmatter("---\ndescription: \"esc \\\"q\\\" \\\\ ok\"\n---\n");
        assert_eq!(m.description.as_deref(), Some("esc \"q\" \\ ok"));

        // No frontmatter / unterminated: whole text is the body.
        assert_eq!(split_frontmatter("# just text"), (Meta::default(), "# just text"));
        assert_eq!(split_frontmatter("---\nname: x\n").1, "---\nname: x\n");
    }

    #[test]
    fn discovery_precedence_and_fallbacks() {
        let t = tempfile::tempdir().unwrap();
        let (cwd, home, user) = (t.path().join("proj"), t.path().join("home"), t.path().join("user"));
        write_skill(&cwd.join(".claude/skills"), "review", &md("review", "project review"));
        write_skill(&cwd.join(".agents/skills"), "rev", &md("review", "agents review wins"));
        write_skill(&home.join("skills"), "review", &md("review", "user review"));
        write_skill(&home.join("skills"), "deploy", &md("deploy", "ship it"));
        write_skill(&user.join(".codex/skills"), "nodesc", "no frontmatter at all\n");
        write_skill(&user.join(".claude/skills"), "deploy", &md("deploy", "shadowed"));
        // Nested packs are not discovered (only one level is scanned).
        write_skill(&cwd.join(".nyq/skills"), "pack/inner", &md("inner", "nested"));
        fs::write(cwd.join(".nyq/skills/stray.md"), "not a dir").unwrap();

        // Workspace `skills/`, `.opencode/skills` and opencode's user root count too.
        write_skill(&cwd.join("skills"), "lint", &md("lint", "workspace lint"));
        write_skill(&cwd.join(".opencode/skills"), "fmt", &md("fmt", "opencode fmt"));
        write_skill(&user.join(".config/opencode/skills"), "doc", &md("doc", "user doc"));
        // Malformed SKILL.md files are reported.
        write_skill(&cwd.join(".claude/skills"), "broken", "---\nname: broken\n");

        let (skills, problems) = discover_in(&roots(&home, Some(&user), &cwd));
        let ids: Vec<&str> = skills.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "broken",
                "deploy",
                "deploy@~/.claude/skills",
                "doc",
                "fmt",
                "lint",
                "nodesc",
                "review",
                "review@.claude/skills",
                &format!("review@{}", slash_path(&home.join("skills"))),
            ]
        );
        let get = |id: &str| skills.iter().find(|s| s.id == id).unwrap();
        assert_eq!(get("deploy").description, "ship it");
        assert_eq!(get("deploy").scope, "user");
        assert_eq!(get("deploy@~/.claude/skills").description, "shadowed");
        assert_eq!(get("nodesc").description, "");
        assert_eq!(get("review").description, "agents review wins");
        assert_eq!(get("review").scope, "project");
        assert_eq!(get("review").location, ".agents/skills");
        assert_eq!(get("review").dir, cwd.join(".agents/skills/rev"));
        assert_eq!(get("review").path, cwd.join(".agents/skills/rev/SKILL.md"));
        assert_eq!(get("review@.claude/skills").description, "project review");
        assert_eq!(find_in(&skills, "review@.claude/skills").unwrap().description, "project review");
        assert_eq!(find_in(&skills, "Review").unwrap().description, "agents review wins");
        assert!(problems.iter().any(|p| p.contains("broken/SKILL.md") && p.contains("closing")), "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("nodesc/SKILL.md") && p.contains("description")), "{problems:?}");
        assert!(!problems.iter().any(|p| p.contains("deploy")), "{problems:?}");
    }

    #[cfg(unix)]
    #[test]
    fn skill_files_must_stay_inside_their_root() {
        let t = tempfile::tempdir().unwrap();
        write_skill(&t.path().join("store"), "linked", &md("linked", "via symlink"));
        let proj = t.path().join("proj");
        let root = proj.join(".claude/skills");
        fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(t.path().join("store/linked"), root.join("linked")).unwrap();
        let skills = discover_in(&roots(&t.path().join("home"), None, &proj)).0;
        assert!(skills.is_empty(), "a skill directory symlink must not leave the project");

        write_skill(&root, "real", &md("real", "inside"));
        std::os::unix::fs::symlink(root.join("real"), root.join("alias")).unwrap();
        let skills = discover_in(&roots(&t.path().join("home"), None, &proj)).0;
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].description, "inside");

        let evil = root.join("evil");
        fs::create_dir(&evil).unwrap();
        std::os::unix::fs::symlink(t.path().join("store/linked/SKILL.md"), evil.join("SKILL.md")).unwrap();
        let skills = discover_in(&roots(&t.path().join("home"), None, &proj)).0;
        assert!(skills.iter().all(|s| s.name != "linked"));
        if Path::new("/dev/zero").exists() {
            let huge = root.join("huge");
            fs::create_dir(&huge).unwrap();
            std::os::unix::fs::symlink("/dev/zero", huge.join("SKILL.md")).unwrap();
            let _ = discover_in(&roots(&t.path().join("home"), None, &proj)).0;
        }
    }

    fn skill(name: &str, description: &str) -> Skill {
        Skill {
            name: name.into(),
            description: description.into(),
            path: name.into(),
            dir: name.into(),
            root: name.into(),
            scope: "user",
            location: "skills".into(),
            id: name.into(),
        }
    }

    #[test]
    fn prompt_section_caps() {
        assert_eq!(prompt_section(&[]), "");
        let mut skills: Vec<Skill> = (0..105).map(|i| skill(&format!("s{i:03}"), "short")).collect();
        skills[0].description = "word ".repeat(100);
        skills[1].description = String::new();
        let p = prompt_section(&skills);
        assert!(p.contains("call the `skill` tool with its name before starting"));
        let first = p.lines().find(|l| l.starts_with("- s000")).unwrap();
        assert!(first.ends_with("...") && first.chars().count() <= "- s000: ".len() + 120);
        assert!(p.contains("- s001\n"));
        assert!(p.contains("- s099: short") && !p.contains("s100"));
        assert!(p.contains("and 5 more"));
    }

    #[tokio::test]
    async fn tool_loads_body_and_lists_files() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("proj/.agents/skills");
        write_skill(&root, "pdf", &md("pdf", "PDF tools"));
        fs::create_dir_all(root.join("pdf/scripts")).unwrap();
        fs::write(root.join("pdf/scripts/fill.py"), "").unwrap();
        fs::write(root.join("pdf/REFERENCE.md"), "").unwrap();
        fs::write(root.join("pdf/.hidden"), "").unwrap();
        let skills = discover_in(&roots(&t.path().join("home"), None, &t.path().join("proj"))).0;
        let tool = tool(skills);
        assert!(!tool.needs_approval(&json!({"name": "pdf"})));
        let out = tool.run(json!({"name": "pdf"}), &ToolCtx::default()).await;
        assert!(!out.is_error, "{}", out.content);
        let expected = format!(
            "Skill: pdf\nBase directory: {}\n\n# pdf\nBody of pdf.\n\nOther files in this skill (relative to the base directory; read one with the `skill` tool's `resource`):\n- REFERENCE.md\n- scripts/fill.py\n",
            slash_path(&root.join("pdf"))
        );
        assert_eq!(out.content, expected);

        let out = tool.run(json!({"name": "pfd"}), &ToolCtx::default()).await;
        assert!(out.is_error);
        assert_eq!(out.content, "Unknown skill \"pfd\". Did you mean: pdf?");
        let out = tool.run(json!({"name": "kubernetes-deploy"}), &ToolCtx::default()).await;
        assert_eq!(out.content, "Unknown skill \"kubernetes-deploy\". Available skills: pdf");
        assert!(tool.run(json!({}), &ToolCtx::default()).await.is_error);
    }

    #[test]
    fn home_directory_is_not_also_a_project() {
        // Started in the home directory: ~/.claude/skills and ~/.nyq/skills
        // are listed once, as user skills.
        let t = tempfile::tempdir().unwrap();
        let user = t.path();
        write_skill(&user.join(".claude/skills"), "review", &md("review", "claude review"));
        write_skill(&user.join(".nyq/skills"), "mine", &md("mine", "nyq skill"));
        let skills = discover_with(&user.join(".nyq"), Some(user), user);
        let ids: Vec<(&str, &str)> = skills.iter().map(|s| (s.id.as_str(), s.scope)).collect();
        assert_eq!(ids, [("mine", "user"), ("review", "user")]);
    }

    #[test]
    fn claw_roots_come_after_the_others() {
        let t = tempfile::tempdir().unwrap();
        let (cwd, user) = (t.path().join("proj"), t.path().join("user"));
        write_skill(&cwd.join(".claw/skills"), "claw", &md("claw", "project claw"));
        write_skill(&user.join(".claw/skills"), "uclaw", &md("uclaw", "user claw"));
        write_skill(&user.join(".claw/skills"), "dup", &md("dup", "shadowed"));
        write_skill(&cwd.join(".nyq/skills"), "dup", &md("dup", "nyq wins"));
        let skills = discover_with(&t.path().join("home"), Some(&user), &cwd);
        let ids: Vec<(&str, &str)> = skills.iter().map(|s| (s.id.as_str(), s.location.as_str())).collect();
        assert_eq!(
            ids,
            [("claw", ".claw/skills"), ("dup", ".nyq/skills"), ("dup@~/.claw/skills", "~/.claw/skills"), ("uclaw", "~/.claw/skills")]
        );
    }

    #[test]
    fn sanitize_strips_hidden_characters() {
        assert_eq!(sanitize("plain\ttext\r\nnext\n"), ("plain\ttext\nnext\n".to_string(), false));
        let (clean, changed) = sanitize("a\u{202e}b\u{2066}c\u{200b}d\u{7}e\u{feff}f\u{e0041}g\u{85}h");
        assert_eq!(clean, "abcdefgh");
        assert!(changed);
        assert_eq!(sanitize("emoji 🎉 and é").0, "emoji 🎉 and é");
    }

    #[test]
    fn chunks_end_on_lines_and_characters() {
        let text = "aaaa\nbbbb\ncccc";
        assert_eq!(chunk(text, 0, None), (text, text.len()));
        assert_eq!(chunk(text, 0, Some(100)), (text, text.len()));
        assert_eq!(chunk(text, 0, Some(8)), ("aaaa\n", 5));
        assert_eq!(chunk(text, 5, Some(8)), ("bbbb\n", 10));
        assert_eq!(chunk(text, 10, Some(8)), ("cccc", 14));
        let wide = "ééé";
        assert_eq!(chunk(wide, 0, Some(3)), ("é", 2));
        assert_eq!(chunk(wide, 0, Some(1)), ("é", 2), "always at least one character");
        assert_eq!(chunk(wide, 2, Some(4)), ("éé", 6));
    }

    #[tokio::test]
    async fn tool_reads_resources_in_parts() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("proj/.agents/skills");
        write_skill(&root, "pdf", &md("pdf", "PDF \u{202e}tools"));
        fs::create_dir_all(root.join("pdf/docs")).unwrap();
        fs::write(root.join("pdf/docs/forms.md"), "Fill forms.\u{200b}\n").unwrap();
        fs::write(root.join("pdf/logo.png"), [0x89, b'P', b'N', b'G', 0, 0xff]).unwrap();
        fs::write(root.join("pdf/nul.txt"), "a\0b").unwrap();
        fs::write(root.join("pdf/.env"), "SECRET=1").unwrap();
        fs::write(t.path().join("proj/outside.md"), "outside").unwrap();
        let line = format!("{}\n", "x".repeat(99));
        fs::write(root.join("pdf/big.md"), line.repeat(700)).unwrap();
        let skills = discover_in(&roots(&t.path().join("home"), None, &t.path().join("proj"))).0;
        assert_eq!(skills[0].description, "PDF tools", "descriptions are sanitized");
        let tool = tool(skills);
        let run = |args: Value| {
            let tool = &tool;
            async move { tool.run(args, &ToolCtx::default()).await }
        };

        let out = run(json!({"name": "pdf"})).await;
        assert!(out.content.contains("read one with the `skill` tool's `resource`"), "{}", out.content);
        assert!(!out.content.contains("sanitized"));
        let out = run(json!({"name": "pdf", "resource": "docs/forms.md"})).await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(out.content, "Fill forms.\n\n[context] Skill content was sanitized before delivery.\n");
        assert_eq!(run(json!({"name": "pdf", "resource": "./docs/forms.md"})).await.content, out.content);
        let same = run(json!({"name": "pdf", "resource": ""})).await;
        assert!(same.content.starts_with("Skill: pdf\n"));

        for (resource, expect) in [
            ("../../outside.md", "inside the skill directory"),
            ("docs/../../pdf/SKILL.md", "inside the skill directory"),
            ("/etc/passwd", "relative"),
            (".env", "hidden"),
            ("missing.md", "has no resource"),
            ("logo.png", "binary"),
            ("nul.txt", "binary"),
            ("docs", "not a regular file"),
        ] {
            let out = run(json!({"name": "pdf", "resource": resource})).await;
            assert!(out.is_error && out.content.contains(expect), "{resource}: {}", out.content);
        }
        assert!(run(json!({"name": "pdf", "resource": 5})).await.is_error);
        assert!(run(json!({"name": "pdf", "offset": -1})).await.is_error);

        // 70,000 bytes in parts of at most skill_chunk_bytes (32 KiB by default).
        let mut all = String::new();
        let mut offset = 0;
        for _ in 0..3 {
            let out = run(json!({"name": "pdf", "resource": "big.md", "offset": offset})).await;
            assert!(!out.is_error, "{}", out.content);
            match out.content.split_once("\n<context_limit ") {
                Some((part, marker)) => {
                    assert!(part.len() <= 32 * 1024);
                    assert!(marker.contains("name=\"skill_chunk_bytes\" action=\"truncated\" observed_bytes=\"70000\""), "{marker}");
                    assert!(marker.contains("override=\"--context-limit skill_chunk_bytes=BYTES|off\""));
                    all.push_str(part);
                    offset = marker.split("next_offset=\"").nth(1).unwrap().split('"').next().unwrap().parse().unwrap();
                    assert_eq!(offset, all.len());
                }
                None => all.push_str(&out.content),
            }
        }
        assert_eq!(all, line.repeat(700));
        assert!(run(json!({"name": "pdf", "resource": "big.md", "offset": 70_001})).await.is_error);
        assert_eq!(run(json!({"name": "pdf", "resource": "big.md", "offset": 70_000})).await.content, "");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resources_do_not_follow_links_out_of_the_skill() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("proj/.agents/skills");
        write_skill(&root, "pdf", &md("pdf", "PDF tools"));
        write_skill(&root, "other", &md("other", "x"));
        fs::write(root.join("other/notes.md"), "other notes").unwrap();
        std::os::unix::fs::symlink(root.join("other"), root.join("pdf/linked")).unwrap();
        std::os::unix::fs::symlink(root.join("other/notes.md"), root.join("pdf/notes.md")).unwrap();
        let tool = tool(discover_in(&roots(&t.path().join("home"), None, &t.path().join("proj"))).0);
        for resource in ["linked/notes.md", "notes.md"] {
            let out = tool.run(json!({"name": "pdf", "resource": resource}), &ToolCtx::default()).await;
            assert!(out.is_error, "{resource}: {}", out.content);
        }
    }
}
