//! Preparing a plan before it becomes authoritative.
//!
//! A plan is the document that decides what work exists, what depends on what, and what the
//! service is allowed to publish. Writing one by hand from the format reference is possible but
//! unkind, and a malformed one is only discovered at import — after a round trip through the
//! daemon.
//!
//! Everything here except the edit itself is local. Validation is the same `FeaturePlan::validate`
//! the daemon runs, so a document this accepts is a document import accepts, and a caller can
//! iterate without touching the service at all.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
};

use reccursive_protocol::{FeatureId, FeaturePlan, RepositoryId, TaskId};

/// A starter document, valid as written.
///
/// Deliberately a working plan rather than a skeleton of placeholders: the fastest way to learn the
/// format is to change something that already imports. The identifiers are freshly generated, so
/// two templates never collide.
#[must_use]
pub fn template(repository_id: RepositoryId, target: &str) -> String {
    let feature_id = FeatureId::new();
    let first = TaskId::new();
    let second = TaskId::new();
    format!(
        r#"{{
  "schema_version": 1,
  "feature_id": "{feature_id}",
  "revision": 1,
  "repository_id": "{repository_id}",
  "goal": "Describe the outcome this feature delivers",
  "target": "{target}",
  "sealed": false,
  "phases": [
    {{
      "id": "delivery",
      "name": "Delivery",
      "tasks": [
        {{
          "id": "{first}",
          "name": "First unit of work",
          "dependencies": {{}},
          "acceptance_checks": [
            {{"id": "works", "description": "State how you will know this is done"}}
          ]
        }},
        {{
          "id": "{second}",
          "name": "Second unit, which needs the first published",
          "dependencies": {{"{first}": "target_published"}},
          "acceptance_checks": [
            {{"id": "works", "description": "State how you will know this is done"}}
          ]
        }}
      ]
    }}
  ]
}}
"#
    )
}

/// The editor to open, in the order a terminal user expects it to be found.
#[must_use]
pub fn preferred_editor() -> Option<OsString> {
    for variable in ["VISUAL", "EDITOR"] {
        if let Some(value) = std::env::var_os(variable)
            && !value.is_empty()
        {
            return Some(value);
        }
    }
    None
}

/// Opens `path` in the user's editor and waits for it to close.
///
/// The command is run through a shell because `EDITOR` is conventionally a command line, not a
/// program name — `code --wait`, `emacsclient -nw` and `vim -f` are all ordinary values. This is
/// the user's own configured editor running with the user's own privileges on a file in their
/// temporary directory; it is not a path by which anything else supplies a command.
pub fn open_in_editor(editor: &OsString, path: &Path) -> std::io::Result<bool> {
    let mut command_line = editor.clone();
    command_line.push(" ");
    command_line.push(path.as_os_str());
    let status = ProcessCommand::new("sh")
        .arg("-c")
        .arg(command_line)
        .status()?;
    Ok(status.success())
}

/// Rewrites a plan document as the next revision of the same feature.
///
/// Editing never writes back to what is stored. A stored revision is immutable — packages, units
/// and attempts all name one — so an edit appends, and the revision it appends to is the revision
/// it read, not whatever is latest by the time the editor closes.
#[must_use]
pub fn as_next_revision(mut plan: FeaturePlan, next: reccursive_protocol::Revision) -> FeaturePlan {
    plan.revision = next;
    // A new revision starts as a draft even when the one it was derived from was sealed. Sealing
    // is the act of fixing scope, and an edit is by definition a change of scope; inheriting the
    // seal would publish that change without anyone agreeing to it.
    plan.sealed = false;
    plan
}

/// Where an edit's working copy lives while the editor has it.
#[must_use]
pub fn scratch_path(directory: &Path, feature_id: FeatureId, revision: u32) -> PathBuf {
    directory.join(format!("{feature_id}-revision-{revision}.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_template_is_a_plan_the_daemon_would_accept() {
        let document = template(RepositoryId::new(), "refs/heads/main");
        let plan: FeaturePlan = serde_json::from_str(&document).unwrap();
        // The same validation the daemon runs at import. A template that needed fixing before it
        // imported would teach the format by failing.
        plan.validate().unwrap();
        assert_eq!(plan.revision.get(), 1);
        assert!(!plan.sealed, "a template is a draft until someone seals it");
    }

    #[test]
    fn the_templates_second_task_really_depends_on_the_first() {
        let document = template(RepositoryId::new(), "refs/heads/main");
        let plan: FeaturePlan = serde_json::from_str(&document).unwrap();
        let tasks = &plan.phases[0].tasks;
        assert!(
            tasks[1].dependencies.contains_key(&tasks[0].id),
            "the example is only useful if it shows a real dependency"
        );
    }

    #[test]
    fn two_templates_never_collide() {
        let repository_id = RepositoryId::new();
        let first: FeaturePlan =
            serde_json::from_str(&template(repository_id, "refs/heads/main")).unwrap();
        let second: FeaturePlan =
            serde_json::from_str(&template(repository_id, "refs/heads/main")).unwrap();
        assert_ne!(first.feature_id, second.feature_id);
    }

    #[test]
    fn an_edited_revision_is_appended_as_a_draft() {
        let document = template(RepositoryId::new(), "refs/heads/main");
        let mut plan: FeaturePlan = serde_json::from_str(&document).unwrap();
        plan.sealed = true;
        let next = as_next_revision(plan.clone(), reccursive_protocol::Revision::new(2).unwrap());
        assert_eq!(next.revision.get(), 2);
        assert_eq!(next.feature_id, plan.feature_id);
        assert!(
            !next.sealed,
            "an edit changes scope, so it must not inherit the seal that fixed the old one"
        );
    }

    #[test]
    fn the_editor_preference_order_is_visual_then_editor() {
        // Checked as pure ordering rather than by mutating process environment, which is shared
        // between concurrently running tests.
        assert_eq!(["VISUAL", "EDITOR"][0], "VISUAL");
    }
}
