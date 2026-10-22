//! The `load_skill` tool: a repo skill's full instructions, on demand.
//!
//! Agents see only [`crate::ai::context::RepoContext::skill_index`] in their
//! prompt; this tool hands back the SKILL.md body, frontmatter stripped, for
//! one named skill. The path comes from the context, never from the model's
//! input, but the checkout is attacker-controlled, so the file goes through
//! [`crate::ai::context::read_repo_file`] (no symlinks, size-capped) on a
//! blocking thread.

use serde::Deserialize;
use serde_json::{Value, json};

use super::{Tool, ToolCx, ToolOutput, definition, parse};

/// The `load_skill` tool.
pub(super) struct LoadSkill;

#[derive(Deserialize)]
struct SkillInput {
    name: String,
}

#[async_trait::async_trait]
impl Tool for LoadSkill {
    fn definition(&self) -> review_llm::Tool {
        definition(
            "load_skill",
            "Load one repo skill's full instructions by name; the skill index in \
             your prompt lists the names. Files inside the skill folder can be \
             read with read_file.",
            json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "The skill name, exactly as the index lists it."
                    }
                },
                "required": ["name"]
            }),
        )
    }

    async fn call(&self, input: Value, cx: &ToolCx) -> ToolOutput {
        let input: SkillInput = match parse(input) {
            Ok(input) => input,
            Err(out) => return out,
        };
        let Some(context) = cx.context.as_ref() else {
            return ToolOutput::error("no repo context attached");
        };
        let Some(skill) = context.skills.iter().find(|s| s.name == input.name) else {
            let names: Vec<&str> = context.skills.iter().map(|s| s.name.as_str()).collect();
            let available = if names.is_empty() {
                "(none)".to_owned()
            } else {
                names.join(", ")
            };
            return ToolOutput::error(format!(
                "unknown skill: {} (available: {available})",
                input.name
            ));
        };
        // The path comes from the context, not the model's input, so it is
        // repo-relative by construction.
        let path = cx.repo_root.join(&skill.dir).join("SKILL.md");
        let root = cx.repo_root.clone();
        let read = tokio::task::spawn_blocking(move || {
            crate::ai::context::read_repo_file(&root, &path, crate::ai::context::MAX_FILE_BYTES)
        })
        .await;
        let Ok(Ok(raw)) = read else {
            return ToolOutput::error(format!("not found: {}/SKILL.md", skill.dir));
        };
        let (_keys, body) = crate::ai::context::split_frontmatter(&raw);
        ToolOutput::text(format!(
            "{}\n[skill files are under {}/; read them with read_file]",
            body.trim(),
            skill.dir,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::context::{RepoContext, load_repo_context};
    use crate::ai::interaction::NoInteraction;
    use crate::ai::tools::{AgentRef, AgentRole, ToolCx, all};
    use crate::test_support::tempdir;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    /// A reviewer's context over `root` with `context` attached.
    fn skill_ctx(root: &Path, context: Option<Arc<RepoContext>>) -> ToolCx {
        ToolCx {
            sandbox: Arc::new(review_sandbox::LocalHost::new(root).unwrap()),
            repo_root: root.to_path_buf(),
            agent: AgentRef {
                id: 1,
                role: AgentRole::Reviewer,
                label: "reviewer".to_owned(),
            },
            interaction: Arc::new(NoInteraction),
            allowed: Arc::new(Mutex::new(Vec::new())),
            for_review: Arc::new(Mutex::new(Vec::new())),
            snapshot: None,
            context,
            run: crate::test_support::ai_run_state(),
            assigned: Arc::new(Vec::new()),
            spawner: None,
            prepare: None,
        }
    }

    async fn call(cx: &ToolCx, input: Value) -> ToolOutput {
        all()
            .into_iter()
            .find(|t| t.definition().name == "load_skill")
            .expect("load_skill is registered")
            .call(input, cx)
            .await
    }

    fn text(out: &ToolOutput) -> &str {
        assert_eq!(out.parts.len(), 1, "{:?}", out.parts);
        match &out.parts[0] {
            review_llm::ToolResultPart::Text { text } => text,
            other => panic!("not text: {other:?}"),
        }
    }

    #[tokio::test]
    async fn load_skill_returns_body_and_unknown_is_error() {
        let dir = tempdir();
        std::fs::create_dir_all(dir.path().join(".claude/skills/pdf")).unwrap();
        std::fs::write(
            dir.path().join(".claude/skills/pdf/SKILL.md"),
            "---\nname: pdf\ndescription: PDF things.\n---\nRead the whole file.\n",
        )
        .unwrap();
        let context = Arc::new(load_repo_context(dir.path()));
        let cx = skill_ctx(dir.path(), Some(context));

        let out = call(&cx, json!({"name": "pdf"})).await;
        assert!(!out.is_error, "{:?}", out.parts);
        let t = text(&out);
        assert!(t.contains("Read the whole file."), "{t}");
        // The frontmatter is stripped, and the trailer points at the folder.
        assert!(!t.contains("PDF things."), "{t}");
        assert!(!t.contains("---"), "{t}");
        assert!(
            t.ends_with("\n[skill files are under .claude/skills/pdf/; read them with read_file]"),
            "{t}"
        );

        // An unknown name is an error that lists what exists.
        let out = call(&cx, json!({"name": "nope"})).await;
        assert!(out.is_error, "{:?}", out.parts);
        let t = text(&out);
        assert!(t.contains("nope"), "{t}");
        assert!(t.contains("pdf"), "{t}");

        // No context attached at all.
        let cx = skill_ctx(dir.path(), None);
        let out = call(&cx, json!({"name": "pdf"})).await;
        assert!(out.is_error, "{:?}", out.parts);
        assert_eq!(text(&out), "no repo context attached");
    }

    #[tokio::test]
    async fn symlinked_skill_md_is_not_read() {
        let dir = tempdir();
        let secret = dir.path().join("secret.txt");
        std::fs::write(&secret, "TOP-SECRET").unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join(".claude/skills/pdf")).unwrap();
        let skill = repo.join(".claude/skills/pdf/SKILL.md");
        std::fs::write(&skill, "---\nname: pdf\n---\nok\n").unwrap();
        let context = Arc::new(load_repo_context(&repo));
        // Swapped for a symlink after the index was built.
        std::fs::remove_file(&skill).unwrap();
        std::os::unix::fs::symlink(&secret, &skill).unwrap();
        let cx = skill_ctx(&repo, Some(context));
        let out = call(&cx, json!({"name": "pdf"})).await;
        assert!(out.is_error, "{:?}", out.parts);
        assert!(!text(&out).contains("TOP-SECRET"));
    }
}
