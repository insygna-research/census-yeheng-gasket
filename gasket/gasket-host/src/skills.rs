//! Skills: a prompt-appended catalog of on-disk instruction files.
//!
//! Global skills live at `<config_dir>/skills/*.md`; project skills at
//! `<cwd>/.gasket/skills/*.md` (same `name:` wins over the global one).
//! Only the catalog line + absolute path is appended — the model reads a
//! skill's full content with the `read` tool on demand.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

struct SkillMeta {
    description: String,
    source: PathBuf,
}

/// Production entry: global root is gasket's config dir (`~/.gasket`).
pub fn append_skills(base: &str, cwd: &Path) -> String {
    append_skills_in(base, cwd, &gasket_core::storage::config_dir())
}

/// Testable core: `global_root` is injected (production uses the config dir).
pub fn append_skills_in(base: &str, cwd: &Path, global_root: &Path) -> String {
    let mut catalog: BTreeMap<String, SkillMeta> = BTreeMap::new();
    scan_dir(&global_root.join("skills"), &mut catalog);
    scan_dir(&cwd.join(".gasket").join("skills"), &mut catalog);
    if catalog.is_empty() {
        return base.to_string();
    }
    let mut out = String::from(base);
    out.push_str(
        "\n\n## Skills\n\nThe entries below are skills. To follow one, load its \
         full instructions with the `read` tool using the path shown for it.\n\n",
    );
    for (name, meta) in &catalog {
        out.push_str(&format!(
            "- name: {name} — {} (source: {})\n",
            meta.description,
            meta.source.display()
        ));
    }
    out
}

fn scan_dir(dir: &Path, catalog: &mut BTreeMap<String, SkillMeta>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return, // no dir = no skills; not an error
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        match std::fs::read_to_string(&path)
            .ok()
            .and_then(|c| parse_frontmatter(&c))
        {
            Some((name, description)) => {
                catalog.insert(
                    name,
                    SkillMeta {
                        description,
                        source: path,
                    },
                );
            }
            None => {
                tracing::warn!(path = %path.display(), "skill missing frontmatter name/description; skipped")
            }
        }
    }
}

/// Hand-parsed frontmatter: `---\n` … `---` block carrying both `name:` and
/// `description:` lines. Anything else is None (caller warns + skips).
fn parse_frontmatter(content: &str) -> Option<(String, String)> {
    let mut name = None;
    let mut description = None;
    for line in content.strip_prefix("---\n")?.lines() {
        if line.trim_end() == "---" {
            return match (name, description) {
                (Some(n), Some(d)) => Some((n, d)),
                _ => None,
            };
        }
        if let Some(v) = line.strip_prefix("name:") {
            name = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("description:") {
            description = Some(v.trim().to_string());
        }
    }
    None // unterminated frontmatter block
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn skill_file(dir: &Path, file: &str, body: &str) {
        fs::create_dir_all(dir.join("skills")).unwrap();
        fs::write(dir.join("skills").join(file), body).unwrap();
    }

    const OK: &str =
        "---\nname: code-review\ndescription: Review diffs for bugs\n---\nBody never injected.\n";

    #[test]
    fn appends_catalog_for_valid_skill() {
        let tmp = tempfile::tempdir().unwrap();
        skill_file(tmp.path(), "review.md", OK);
        let out = append_skills_in("BASE", Path::new("/nope"), tmp.path());
        assert!(out.starts_with("BASE"));
        assert!(out.contains("## Skills"));
        assert!(out.contains("- name: code-review — Review diffs for bugs"));
        assert!(out.contains(&format!(
            "(source: {})",
            tmp.path().join("skills/review.md").display()
        )));
        assert!(
            out.contains("`read`"),
            "must tell the model to use the read tool"
        );
        assert!(!out.contains("Body never injected"));
    }

    #[test]
    fn missing_description_and_no_frontmatter_are_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        skill_file(tmp.path(), "bad.md", "---\nname: x\n---\nbody\n");
        assert_eq!(
            append_skills_in("BASE", Path::new("/nope"), tmp.path()),
            "BASE"
        );
        skill_file(tmp.path(), "plain.md", "Just some markdown.\n");
        assert_eq!(
            append_skills_in("BASE", Path::new("/nope"), tmp.path()),
            "BASE"
        );
    }

    #[test]
    fn no_skills_returns_base_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            append_skills_in("BASE", Path::new("/nope"), tmp.path()),
            "BASE"
        );
    }

    #[test]
    fn project_overrides_global_on_name_clash() {
        let g = tempfile::tempdir().unwrap();
        let p = tempfile::tempdir().unwrap();
        skill_file(
            g.path(),
            "a.md",
            "---\nname: dup\ndescription: global one\n---\n",
        );
        skill_file(
            &p.path().join(".gasket"),
            "b.md",
            "---\nname: dup\ndescription: project one\n---\n",
        );
        let out = append_skills_in("BASE", p.path(), g.path());
        assert!(out.contains("project one") && !out.contains("global one"));
        assert_eq!(
            out.matches("- name: dup").count(),
            1,
            "exactly one line per name"
        );
    }

    #[test]
    fn project_only_skill_is_included() {
        let g = tempfile::tempdir().unwrap();
        let p = tempfile::tempdir().unwrap();
        skill_file(
            &p.path().join(".gasket"),
            "only.md",
            "---\nname: proj\ndescription: from project\n---\n",
        );
        assert!(
            append_skills_in("BASE", p.path(), g.path()).contains("- name: proj — from project")
        );
    }
}
