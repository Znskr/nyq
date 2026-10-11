//! Skills the user asks for by name in a prompt: a leading `/name`, a
//! `$name` word (from the `$` picker), or "use the NAME skill". Their
//! instructions go with that turn, so the model does not have to load them.
//! Skills are matched by id (`name`, or `name@location` for a duplicate).

use crate::skills::{self, Skill};

/// `(name, description)` rows for the `/` and `$` pickers.
pub fn entries(skills: &[Skill]) -> Vec<(String, String)> {
    skills.iter().map(|s| (s.id.clone(), s.description.clone())).collect()
}

/// The skill a leading `/name` names, unless `is_command` claims the word.
pub fn leading<'a>(skills: &'a [Skill], text: &str, is_command: impl Fn(&str) -> bool) -> Option<&'a Skill> {
    let word = text.split_whitespace().next()?;
    let name = word.strip_prefix('/')?;
    if is_command(word) {
        return None;
    }
    skills.iter().find(|s| s.id.eq_ignore_ascii_case(name))
}

/// Every skill `text` names, in the order of `skills`.
pub fn named<'a>(skills: &'a [Skill], text: &str) -> Vec<&'a Skill> {
    let lower = text.to_lowercase();
    let words: Vec<&str> = text
        .split_whitespace()
        .map(|w| w.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', '"', '\'']))
        .collect();
    let first = words.first().copied().unwrap_or("");
    skills
        .iter()
        .filter(|s| {
            let name = s.id.to_lowercase();
            first.eq_ignore_ascii_case(&format!("/{name}"))
                || words.iter().any(|w| w.eq_ignore_ascii_case(&format!("${name}")))
                || [
                    format!("use the {name} skill"),
                    format!("use {name} skill"),
                    format!("using the {name} skill"),
                    format!("use the skill {name}"),
                    format!("use skill {name}"),
                ]
                .iter()
                .any(|p| lower.contains(p.as_str()))
        })
        .collect()
}

/// The turn text with each skill's instructions ahead of the request.
/// Skills that cannot be read are reported and skipped.
pub fn turn_text(chosen: &[&Skill], text: &str) -> (String, Vec<String>) {
    let mut out = String::new();
    let mut errors = Vec::new();
    for skill in chosen {
        match skills::load_text(skill) {
            Ok(body) => out.push_str(&format!(
                "<skill name=\"{}\">\nThe user explicitly invoked this skill for this request; follow its instructions.\n\n{}</skill>\n\n",
                skill.id, body
            )),
            Err(e) => errors.push(format!("skill {}: {e}", skill.id)),
        }
    }
    out.push_str(text);
    (out, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skills(dir: &std::path::Path) -> Vec<Skill> {
        for name in ["pdf", "review-pr"] {
            let d = dir.join(".agents/skills").join(name);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("SKILL.md"), format!("---\nname: {name}\ndescription: d\n---\nDo {name} things.\n")).unwrap();
        }
        skills::discover_with(&dir.join("home"), None, dir)
    }

    #[test]
    fn finds_skills_by_slash_dollar_and_words() {
        let dir = tempfile::tempdir().unwrap();
        let all = skills(dir.path());
        let names = |text: &str| named(&all, text).iter().map(|s| s.name.clone()).collect::<Vec<_>>();
        assert_eq!(names("/pdf split this file"), ["pdf"]);
        assert_eq!(names("please $review-pr, and $pdf."), ["pdf", "review-pr"]);
        assert_eq!(names("Use the review-pr skill on #12"), ["review-pr"]);
        assert!(names("a pdf about prices").is_empty());
        assert!(names("cost is $5").is_empty());
        assert_eq!(leading(&all, "/pdf x", |_| false).map(|s| s.name.as_str()), Some("pdf"));
        assert!(leading(&all, "/pdf x", |c| c == "/pdf").is_none());
        assert!(leading(&all, "pdf", |_| false).is_none());

        let (text, errors) = turn_text(&named(&all, "$pdf go"), "$pdf go");
        assert!(errors.is_empty());
        assert!(text.starts_with("<skill name=\"pdf\">"), "{text}");
        assert!(text.contains("Do pdf things."));
        assert!(text.ends_with("</skill>\n\n$pdf go"));
    }
}
