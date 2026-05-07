use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::{DaemonTestScope, GitTestMode, TestRepo};
use crate::test_utils::fixture_path;
use git_ai::commands::checkpoint_agent::bash_tool::git_status_fallback;
use serde_json::json;
use serial_test::serial;
use std::fs;
use std::path::{Path, PathBuf};

fn new_superrepo() -> TestRepo {
    TestRepo::new_dedicated_daemon()
}

fn new_fixture_repo() -> TestRepo {
    TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon)
}

fn submodule_repo(path: &Path) -> TestRepo {
    TestRepo::new_at_path_with_mode_and_daemon_scope(
        path,
        GitTestMode::from_env(),
        DaemonTestScope::Dedicated,
    )
}

fn seed_superrepo(superrepo: &TestRepo) {
    fs::write(superrepo.path().join("README.md"), "# Superrepo\n").unwrap();
    commit_fixture_state(superrepo, "initial superrepo");
}

fn commit_fixture_state(repo: &TestRepo, message: &str) {
    repo.git_og(&["add", "."])
        .expect("fixture add should succeed");
    repo.git_og(&["commit", "-m", message])
        .expect("fixture commit should succeed");
}

fn add_submodule(superrepo: &TestRepo, relative_path: &str) -> PathBuf {
    let source = new_fixture_repo();
    fs::write(source.path().join("README.md"), "# Submodule source\n").unwrap();
    commit_fixture_state(&source, "initial submodule source");

    let submodule_path = superrepo.path().join(relative_path);
    if let Some(parent) = submodule_path.parent() {
        fs::create_dir_all(parent).unwrap();
    }

    superrepo
        .git_og(&[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            source.path().to_str().unwrap(),
            relative_path,
        ])
        .expect("submodule add should succeed");
    commit_fixture_state(superrepo, &format!("add submodule {}", relative_path));

    submodule_path
}

fn checkpoint_mock_ai_from_superrepo(superrepo: &TestRepo, files: &[PathBuf]) {
    let mut args = vec!["checkpoint", "mock_ai"];
    let path_strings: Vec<String> = files
        .iter()
        .map(|path| path.to_string_lossy().to_string())
        .collect();
    args.extend(path_strings.iter().map(String::as_str));

    superrepo
        .git_ai_from_working_dir(&superrepo.canonical_path(), &args)
        .expect("checkpoint from superrepo CWD should succeed");
}

fn claude_bash_hook_input(superrepo_root: &Path, event: &str, tool_use_id: &str) -> String {
    json!({
        "cwd": superrepo_root.to_string_lossy().to_string(),
        "hook_event_name": event,
        "tool_name": "Bash",
        "tool_use_id": tool_use_id,
        "session_id": "submodule-bash-session",
        "transcript_path": fixture_path("example-claude-code.jsonl").to_string_lossy().to_string(),
        "tool_input": {
            "command": "generate files across the superrepo and submodules"
        }
    })
    .to_string()
}

#[test]
#[serial]
fn single_submodule_file_edit_from_superrepo_cwd_is_ai_attributed() {
    let superrepo = new_superrepo();
    seed_superrepo(&superrepo);

    let submodule_path = add_submodule(&superrepo, "vendor/lib-a");
    let submodule = submodule_repo(&submodule_path);

    let target = submodule.canonical_path().join("generated.txt");
    fs::write(&target, "AI submodule line 1\nAI submodule line 2\n").unwrap();

    checkpoint_mock_ai_from_superrepo(&superrepo, &[target]);

    let commit = submodule
        .stage_all_and_commit("add AI file in submodule")
        .unwrap();
    assert!(
        !commit.authorship_log.attestations.is_empty(),
        "submodule commit should contain AI attestations"
    );

    let mut file = submodule.filename("generated.txt");
    file.assert_lines_and_blame(vec!["AI submodule line 1".ai(), "AI submodule line 2".ai()]);
}

#[test]
#[serial]
fn multi_submodule_file_edit_from_superrepo_cwd_is_split_per_submodule() {
    let superrepo = new_superrepo();
    seed_superrepo(&superrepo);

    let submodule_a_path = add_submodule(&superrepo, "vendor/lib-a");
    let submodule_a = submodule_repo(&submodule_a_path);
    let submodule_b_path = add_submodule(&superrepo, "vendor/lib-b");
    let submodule_b = submodule_repo(&submodule_b_path);

    let file_a = submodule_a.canonical_path().join("a.txt");
    let file_b = submodule_b.canonical_path().join("b.txt");
    fs::write(&file_a, "AI line in submodule A\n").unwrap();
    fs::write(&file_b, "AI line in submodule B\n").unwrap();

    checkpoint_mock_ai_from_superrepo(&superrepo, &[file_a, file_b]);

    let commit_a = submodule_a
        .stage_all_and_commit("AI in submodule A")
        .unwrap();
    assert!(
        !commit_a.authorship_log.attestations.is_empty(),
        "first submodule should contain AI attestations"
    );
    let mut a = submodule_a.filename("a.txt");
    a.assert_lines_and_blame(vec!["AI line in submodule A".ai()]);

    let commit_b = submodule_b
        .stage_all_and_commit("AI in submodule B")
        .unwrap();
    assert!(
        !commit_b.authorship_log.attestations.is_empty(),
        "second submodule should contain AI attestations"
    );
    let mut b = submodule_b.filename("b.txt");
    b.assert_lines_and_blame(vec!["AI line in submodule B".ai()]);
}

#[test]
#[serial]
fn mixed_superrepo_and_submodule_file_edit_from_superrepo_cwd_routes_both() {
    let superrepo = new_superrepo();
    seed_superrepo(&superrepo);

    let submodule_path = add_submodule(&superrepo, "vendor/lib-a");
    let submodule = submodule_repo(&submodule_path);

    let super_file = superrepo.canonical_path().join("super_feature.txt");
    let sub_file = submodule.canonical_path().join("sub_feature.txt");
    fs::write(&super_file, "AI line in superrepo\n").unwrap();
    fs::write(&sub_file, "AI line in submodule\n").unwrap();

    checkpoint_mock_ai_from_superrepo(&superrepo, &[super_file, sub_file]);

    let sub_commit = submodule.stage_all_and_commit("AI in submodule").unwrap();
    assert!(
        !sub_commit.authorship_log.attestations.is_empty(),
        "submodule commit should contain AI attestations"
    );
    let mut sub = submodule.filename("sub_feature.txt");
    sub.assert_lines_and_blame(vec!["AI line in submodule".ai()]);

    let super_commit = superrepo.stage_all_and_commit("AI in superrepo").unwrap();
    assert!(
        !super_commit.authorship_log.attestations.is_empty(),
        "superrepo commit should contain AI attestations for its own file"
    );
    let mut super_feature = superrepo.filename("super_feature.txt");
    super_feature.assert_lines_and_blame(vec!["AI line in superrepo".ai()]);
}

#[test]
#[serial]
fn superrepo_only_file_edit_still_works_when_submodule_exists() {
    let superrepo = new_superrepo();
    seed_superrepo(&superrepo);

    let submodule_path = add_submodule(&superrepo, "vendor/lib-a");
    let _submodule = submodule_repo(&submodule_path);

    let super_file = superrepo.canonical_path().join("super_only.txt");
    fs::write(&super_file, "AI superrepo-only line\n").unwrap();

    checkpoint_mock_ai_from_superrepo(&superrepo, &[super_file]);

    let commit = superrepo
        .stage_all_and_commit("AI only in superrepo")
        .unwrap();
    assert!(
        !commit.authorship_log.attestations.is_empty(),
        "superrepo-only edit should still be AI attributed"
    );

    let mut file = superrepo.filename("super_only.txt");
    file.assert_lines_and_blame(vec!["AI superrepo-only line".ai()]);
}

#[test]
#[serial]
fn git_status_fallback_reports_submodule_file_paths() {
    let superrepo = new_superrepo();
    seed_superrepo(&superrepo);

    let submodule_path = add_submodule(&superrepo, "vendor/lib-a");
    let submodule = submodule_repo(&submodule_path);

    fs::write(
        submodule.canonical_path().join("status_fallback.txt"),
        "AI status fallback line\n",
    )
    .unwrap();

    let changed = git_status_fallback(&superrepo.canonical_path()).unwrap();
    assert!(
        changed
            .iter()
            .any(|path| path == "vendor/lib-a/status_fallback.txt"),
        "status fallback should include submodule file paths, got {:?}",
        changed
    );
}

#[test]
#[serial]
fn claude_bash_edit_across_superrepo_and_submodules_is_split_per_repo() {
    let superrepo = new_superrepo();
    seed_superrepo(&superrepo);

    let submodule_a_path = add_submodule(&superrepo, "vendor/lib-a");
    let submodule_a = submodule_repo(&submodule_a_path);
    let submodule_b_path = add_submodule(&superrepo, "vendor/lib-b");
    let submodule_b = submodule_repo(&submodule_b_path);

    let superrepo_root = superrepo.canonical_path();
    let tool_use_id = "submodule-bash-tool-use";
    let pre = claude_bash_hook_input(&superrepo_root, "PreToolUse", tool_use_id);
    superrepo
        .git_ai_from_working_dir(
            &superrepo_root,
            &["checkpoint", "claude", "--hook-input", &pre],
        )
        .expect("Claude Bash pre-hook should succeed");

    let super_file = superrepo_root.join("bash_super.txt");
    let file_a = submodule_a.canonical_path().join("bash_a.txt");
    let file_b = submodule_b.canonical_path().join("bash_b.txt");
    fs::write(&super_file, "AI Bash line in superrepo\n").unwrap();
    fs::write(&file_a, "AI Bash line in submodule A\n").unwrap();
    fs::write(&file_b, "AI Bash line in submodule B\n").unwrap();

    let post = claude_bash_hook_input(&superrepo_root, "PostToolUse", tool_use_id);
    superrepo
        .git_ai_from_working_dir(
            &superrepo_root,
            &["checkpoint", "claude", "--hook-input", &post],
        )
        .expect("Claude Bash post-hook should succeed");

    let commit_a = submodule_a
        .stage_all_and_commit("Bash AI in submodule A")
        .unwrap();
    assert!(
        !commit_a.authorship_log.attestations.is_empty(),
        "Claude Bash edit in submodule A should be AI attributed"
    );

    let commit_b = submodule_b
        .stage_all_and_commit("Bash AI in submodule B")
        .unwrap();
    assert!(
        !commit_b.authorship_log.attestations.is_empty(),
        "Claude Bash edit in submodule B should be AI attributed"
    );

    let super_commit = superrepo
        .stage_all_and_commit("Bash AI in superrepo")
        .unwrap();
    assert!(
        !super_commit.authorship_log.attestations.is_empty(),
        "Claude Bash edit in superrepo should be AI attributed"
    );

    let mut a = submodule_a.filename("bash_a.txt");
    a.assert_lines_and_blame(vec!["AI Bash line in submodule A".ai()]);
    let mut b = submodule_b.filename("bash_b.txt");
    b.assert_lines_and_blame(vec!["AI Bash line in submodule B".ai()]);
    let mut super_file = superrepo.filename("bash_super.txt");
    super_file.assert_lines_and_blame(vec!["AI Bash line in superrepo".ai()]);
}
