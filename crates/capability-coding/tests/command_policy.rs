mod common;

use common::TestDirectory;
use young_capability_coding::{CodingWorkspace, CommandApprovalPolicy, CommandPolicyDecision};

#[test]
fn low_risk_read_and_validation_commands_are_allowed() {
    let workspace = CodingWorkspace::resolve(env!("CARGO_MANIFEST_DIR"))
        .expect("capability workspace resolves");
    let policy = CommandApprovalPolicy;

    for command in [
        "pwd",
        "git diff --no-textconv --no-ext-diff -- Cargo.toml",
        "cargo check --workspace",
        "cargo test --workspace",
        "cargo clippy --workspace --all-targets",
        "cargo fmt --all -- --check",
        "rg approval src tests",
    ] {
        assert_eq!(
            policy.classify(&workspace, command),
            CommandPolicyDecision::Allow,
            "expected low-risk command to be allowed: {command}",
        );
    }
}

#[test]
fn side_effecting_and_uncertain_commands_require_an_informative_approval() {
    let workspace = CodingWorkspace::resolve(env!("CARGO_MANIFEST_DIR"))
        .expect("capability workspace resolves");
    let policy = CommandApprovalPolicy;

    for (command, expected_reason_fragment) in [
        ("touch marker.txt", "mutate workspace files"),
        ("rm -rf target/debug", "destructive"),
        ("git reset --hard HEAD~1", "destructive"),
        ("cargo add anyhow", "dependencies"),
        ("npm install", "dependencies"),
        ("sleep 30 &", "background"),
        ("git status --short", "executes a helper"),
        ("curl https://example.com", "not classified as low-risk"),
    ] {
        let CommandPolicyDecision::RequiresApproval { reason } =
            policy.classify(&workspace, command)
        else {
            panic!("expected approval for command: {command}");
        };
        assert!(
            reason.contains(expected_reason_fragment),
            "approval reason '{reason}' should describe command: {command}",
        );
    }
}

#[test]
fn explicit_cross_workspace_access_requires_approval() {
    let container = TestDirectory::new("cross-workspace");
    let root = container.path().join("workspace");
    let outside = container.path().join("outside.txt");
    std::fs::create_dir(&root).expect("workspace directory is created");
    std::fs::write(&outside, "outside\n").expect("outside fixture is written");
    let workspace = CodingWorkspace::resolve(&root).expect("workspace resolves");
    let policy = CommandApprovalPolicy;

    for command in [
        "cat ../outside.txt".to_string(),
        format!("cat {}", outside.display()),
        "cat ~young/.ssh/config".to_string(),
    ] {
        let CommandPolicyDecision::RequiresApproval { reason } =
            policy.classify(&workspace, &command)
        else {
            panic!("expected cross-workspace approval for command: {command}");
        };
        assert!(reason.contains("outside the workspace"), "{reason}");
    }

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&outside, root.join("outside-link"))
            .expect("outside symlink is created");
        let CommandPolicyDecision::RequiresApproval { reason } =
            policy.classify(&workspace, "cat outside-link")
        else {
            panic!("expected symlink escape to require approval");
        };
        assert!(reason.contains("outside the workspace"), "{reason}");

        let CommandPolicyDecision::RequiresApproval { reason } =
            policy.classify(&workspace, "cat outside-link/not-created-yet")
        else {
            panic!("expected a missing descendant of an outside symlink to require approval");
        };
        assert!(reason.contains("outside the workspace"), "{reason}");

        std::os::unix::fs::symlink(&outside, root.join("safe\\q"))
            .expect("backslash-named outside symlink is created");
        let CommandPolicyDecision::RequiresApproval { reason } =
            policy.classify(&workspace, "cat \"safe\\q\"")
        else {
            panic!("double-quoted backslash must preserve the shell's actual path");
        };
        assert!(reason.contains("outside the workspace"), "{reason}");

        std::os::unix::fs::symlink(&outside, root.join("safeq"))
            .expect("line-continuation outside symlink is created");
        let CommandPolicyDecision::RequiresApproval { reason } =
            policy.classify(&workspace, "cat safe\\\nq")
        else {
            panic!("unquoted line continuation must match the shell's actual path");
        };
        assert!(reason.contains("outside the workspace"), "{reason}");

        std::fs::create_dir(root.join("-")).expect("dash-prefixed path fixture is created");
        let CommandPolicyDecision::RequiresApproval { reason } =
            policy.classify(&workspace, "cat -- -/../../outside.txt")
        else {
            panic!("paths after -- must still receive workspace validation");
        };
        assert!(reason.contains("outside the workspace"), "{reason}");
    }
}

#[test]
fn malformed_and_clearly_unsafe_commands_are_rejected() {
    let workspace = CodingWorkspace::resolve(env!("CARGO_MANIFEST_DIR"))
        .expect("capability workspace resolves");
    let policy = CommandApprovalPolicy;
    let oversized = "x".repeat(64 * 1024 + 1);

    for (command, expected_reason_fragment) in [
        ("", "must not be empty"),
        (oversized.as_str(), "65536 bytes"),
        ("sudo rm -rf target", "privilege elevation"),
        ("/usr/bin/sudo rm -rf target", "privilege elevation"),
        ("rm -rf /", "filesystem root"),
        ("rm -rf /./", "filesystem root"),
        ("/bin/rm -rf /", "filesystem root"),
        ("rm -rf /tmp/../*", "filesystem root"),
        ("cd ..", "outside the workspace"),
        ("echo 'unterminated", "malformed shell syntax"),
    ] {
        let CommandPolicyDecision::Reject { reason } = policy.classify(&workspace, command) else {
            panic!("expected command to be rejected: {command}");
        };
        assert!(
            reason.contains(expected_reason_fragment),
            "rejection reason '{reason}' should describe command: {command}",
        );
    }
}

#[test]
fn shell_composition_cannot_hide_a_risky_operation() {
    let workspace = CodingWorkspace::resolve(env!("CARGO_MANIFEST_DIR"))
        .expect("capability workspace resolves");
    let policy = CommandApprovalPolicy;

    for (command, expected_reason_fragment) in [
        (
            "cargo test --workspace && touch marker.txt",
            "mutate workspace files",
        ),
        ("pwd > marker.txt", "redirects input or output"),
        ("echo $(touch marker.txt)", "dynamic shell expansion"),
        ("echo `touch marker.txt`", "dynamic shell expansion"),
        ("cat *.rs", "dynamic shell expansion"),
        ("rg needle --pre 'touch marker.txt'", "executes a helper"),
        ("cargo test --workspace &", "background"),
        ("cargo test --workspace\nrm -rf target", "destructive"),
        ("git status --short | tee status.txt", "executes a helper"),
    ] {
        let CommandPolicyDecision::RequiresApproval { reason } =
            policy.classify(&workspace, command)
        else {
            panic!("expected composed command to require approval: {command}");
        };
        assert!(
            reason.contains(expected_reason_fragment),
            "approval reason '{reason}' should describe command: {command}",
        );
    }
}

#[test]
fn composed_read_and_validation_commands_remain_low_risk() {
    let workspace = CodingWorkspace::resolve(env!("CARGO_MANIFEST_DIR"))
        .expect("capability workspace resolves");
    let policy = CommandApprovalPolicy;

    for command in [
        "rg approval src tests | head -n 20",
        "cargo test --workspace || cargo check --workspace",
        "printf '%s\\n' ready; pwd",
        "git branch --show-current",
        "sed -n '1,20p' Cargo.toml",
        "command -v cargo",
        "cargo metadata --no-deps",
        "cat '*.rs'",
    ] {
        assert_eq!(
            policy.classify(&workspace, command),
            CommandPolicyDecision::Allow,
            "expected composed low-risk command to be allowed: {command}",
        );
    }
}

#[test]
fn low_risk_programs_cannot_use_side_effecting_escape_hatches() {
    let workspace = CodingWorkspace::resolve(env!("CARGO_MANIFEST_DIR"))
        .expect("capability workspace resolves");
    let policy = CommandApprovalPolicy;

    for (command, expected_reason_fragment) in [
        ("git diff --output=diff.txt", "mutate workspace files"),
        ("git diff -- Cargo.toml", "executes a helper"),
        ("git log -p", "executes a helper"),
        ("git show HEAD", "executes a helper"),
        ("git grep -O less approval", "executes a helper"),
        ("git grep -Oless approval", "executes a helper"),
        (
            "git grep --open-files-in-pager=less approval",
            "executes a helper",
        ),
        (
            "git grep --open-files-in=less approval",
            "executes a helper",
        ),
        ("git diff --ext-diff", "executes a helper"),
        (
            "cargo test --config build.rustc-wrapper=./wrapper",
            "executes configured tooling",
        ),
        ("find . -fprint report.txt", "mutate workspace files"),
        ("file --compile -m magic", "mutate workspace files"),
        ("file --comp -m magic", "mutate workspace files"),
        ("file -Cm magic", "mutate workspace files"),
        ("rg -z needle archive.gz", "executes a helper"),
        ("rg -iz needle archive.gz", "executes a helper"),
        ("rg --search-zip needle archive.gz", "executes a helper"),
        ("cargo clippy --fix", "mutate workspace files"),
        (
            "find . -fprintf report.txt '%p\\n'",
            "mutate workspace files",
        ),
        (
            "sed -n '1w report.txt' Cargo.toml",
            "mutate workspace files",
        ),
        ("sed -e'1w marker.txt' 1", "mutate workspace files"),
    ] {
        let CommandPolicyDecision::RequiresApproval { reason } =
            policy.classify(&workspace, command)
        else {
            panic!("expected escape hatch to require approval: {command}");
        };
        assert!(
            reason.contains(expected_reason_fragment),
            "approval reason '{reason}' should describe command: {command}",
        );
    }
}

#[test]
fn recursive_and_indirect_read_modes_require_approval() {
    let workspace = CodingWorkspace::resolve(env!("CARGO_MANIFEST_DIR"))
        .expect("capability workspace resolves");
    let policy = CommandApprovalPolicy;

    for command in [
        "rg -L needle .",
        "rg --follow needle .",
        "grep -R needle .",
        "grep --dereference-recursive needle .",
        "find -L . -name '*.rs'",
        "find . -follow -name '*.rs'",
        "file --files-from paths.txt",
        "file -f paths.txt",
        "wc --files0-from=paths.txt",
        "find -files0-from paths.txt",
        "ls -LR .",
        "tail -F app.log",
    ] {
        assert!(
            matches!(
                policy.classify(&workspace, command),
                CommandPolicyDecision::RequiresApproval { .. }
            ),
            "expected risky read mode to require approval: {command}",
        );
    }
}

#[test]
fn path_bearing_options_cannot_hide_cross_workspace_inputs() {
    let workspace = CodingWorkspace::resolve(env!("CARGO_MANIFEST_DIR"))
        .expect("capability workspace resolves");
    let policy = CommandApprovalPolicy;

    for command in [
        "rg -f/etc/patterns needle",
        "rg --file=/etc/patterns needle",
        "grep -nf/etc/patterns needle",
        "git grep -f/etc/patterns needle",
        "git ls-files -X/etc/ignore",
        "file -m/etc/magic Cargo.toml",
        "file -M/etc/magic Cargo.toml",
        "file -m Cargo.toml:/etc/magic Cargo.toml",
        "file --magic-file=Cargo.toml:/etc/magic Cargo.toml",
    ] {
        let CommandPolicyDecision::RequiresApproval { reason } =
            policy.classify(&workspace, command)
        else {
            panic!("expected cross-workspace option path approval: {command}");
        };
        assert!(reason.contains("outside the workspace"), "{reason}");
    }
}

#[test]
fn policy_work_is_bounded_for_overly_complex_shell_input() {
    let workspace = CodingWorkspace::resolve(env!("CARGO_MANIFEST_DIR"))
        .expect("capability workspace resolves");
    let policy = CommandApprovalPolicy;
    let command = format!("rg needle {}", vec!["missing-file"; 257].join(" "));

    let CommandPolicyDecision::Reject { reason } = policy.classify(&workspace, &command) else {
        panic!("overly complex command should be rejected before path classification");
    };
    assert!(reason.contains("too complex"), "{reason}");
}
