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
        "cargo check --workspace",
        "cargo test --workspace",
        "cargo clippy --workspace --all-targets",
        "cargo fmt --all -- --check",
        "rg --no-config approval src tests",
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
        ("printf -v PATH .", "shell variable"),
        ("printf -vPATH .", "shell variable"),
        ("printf '%1000000000s' x", "amplify output"),
        ("printf '%*s' 1000000000 x", "amplify output"),
        ("printf -- '%1000000000s' x", "amplify output"),
        ("git status --short", "fsmonitor"),
        ("git ls-files /definitely-outside-workspace", "fsmonitor"),
        (
            "git grep approval /definitely-outside-workspace",
            "fsmonitor",
        ),
        (
            "git diff --no-textconv --no-ext-diff -- Cargo.toml",
            "fsmonitor",
        ),
        ("curl https://example.com", "not classified as low-risk"),
        (
            "rg approval src tests",
            "preprocessor from inherited configuration",
        ),
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
        format!("cat {}=ignored", outside.display()),
        format!(
            "cargo test --target-dir {}=ignored",
            container.path().join("outside-target").display()
        ),
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

        std::os::unix::fs::symlink(
            container.path().join("outside-missing"),
            root.join("dangling-outside-link"),
        )
        .expect("dangling outside symlink is created");
        let CommandPolicyDecision::RequiresApproval { reason } =
            policy.classify(&workspace, "cat dangling-outside-link")
        else {
            panic!("an unresolved symlink must fail closed");
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
    let oversized_whitespace = " ".repeat(64 * 1024 + 1);

    for (command, expected_reason_fragment) in [
        ("", "must not be empty"),
        (oversized.as_str(), "65536 bytes"),
        (oversized_whitespace.as_str(), "65536 bytes"),
        ("sudo rm -rf target", "privilege elevation"),
        ("/usr/bin/sudo rm -rf target", "privilege elevation"),
        ("rm -rf /", "filesystem root"),
        ("rm -rf /./", "filesystem root"),
        ("/bin/rm -rf /", "filesystem root"),
        ("rm -rf /tmp/../*", "filesystem root"),
        ("find / -delete", "filesystem root"),
        ("/usr/bin/find / -delete", "filesystem root"),
        ("chmod -R 777 /", "filesystem root"),
        ("chown -R root /", "filesystem root"),
        ("env rm -rf /", "filesystem root"),
        ("env -i rm -rf /", "filesystem root"),
        ("env -iv rm -rf /", "filesystem root"),
        ("env -uFOO rm -rf /", "filesystem root"),
        ("env -P /usr/bin rm -rf /", "filesystem root"),
        ("env -- FOO=bar rm -rf /", "filesystem root"),
        ("env -- 1=foo rm -rf /", "filesystem root"),
        ("env -- FOO=bar sudo true", "privilege elevation"),
        ("env -uFOO sudo true", "privilege elevation"),
        (
            "env -S 'rm -rf /'",
            "unsupported transparent command wrapper syntax",
        ),
        ("command -- rm -rf /", "filesystem root"),
        ("command -p rm -rf /", "filesystem root"),
        ("exec rm -rf /", "filesystem root"),
        ("exec -c rm -rf /", "filesystem root"),
        (
            "env env env env env rm -rf /",
            "too many transparent command wrappers",
        ),
        ("cd ..", "outside the workspace"),
        ("echo 'unterminated", "malformed shell syntax"),
        ("; pwd", "malformed shell syntax"),
        ("pwd;;pwd", "malformed shell syntax"),
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
        ("git status --short | tee status.txt", "fsmonitor"),
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
fn unsupported_shell_syntax_fails_closed() {
    let workspace = CodingWorkspace::resolve(env!("CARGO_MANIFEST_DIR"))
        .expect("capability workspace resolves");
    let policy = CommandApprovalPolicy;

    for command in [
        "cat <<EOF\nhello\nEOF",
        "cat <(pwd)",
        "MODE=check cargo test --workspace",
        "if true; then pwd; fi",
        "for file in Cargo.toml; do cat \"$file\"; done",
        "check() { pwd; }",
    ] {
        let reason = match policy.classify(&workspace, command) {
            CommandPolicyDecision::RequiresApproval { reason }
            | CommandPolicyDecision::Reject { reason } => reason,
            CommandPolicyDecision::Allow => {
                panic!("unsupported shell syntax must not be allowed: {command}")
            }
        };
        assert!(
            !reason.trim().is_empty(),
            "fail-closed decision should explain unsupported syntax: {command}",
        );
    }
}

#[test]
fn composed_read_and_validation_commands_remain_low_risk() {
    let workspace = CodingWorkspace::resolve(env!("CARGO_MANIFEST_DIR"))
        .expect("capability workspace resolves");
    let policy = CommandApprovalPolicy;

    for command in [
        "rg --no-config approval src tests | head -n 20",
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
        ("git diff -- Cargo.toml", "fsmonitor"),
        ("git log -p", "signature verification"),
        (
            "git log -p --author --no-textconv --no-ext-diff",
            "signature verification",
        ),
        (
            "git log -p -- --no-textconv --no-ext-diff",
            "signature verification",
        ),
        (
            "git log --no-textconv --no-ext-diff -p --show-signature",
            "signature verification",
        ),
        (
            "git log --no-textconv --no-ext-diff --format='%G?'",
            "signature verification",
        ),
        (
            "git log --no-textconv --no-ext-diff",
            "signature verification",
        ),
        (
            "git show --no-textconv --no-ext-diff HEAD",
            "signature verification",
        ),
        ("git show HEAD", "signature verification"),
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
        ("file -z archive.zst", "executes a helper"),
        ("file -iZ archive.zst", "executes a helper"),
        ("file --uncompress archive.zst", "executes a helper"),
        (
            "file --uncompress-noreport archive.zst",
            "executes a helper",
        ),
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
fn magic_file_path_lists_have_a_bounded_classification_budget() {
    let workspace = CodingWorkspace::resolve(env!("CARGO_MANIFEST_DIR"))
        .expect("capability workspace resolves");
    let policy = CommandApprovalPolicy;
    let paths = |range: std::ops::Range<usize>| {
        range
            .map(|index| format!("missing-{index}"))
            .collect::<Vec<_>>()
            .join(":")
    };
    let at_limit = format!("file -m{} Cargo.toml", paths(0..255));
    let over_limit = format!(
        "file -m{} Cargo.toml; file -m{} Cargo.toml",
        paths(0..128),
        paths(128..256),
    );

    assert_eq!(
        policy.classify(&workspace, &at_limit),
        CommandPolicyDecision::Allow,
    );
    let CommandPolicyDecision::RequiresApproval { reason } =
        policy.classify(&workspace, &over_limit)
    else {
        panic!("an oversized magic-file path list must fail closed");
    };
    assert!(reason.contains("path inspection budget"), "{reason}");
}

#[test]
fn shared_deep_prefixes_cannot_multiply_workspace_path_inspections() {
    let directory = TestDirectory::new("deep-path-probe-budget");
    let relative = (0..128)
        .map(|index| format!("d{index}"))
        .collect::<Vec<_>>()
        .join("/");
    std::fs::create_dir_all(directory.path().join(&relative))
        .expect("deep workspace fixture is created");
    let workspace = CodingWorkspace::resolve(directory.path()).expect("workspace resolves");
    let policy = CommandApprovalPolicy;
    let command = format!("file -m{relative}/missing-a:{relative}/missing-b missing-input");

    let CommandPolicyDecision::RequiresApproval { reason } = policy.classify(&workspace, &command)
    else {
        panic!("component-level path probes must share one classification budget");
    };
    assert!(reason.contains("path inspection budget"), "{reason}");
}

#[test]
fn printf_format_reuse_has_a_bounded_low_risk_literal() {
    let workspace = CodingWorkspace::resolve(env!("CARGO_MANIFEST_DIR"))
        .expect("capability workspace resolves");
    let policy = CommandApprovalPolicy;
    let at_limit = format!("printf 'missing/{}%s' a b", "x".repeat(1014));
    let over_limit = format!("printf 'missing/{}%s' a b", "x".repeat(1015));

    assert_eq!(
        policy.classify(&workspace, &at_limit),
        CommandPolicyDecision::Allow,
    );
    let CommandPolicyDecision::RequiresApproval { reason } =
        policy.classify(&workspace, &over_limit)
    else {
        panic!("a reusable large printf format must require approval");
    };
    assert!(reason.contains("amplify output"), "{reason}");
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn rg_no_config_disables_an_inherited_preprocessor() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    if Command::new("rg").arg("--version").output().is_err() {
        return;
    }

    let directory = TestDirectory::new("rg-no-config");
    let helper = directory.path().join("preprocessor.sh");
    let marker = directory.path().join("preprocessor-ran");
    let config = directory.path().join("ripgreprc");
    std::fs::write(
        &helper,
        format!("#!/bin/sh\n: > '{}'\ncat \"$1\"\n", marker.display()),
    )
    .expect("preprocessor fixture is written");
    let mut permissions = std::fs::metadata(&helper)
        .expect("preprocessor metadata is available")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&helper, permissions).expect("preprocessor is executable");
    std::fs::write(&config, format!("--pre={}\n", helper.display()))
        .expect("ripgrep config fixture is written");
    std::fs::write(directory.path().join("input.txt"), "needle\n")
        .expect("search fixture is written");

    let configured = Command::new("rg")
        .args(["needle", "input.txt"])
        .current_dir(directory.path())
        .env("RIPGREP_CONFIG_PATH", &config)
        .status()
        .expect("configured ripgrep runs");
    assert!(configured.success());
    assert!(
        marker.exists(),
        "fixture must prove the config executes a helper"
    );
    std::fs::remove_file(&marker).expect("marker is reset");

    let protected = Command::new("rg")
        .args(["--no-config", "needle", "input.txt"])
        .current_dir(directory.path())
        .env("RIPGREP_CONFIG_PATH", &config)
        .status()
        .expect("protected ripgrep runs");
    assert!(protected.success());
    assert!(
        !marker.exists(),
        "--no-config must suppress the inherited helper"
    );
}

#[test]
fn commands_at_the_complexity_limits_remain_classifiable() {
    let workspace = CodingWorkspace::resolve(env!("CARGO_MANIFEST_DIR"))
        .expect("capability workspace resolves");
    let policy = CommandApprovalPolicy;
    let thirty_two_commands = vec!["pwd"; 32].join("; ");
    let two_hundred_fifty_six_words = format!("printf {}", vec!["''"; 255].join(" "));

    for command in [thirty_two_commands, two_hundred_fifty_six_words] {
        assert_eq!(
            policy.classify(&workspace, &command),
            CommandPolicyDecision::Allow,
            "a command exactly at a complexity limit should remain allowed",
        );
    }
}

#[test]
fn commands_above_each_complexity_limit_are_rejected() {
    let workspace = CodingWorkspace::resolve(env!("CARGO_MANIFEST_DIR"))
        .expect("capability workspace resolves");
    let policy = CommandApprovalPolicy;
    let thirty_three_commands = vec!["pwd"; 33].join("; ");
    let two_hundred_fifty_seven_words = format!("printf {}", vec!["''"; 256].join(" "));

    for command in [thirty_three_commands, two_hundred_fifty_seven_words] {
        let CommandPolicyDecision::Reject { reason } = policy.classify(&workspace, &command) else {
            panic!("a command above a complexity limit must be rejected");
        };
        assert!(reason.contains("too complex"), "{reason}");
    }
}

#[test]
fn non_posix_whitespace_does_not_change_the_program_identity() {
    let workspace = CodingWorkspace::resolve(env!("CARGO_MANIFEST_DIR"))
        .expect("capability workspace resolves");
    let policy = CommandApprovalPolicy;

    for command in ["pwd\u{00a0}", "pwd\r", "pwd\u{000b}"] {
        let CommandPolicyDecision::RequiresApproval { reason } =
            policy.classify(&workspace, command)
        else {
            panic!("non-POSIX whitespace must remain part of the program name: {command:?}");
        };
        assert!(reason.contains("not classified as low-risk"), "{reason}");
    }
}
