use crate::command_input::{parse_run_command_arguments, validate_run_command_text};
use crate::workspace::CodingWorkspace;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use young_tool_runtime::ToolCall;

pub(crate) const LOW_RISK_EXTERNAL_PROGRAMS: &[&str] = &[
    "git", "cargo", "rg", "ls", "cat", "grep", "head", "tail", "wc", "stat", "file", "find", "sed",
];

const MAX_SIMPLE_COMMANDS: usize = 32;
const MAX_POLICY_WORDS: usize = 256;
const MAX_PATH_PROBE_CANDIDATES: usize = 256;
const MAX_WORKSPACE_PATH_INSPECTIONS: usize = 256;
const MAX_LOW_RISK_PRINTF_FORMAT_BYTES: usize = 1024;

/// Result of classifying one concrete `run_command` invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandPolicyDecision {
    Allow,
    RequiresApproval { reason: String },
    Reject { reason: String },
}

/// Conservative first-phase policy for local command execution.
#[derive(Clone, Copy, Debug, Default)]
pub struct CommandApprovalPolicy;

impl CommandApprovalPolicy {
    pub(crate) fn classify_call(
        &self,
        workspace: &CodingWorkspace,
        call: &ToolCall,
    ) -> CommandPolicyDecision {
        let command = match parse_run_command_arguments(&call.arguments) {
            Ok(command) => command,
            Err(error) => return reject(error.reason()),
        };
        self.classify_validated(workspace, command)
    }

    pub fn classify(&self, workspace: &CodingWorkspace, command: &str) -> CommandPolicyDecision {
        if let Err(error) = validate_run_command_text(command) {
            return reject(error.reason());
        }
        self.classify_validated(workspace, command)
    }

    fn classify_validated(
        &self,
        workspace: &CodingWorkspace,
        command: &str,
    ) -> CommandPolicyDecision {
        let parsed = match ParsedShellCommand::parse(command) {
            Ok(parsed) => parsed,
            Err(ShellParseError::Malformed) => {
                return reject("command contains malformed shell syntax");
            }
            Err(ShellParseError::TooComplex) => {
                return reject("command is too complex for the first-phase approval policy");
            }
        };
        let mut path_probes = PathProbeBudget::default();
        for words in &parsed.commands {
            if let Some(decision) = classify_hard_rejection(workspace, words, &mut path_probes) {
                return decision;
            }
        }
        if parsed.has_background {
            return requires_approval("command starts or manages a background process");
        }
        if parsed.has_redirection {
            return requires_approval("command redirects input or output");
        }
        if parsed.has_dynamic_expansion || parsed.has_pathname_expansion {
            return requires_approval("command uses dynamic shell expansion");
        }
        if parsed.has_comment || parsed.has_unclassified_syntax {
            return requires_approval(
                "command contains shell syntax that is not classified as low-risk",
            );
        }

        for words in &parsed.commands {
            match classify_simple_command(workspace, words, &mut path_probes) {
                CommandPolicyDecision::Allow => {}
                decision => return decision,
            }
        }
        CommandPolicyDecision::Allow
    }
}

fn classify_simple_command(
    workspace: &CodingWorkspace,
    words: &[String],
    path_probes: &mut PathProbeBudget,
) -> CommandPolicyDecision {
    let Some(program) = words.first().map(String::as_str) else {
        return reject("command contains malformed shell syntax");
    };
    let arguments = &words[1..];

    if program == "file" && file_may_execute_uncompress_helper(arguments) {
        return requires_approval("command executes a helper while inspecting compressed files");
    }
    if program == "git"
        && arguments
            .iter()
            .any(|argument| long_option_matches_or_abbreviates(argument, "--output"))
    {
        return requires_approval("command may mutate workspace files");
    }
    if program == "git" && git_may_use_signature_verification(arguments) {
        return requires_approval("command executes configured signature verification tooling");
    }
    if program == "git"
        && arguments.iter().any(|argument| {
            short_option_cluster_contains(argument, 'O')
                || long_option_matches_or_abbreviates(argument, "--open-files-in-pager")
                || long_option_matches_or_abbreviates(argument, "--ext-diff")
                || long_option_matches_or_abbreviates(argument, "--textconv")
        })
    {
        return requires_approval("command executes a helper configured by Git");
    }
    if program == "git"
        && matches!(
            arguments.first().map(String::as_str),
            Some("status" | "diff" | "ls-files" | "grep")
        )
    {
        return requires_approval(
            "command may invoke Git's configured fsmonitor helper or start its daemon",
        );
    }
    match classify_path_bearing_options(workspace, program, arguments, path_probes) {
        PathClassification::WithinWorkspace => {}
        PathClassification::EscapesWorkspace => {
            return requires_approval("command may access a path outside the workspace");
        }
        PathClassification::BudgetExceeded => {
            return requires_approval("command exceeds the workspace path inspection budget");
        }
    }
    match classify_argument_paths(workspace, arguments, path_probes) {
        PathClassification::WithinWorkspace => {}
        PathClassification::EscapesWorkspace => {
            return requires_approval("command may access a path outside the workspace");
        }
        PathClassification::BudgetExceeded => {
            return requires_approval("command exceeds the workspace path inspection budget");
        }
    }
    if matches!(
        words,
        [cargo, operation, ..]
            if cargo == "cargo" && matches!(operation.as_str(), "add" | "install" | "update")
    ) || matches!(
        words,
        [manager, operation, ..]
            if matches!(manager.as_str(), "npm" | "pnpm" | "yarn" | "pip" | "pip3")
                && matches!(operation.as_str(), "install" | "add" | "update")
    ) {
        return requires_approval("command installs or updates dependencies");
    }
    if program == "rm"
        || matches!(words, [git, operation, ..] if git == "git" && matches!(operation.as_str(), "reset" | "clean"))
    {
        return requires_approval("command performs a destructive workspace operation");
    }
    if matches!(
        program,
        "touch" | "mkdir" | "cp" | "mv" | "chmod" | "chown" | "tee"
    ) || (program == "sed"
        && arguments
            .iter()
            .any(|argument| argument == "-i" || argument.starts_with("-i")))
    {
        return requires_approval("command may mutate workspace files");
    }
    if matches!(words, [cargo, operation, arguments @ ..]
        if cargo == "cargo"
            && operation == "clippy"
            && arguments.iter().any(|argument| argument == "--fix"))
    {
        return requires_approval("command may mutate workspace files");
    }
    if program == "cargo"
        && arguments
            .iter()
            .any(|argument| argument == "--config" || argument.starts_with("--config="))
    {
        return requires_approval("command executes configured tooling");
    }
    if program == "file"
        && arguments
            .iter()
            .any(|argument| is_file_compile_option(argument))
    {
        return requires_approval("command may mutate workspace files");
    }
    if program == "rg"
        && arguments.iter().any(|argument| {
            long_option_matches_or_abbreviates(argument, "--pre")
                || long_option_matches_or_abbreviates(argument, "--search-zip")
                || short_option_cluster_contains(argument, 'z')
        })
    {
        return requires_approval("command executes a helper while searching files");
    }
    if program == "rg" && arguments.first().map(String::as_str) != Some("--no-config") {
        return requires_approval(
            "command may execute a preprocessor from inherited configuration",
        );
    }
    if (program == "rg"
        && arguments.iter().any(|argument| {
            long_option_matches_or_abbreviates(argument, "--follow")
                || short_option_cluster_contains(argument, 'L')
        }))
        || (program == "grep"
            && arguments.iter().any(|argument| {
                long_option_matches_or_abbreviates(argument, "--dereference-recursive")
                    || short_option_cluster_contains(argument, 'R')
            }))
        || (program == "find"
            && arguments.iter().any(|argument| {
                matches!(argument.as_str(), "-L" | "-follow")
                    || argument.starts_with("-files0-from")
            }))
        || (program == "file"
            && arguments.iter().any(|argument| {
                argument == "-f"
                    || short_option_cluster_contains(argument, 'f')
                    || long_option_matches_or_abbreviates(argument, "--files-from")
            }))
        || (program == "wc"
            && arguments
                .iter()
                .any(|argument| long_option_matches_or_abbreviates(argument, "--files0-from")))
        || (program == "ls"
            && arguments.iter().any(|argument| {
                short_option_cluster_contains(argument, 'L')
                    || long_option_matches_or_abbreviates(argument, "--dereference")
            }))
    {
        return requires_approval("command may follow or load paths outside the workspace");
    }
    if program == "tail"
        && arguments.iter().any(|argument| {
            short_option_cluster_contains(argument, 'f')
                || short_option_cluster_contains(argument, 'F')
                || long_option_matches_or_abbreviates(argument, "--follow")
                || long_option_matches_or_abbreviates(argument, "--retry")
                || long_option_matches_or_abbreviates(argument, "--pid")
        })
    {
        return requires_approval("command starts a long-running file monitor");
    }
    if program == "printf" {
        if arguments.first().is_some_and(|argument| {
            argument == "-v" || (argument.starts_with("-v") && argument.len() > 2)
        }) {
            return requires_approval("command changes a shell variable");
        }
        if printf_format_may_amplify_output(arguments) {
            return requires_approval("command may amplify output without a fixed resource bound");
        }
    }
    if program == "find" && find_may_mutate_or_execute(arguments) {
        return requires_approval("command may mutate workspace files or execute a helper");
    }
    if program == "sed" && !is_safe_sed_read(arguments) {
        return requires_approval("command may mutate workspace files or execute a helper");
    }

    let allowed = match words {
        [program] if program == "pwd" => true,
        [command, flag, _program] if command == "command" && flag == "-v" => true,
        [program, ..] if matches!(program.as_str(), "echo" | "printf" | "true" | "false") => true,
        _ => false,
    } || is_low_risk_external_command(words);

    if allowed {
        CommandPolicyDecision::Allow
    } else {
        requires_approval("command is not classified as low-risk")
    }
}

fn is_low_risk_external_command(words: &[String]) -> bool {
    let Some(program) = words.first().map(String::as_str) else {
        return false;
    };
    if !LOW_RISK_EXTERNAL_PROGRAMS.contains(&program) {
        return false;
    }

    match words {
        [git, subcommand, ..] if git == "git" && subcommand == "rev-parse" => true,
        [git, branch, option]
            if git == "git" && branch == "branch" && option == "--show-current" =>
        {
            true
        }
        [cargo, operation, ..]
            if cargo == "cargo"
                && matches!(operation.as_str(), "check" | "test" | "clippy" | "metadata") =>
        {
            true
        }
        [cargo, operation, arguments @ ..]
            if cargo == "cargo"
                && operation == "fmt"
                && arguments.iter().any(|argument| argument == "--check") =>
        {
            true
        }
        [sed, arguments @ ..] if sed == "sed" && is_safe_sed_read(arguments) => true,
        [program, ..] if !matches!(program.as_str(), "git" | "cargo" | "sed") => true,
        _ => false,
    }
}

fn find_may_mutate_or_execute(arguments: &[String]) -> bool {
    arguments.iter().any(|argument| {
        matches!(
            argument.as_str(),
            "-delete"
                | "-exec"
                | "-execdir"
                | "-ok"
                | "-okdir"
                | "-fprint"
                | "-fprint0"
                | "-fprintf"
                | "-fls"
        )
    })
}

fn is_file_compile_option(argument: &str) -> bool {
    long_option_matches_or_abbreviates(argument, "--compile")
        || (argument.starts_with('-') && !argument.starts_with("--") && argument[1..].contains('C'))
}

fn file_may_execute_uncompress_helper(arguments: &[String]) -> bool {
    arguments.iter().any(|argument| {
        short_option_cluster_contains(argument, 'z')
            || short_option_cluster_contains(argument, 'Z')
            || long_option_matches_or_abbreviates(argument, "--uncompress")
            || long_option_matches_or_abbreviates(argument, "--uncompress-noreport")
    })
}

fn short_option_cluster_contains(argument: &str, option: char) -> bool {
    argument.starts_with('-') && !argument.starts_with("--") && argument[1..].contains(option)
}

fn git_may_use_signature_verification(arguments: &[String]) -> bool {
    matches!(arguments.first().map(String::as_str), Some("log" | "show"))
        || arguments.iter().any(|argument| {
            long_option_matches_or_abbreviates(argument, "--show-signature")
                || argument.as_bytes().windows(2).any(|window| window == b"%G")
        })
}

fn printf_format_may_amplify_output(arguments: &[String]) -> bool {
    let (format, operands) = match arguments {
        [separator, format, operands @ ..] if separator == "--" => (format, operands),
        [format, operands @ ..] => (format, operands),
        [] => return false,
    };
    if format.len() > MAX_LOW_RISK_PRINTF_FORMAT_BYTES && operands.len() > 1 {
        return true;
    }
    let mut in_directive = false;

    for character in format.chars() {
        if !in_directive {
            in_directive = character == '%';
            continue;
        }
        match character {
            '%' => in_directive = false,
            '*' => return true,
            character if character.is_ascii_digit() => return true,
            character if character.is_ascii_alphabetic() => in_directive = false,
            _ => {}
        }
    }
    false
}

fn long_option_matches_or_abbreviates(argument: &str, full_option: &str) -> bool {
    let name = argument.split_once('=').map_or(argument, |(name, _)| name);
    name.starts_with("--") && name.len() > 2 && full_option.starts_with(name)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PathClassification {
    WithinWorkspace,
    EscapesWorkspace,
    BudgetExceeded,
}

struct PathProbeBudget {
    remaining_candidates: usize,
    remaining_inspections: usize,
    cached: HashMap<String, bool>,
}

impl Default for PathProbeBudget {
    fn default() -> Self {
        Self {
            remaining_candidates: MAX_PATH_PROBE_CANDIDATES,
            remaining_inspections: MAX_WORKSPACE_PATH_INSPECTIONS,
            cached: HashMap::new(),
        }
    }
}

impl PathProbeBudget {
    fn classify(&mut self, workspace: &CodingWorkspace, candidate: &str) -> PathClassification {
        if let Some(escapes) = self.cached.get(candidate) {
            return if *escapes {
                PathClassification::EscapesWorkspace
            } else {
                PathClassification::WithinWorkspace
            };
        }
        if self.remaining_candidates == 0 {
            return PathClassification::BudgetExceeded;
        }
        self.remaining_candidates -= 1;
        let Some(escapes) =
            escapes_workspace(workspace, candidate, &mut self.remaining_inspections)
        else {
            return PathClassification::BudgetExceeded;
        };
        self.cached.insert(candidate.to_string(), escapes);
        if escapes {
            PathClassification::EscapesWorkspace
        } else {
            PathClassification::WithinWorkspace
        }
    }
}

fn classify_path_bearing_options(
    workspace: &CodingWorkspace,
    program: &str,
    arguments: &[String],
    path_probes: &mut PathProbeBudget,
) -> PathClassification {
    match (program, arguments) {
        ("rg" | "grep", arguments) => {
            classify_option_paths(workspace, arguments, &['f'], "--file", false, path_probes)
        }
        ("git", [subcommand, arguments @ ..]) if subcommand == "grep" => {
            classify_option_paths(workspace, arguments, &['f'], "--file", false, path_probes)
        }
        ("git", [subcommand, arguments @ ..]) if subcommand == "ls-files" => classify_option_paths(
            workspace,
            arguments,
            &['X'],
            "--exclude-from",
            false,
            path_probes,
        ),
        ("file", arguments) => classify_option_paths(
            workspace,
            arguments,
            &['m', 'M'],
            "--magic-file",
            true,
            path_probes,
        ),
        _ => PathClassification::WithinWorkspace,
    }
}

fn classify_option_paths(
    workspace: &CodingWorkspace,
    arguments: &[String],
    short_options: &[char],
    long_option: &str,
    colon_separated: bool,
    path_probes: &mut PathProbeBudget,
) -> PathClassification {
    let mut values = Vec::new();
    for (index, argument) in arguments.iter().enumerate() {
        let value = if short_options
            .iter()
            .any(|option| is_exact_short_option(argument, *option))
            || long_option_matches_or_abbreviates(argument, long_option) && !argument.contains('=')
        {
            arguments.get(index + 1).map(String::as_str)
        } else if let Some((_, value)) = argument.split_once('=') {
            long_option_matches_or_abbreviates(argument, long_option).then_some(value)
        } else {
            attached_short_option_value(argument, short_options)
        };
        if let Some(value) = value {
            values.push(value);
        }
    }

    if colon_separated {
        for path in values.into_iter().flat_map(|value| value.split(':')) {
            let classification = path_probes.classify(workspace, path);
            if classification != PathClassification::WithinWorkspace {
                return classification;
            }
        }
    } else {
        for path in values {
            let classification = path_probes.classify(workspace, path);
            if classification != PathClassification::WithinWorkspace {
                return classification;
            }
        }
    }
    PathClassification::WithinWorkspace
}

fn is_exact_short_option(argument: &str, option: char) -> bool {
    argument
        .strip_prefix('-')
        .is_some_and(|argument| argument.len() == option.len_utf8() && argument.starts_with(option))
}

fn attached_short_option_value<'a>(argument: &'a str, options: &[char]) -> Option<&'a str> {
    if !argument.starts_with('-') || argument.starts_with("--") {
        return None;
    }
    let option_index = argument
        .char_indices()
        .skip(1)
        .find_map(|(index, character)| {
            options
                .contains(&character)
                .then_some(index + character.len_utf8())
        })?;
    (option_index < argument.len()).then_some(&argument[option_index..])
}

fn classify_hard_rejection(
    workspace: &CodingWorkspace,
    words: &[String],
    path_probes: &mut PathProbeBudget,
) -> Option<CommandPolicyDecision> {
    let invocation = match unwrap_transparent_command_wrappers(words) {
        Ok(invocation) => invocation,
        Err(error) => return Some(reject(error.reason())),
    };
    let program = invocation.first()?;
    let arguments = &invocation[1..];
    if matches!(program_basename(program), "sudo" | "doas" | "su") {
        return Some(reject("command attempts privilege elevation"));
    }
    if command_mutates_filesystem_root(program, arguments) {
        return Some(reject("command targets the filesystem root"));
    }
    if program == "cd" {
        match arguments
            .first()
            .map_or(PathClassification::WithinWorkspace, |path| {
                path_probes.classify(workspace, path)
            }) {
            PathClassification::WithinWorkspace => {}
            PathClassification::EscapesWorkspace => {
                return Some(reject(
                    "command changes its working directory outside the workspace",
                ));
            }
            PathClassification::BudgetExceeded => {
                return Some(requires_approval(
                    "command exceeds the workspace path inspection budget",
                ));
            }
        }
    }
    None
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TransparentWrapperError {
    TooDeep,
    UnsupportedSyntax,
}

impl TransparentWrapperError {
    fn reason(self) -> &'static str {
        match self {
            Self::TooDeep => "command uses too many transparent command wrappers",
            Self::UnsupportedSyntax => {
                "command uses unsupported transparent command wrapper syntax"
            }
        }
    }
}

fn unwrap_transparent_command_wrappers(
    mut words: &[String],
) -> Result<&[String], TransparentWrapperError> {
    const MAX_TRANSPARENT_WRAPPER_DEPTH: usize = 4;

    for depth in 0..=MAX_TRANSPARENT_WRAPPER_DEPTH {
        let Some(program) = words.first() else {
            return Ok(words);
        };
        let argument_index = match program_basename(program) {
            "command" => command_wrapped_program_index(words),
            "exec" => exec_wrapped_program_index(words),
            "env" => env_wrapped_program_index(words),
            _ => return Ok(words),
        }?;
        let Some(argument_index) = argument_index else {
            return Ok(words);
        };
        if depth == MAX_TRANSPARENT_WRAPPER_DEPTH {
            return Err(TransparentWrapperError::TooDeep);
        }
        words = &words[argument_index..];
    }
    unreachable!("transparent wrapper loop returns at its bounded depth")
}

fn command_wrapped_program_index(
    words: &[String],
) -> Result<Option<usize>, TransparentWrapperError> {
    let mut index = 1;
    while let Some(argument) = words.get(index) {
        match argument.as_str() {
            "--" => return Ok((index + 1 < words.len()).then_some(index + 1)),
            "-p" => index += 1,
            "-v" | "-V" => return Ok(None),
            _ if argument.starts_with('-') => {
                return Err(TransparentWrapperError::UnsupportedSyntax);
            }
            _ => return Ok(Some(index)),
        }
    }
    Ok(None)
}

fn exec_wrapped_program_index(words: &[String]) -> Result<Option<usize>, TransparentWrapperError> {
    let mut index = 1;
    while let Some(argument) = words.get(index) {
        match argument.as_str() {
            "--" => return Ok((index + 1 < words.len()).then_some(index + 1)),
            "-a" => index += 2,
            _ if argument.starts_with('-')
                && argument[1..]
                    .chars()
                    .all(|option| matches!(option, 'c' | 'l')) =>
            {
                index += 1;
            }
            _ if argument.starts_with('-') => {
                return Err(TransparentWrapperError::UnsupportedSyntax);
            }
            _ => return Ok(Some(index)),
        }
    }
    Ok(None)
}

fn env_wrapped_program_index(words: &[String]) -> Result<Option<usize>, TransparentWrapperError> {
    let mut index = 1;
    while let Some(argument) = words.get(index) {
        match argument.as_str() {
            "--" => {
                index += 1;
                while words.get(index).is_some_and(|word| is_env_assignment(word)) {
                    index += 1;
                }
                return Ok((index < words.len()).then_some(index));
            }
            "-u" | "--unset" | "-C" | "--chdir" | "-P" | "-a" | "--argv0" => {
                index += 2;
            }
            "-S" | "--split-string" => {
                return Err(TransparentWrapperError::UnsupportedSyntax);
            }
            "--help" | "--version" => return Ok(None),
            _ if argument.starts_with("--unset=")
                || argument.starts_with("--chdir=")
                || argument.starts_with("--argv0=") =>
            {
                index += 1;
            }
            _ if argument.starts_with("-S") || argument.starts_with("--split-string=") => {
                return Err(TransparentWrapperError::UnsupportedSyntax);
            }
            _ if env_short_option_has_attached_value(argument) => index += 1,
            _ if env_short_option_cluster_has_no_value(argument) => index += 1,
            _ if argument.starts_with("--block-signal")
                || argument.starts_with("--default-signal")
                || argument.starts_with("--ignore-signal")
                || argument == "--list-signal-handling"
                || argument == "--debug" =>
            {
                index += 1;
            }
            _ if argument.starts_with('-') => {
                return Err(TransparentWrapperError::UnsupportedSyntax);
            }
            _ if is_env_assignment(argument) => index += 1,
            _ => return Ok(Some(index)),
        }
    }
    Ok(None)
}

fn env_short_option_has_attached_value(argument: &str) -> bool {
    ["-u", "-C", "-P", "-a"].iter().any(|option| {
        argument
            .strip_prefix(option)
            .is_some_and(|value| !value.is_empty())
    })
}

fn env_short_option_cluster_has_no_value(argument: &str) -> bool {
    argument.strip_prefix('-').is_some_and(|options| {
        !options.is_empty()
            && options
                .chars()
                .all(|option| matches!(option, '0' | 'i' | 'v'))
    })
}

fn is_env_assignment(word: &str) -> bool {
    word.split_once('=')
        .is_some_and(|(name, _value)| !name.is_empty())
}

fn command_mutates_filesystem_root(program: &str, arguments: &[String]) -> bool {
    if !arguments
        .iter()
        .any(|argument| targets_filesystem_root(argument))
    {
        return false;
    }

    match program_basename(program) {
        "rm" | "chmod" | "chown" => true,
        "find" => find_may_mutate_or_execute(arguments),
        _ => false,
    }
}

fn targets_filesystem_root(argument: &str) -> bool {
    let path = Path::new(argument);
    if !path.is_absolute() {
        return false;
    }

    let mut depth = 0usize;
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::CurDir => {}
            Component::ParentDir => depth = depth.saturating_sub(1),
            Component::Normal(component) => {
                let component = component.to_string_lossy();
                if depth == 0
                    && component
                        .chars()
                        .any(|character| matches!(character, '*' | '?' | '['))
                {
                    return true;
                }
                depth += 1;
            }
        }
    }
    depth == 0
}

fn is_safe_sed_read(arguments: &[String]) -> bool {
    let mut scripts = Vec::new();
    let mut index = 0;
    let mut implicit_script_seen = false;

    while index < arguments.len() {
        let argument = &arguments[index];
        match argument.as_str() {
            "-n" | "--quiet" | "--silent" => index += 1,
            "-e" | "--expression" => {
                let Some(script) = arguments.get(index + 1) else {
                    return false;
                };
                scripts.push(script.as_str());
                index += 2;
            }
            "--" => {
                index += 1;
                if scripts.is_empty() {
                    let Some(script) = arguments.get(index) else {
                        return false;
                    };
                    scripts.push(script.as_str());
                }
                break;
            }
            _ if argument.starts_with("--expression=") => {
                scripts.push(&argument["--expression=".len()..]);
                index += 1;
            }
            _ if argument.starts_with("-e") && argument.len() > 2 => {
                scripts.push(&argument[2..]);
                index += 1;
            }
            _ if argument.starts_with('-') => return false,
            _ if scripts.is_empty() && !implicit_script_seen => {
                scripts.push(argument.as_str());
                implicit_script_seen = true;
                index += 1;
            }
            _ => index += 1,
        }
    }

    !scripts.is_empty() && scripts.into_iter().all(is_safe_sed_script)
}

fn is_safe_sed_script(script: &str) -> bool {
    !script.is_empty()
        && script.chars().all(|character| {
            character.is_ascii_digit() || matches!(character, ',' | '$' | 'p' | 'q')
        })
}

fn program_basename(program: &str) -> &str {
    Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program)
}

#[derive(Default)]
struct ParsedShellCommand {
    commands: Vec<Vec<String>>,
    has_background: bool,
    has_redirection: bool,
    has_dynamic_expansion: bool,
    has_pathname_expansion: bool,
    has_comment: bool,
    has_unclassified_syntax: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Quote {
    None,
    Single,
    Double,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShellParseError {
    Malformed,
    TooComplex,
}

impl ParsedShellCommand {
    fn parse(command: &str) -> Result<Self, ShellParseError> {
        let mut parsed = Self::default();
        let mut words = Vec::new();
        let mut word_count = 0;
        let mut word = String::new();
        let mut word_started = false;
        let mut quote = Quote::None;
        let mut characters = command.chars().peekable();
        let mut requires_following_command = false;

        while let Some(character) = characters.next() {
            match quote {
                Quote::Single => {
                    if character == '\'' {
                        quote = Quote::None;
                    } else {
                        word.push(character);
                    }
                    word_started = true;
                }
                Quote::Double => match character {
                    '"' => quote = Quote::None,
                    '\\' => {
                        if let Some(escaped) = characters.next() {
                            match escaped {
                                '$' | '`' | '"' | '\\' => {
                                    word.push(escaped);
                                    word_started = true;
                                }
                                '\n' => {}
                                _ => {
                                    word.push('\\');
                                    word.push(escaped);
                                    word_started = true;
                                }
                            }
                        } else {
                            word.push('\\');
                            parsed.has_unclassified_syntax = true;
                            word_started = true;
                        }
                    }
                    '$' | '`' => {
                        parsed.has_dynamic_expansion = true;
                        word.push(character);
                        word_started = true;
                    }
                    _ => {
                        word.push(character);
                        word_started = true;
                    }
                },
                Quote::None => match character {
                    '\'' => {
                        quote = Quote::Single;
                        word_started = true;
                    }
                    '"' => {
                        quote = Quote::Double;
                        word_started = true;
                    }
                    '\\' => {
                        if let Some(escaped) = characters.next() {
                            if escaped != '\n' {
                                word.push(escaped);
                                word_started = true;
                            }
                        } else {
                            word.push('\\');
                            parsed.has_unclassified_syntax = true;
                            word_started = true;
                        }
                    }
                    '$' | '`' | '(' | ')' | '{' | '}' => {
                        parsed.has_dynamic_expansion = true;
                        word.push(character);
                        word_started = true;
                    }
                    '*' | '?' | '[' => {
                        parsed.has_pathname_expansion = true;
                        word.push(character);
                        word_started = true;
                    }
                    '#' if !word_started => {
                        parsed.has_comment = true;
                        break;
                    }
                    '>' | '<' => {
                        push_word(&mut words, &mut word, &mut word_started, &mut word_count)?;
                        parsed.has_redirection = true;
                    }
                    '&' => {
                        push_word(&mut words, &mut word, &mut word_started, &mut word_count)?;
                        if characters.peek() == Some(&'&') {
                            characters.next();
                            push_command(&mut parsed.commands, &mut words)?;
                            requires_following_command = true;
                        } else {
                            push_command(&mut parsed.commands, &mut words)?;
                            parsed.has_background = true;
                            requires_following_command = false;
                        }
                    }
                    '|' => {
                        push_word(&mut words, &mut word, &mut word_started, &mut word_count)?;
                        if characters.peek() == Some(&'|') {
                            characters.next();
                        }
                        push_command(&mut parsed.commands, &mut words)?;
                        requires_following_command = true;
                    }
                    ';' => {
                        push_word(&mut words, &mut word, &mut word_started, &mut word_count)?;
                        if words.is_empty() {
                            return Err(ShellParseError::Malformed);
                        }
                        push_command(&mut parsed.commands, &mut words)?;
                        requires_following_command = false;
                    }
                    '\n' => {
                        push_word(&mut words, &mut word, &mut word_started, &mut word_count)?;
                        if !words.is_empty() {
                            push_command(&mut parsed.commands, &mut words)?;
                        }
                        requires_following_command = false;
                    }
                    ' ' | '\t' => {
                        push_word(&mut words, &mut word, &mut word_started, &mut word_count)?;
                    }
                    _ => {
                        word.push(character);
                        word_started = true;
                    }
                },
            }
        }

        if quote != Quote::None {
            return Err(ShellParseError::Malformed);
        }
        push_word(&mut words, &mut word, &mut word_started, &mut word_count)?;
        if !words.is_empty() {
            push_command(&mut parsed.commands, &mut words)?;
            requires_following_command = false;
        }
        if parsed.commands.is_empty() || requires_following_command {
            return Err(ShellParseError::Malformed);
        }
        Ok(parsed)
    }
}

fn push_word(
    words: &mut Vec<String>,
    word: &mut String,
    word_started: &mut bool,
    word_count: &mut usize,
) -> Result<(), ShellParseError> {
    if *word_started {
        if *word_count >= MAX_POLICY_WORDS {
            return Err(ShellParseError::TooComplex);
        }
        words.push(std::mem::take(word));
        *word_started = false;
        *word_count += 1;
    }
    Ok(())
}

fn push_command(
    commands: &mut Vec<Vec<String>>,
    words: &mut Vec<String>,
) -> Result<(), ShellParseError> {
    if words.is_empty() {
        return Err(ShellParseError::Malformed);
    }
    if commands.len() >= MAX_SIMPLE_COMMANDS {
        return Err(ShellParseError::TooComplex);
    }
    commands.push(std::mem::take(words));
    Ok(())
}

fn path_candidate(word: &str) -> Option<&str> {
    let candidate = if word.starts_with("--") {
        word.split_once('=').map_or(word, |(_, value)| value)
    } else {
        word
    };
    (!candidate.is_empty() && candidate != "." && !candidate.starts_with('-')).then_some(candidate)
}

fn classify_argument_paths(
    workspace: &CodingWorkspace,
    arguments: &[String],
    path_probes: &mut PathProbeBudget,
) -> PathClassification {
    let mut positional_only = false;
    for argument in arguments {
        if argument == "--" && !positional_only {
            positional_only = true;
            continue;
        }
        let candidate = if positional_only {
            Some(argument.as_str())
        } else {
            path_candidate(argument)
        };
        if let Some(path) = candidate {
            let classification = path_probes.classify(workspace, path);
            if classification != PathClassification::WithinWorkspace {
                return classification;
            }
        }
    }
    PathClassification::WithinWorkspace
}

fn escapes_workspace(
    workspace: &CodingWorkspace,
    candidate: &str,
    remaining_inspections: &mut usize,
) -> Option<bool> {
    if candidate.starts_with('~') {
        return Some(true);
    }
    let path = Path::new(candidate);
    if path
        .components()
        .any(|component| component == Component::ParentDir)
    {
        return Some(true);
    }

    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace.context().root().join(path)
    };
    if path.is_absolute() && !path.starts_with(workspace.context().root()) {
        return Some(true);
    }

    path_escapes_workspace_with(workspace.context().root(), &resolved, &mut |path| {
        inspect_workspace_path(path, remaining_inspections)
    })
}

enum PathInspection {
    Missing,
    Existing,
    Symlink(PathBuf),
    Inaccessible,
}

fn inspect_workspace_path(
    path: &Path,
    remaining_inspections: &mut usize,
) -> Option<PathInspection> {
    if *remaining_inspections == 0 {
        return None;
    }
    *remaining_inspections -= 1;
    match path.symlink_metadata() {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            if *remaining_inspections == 0 {
                return None;
            }
            *remaining_inspections -= 1;
            Some(match path.canonicalize() {
                Ok(resolved) => PathInspection::Symlink(resolved),
                Err(_) => PathInspection::Inaccessible,
            })
        }
        Ok(_) => Some(PathInspection::Existing),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            Some(PathInspection::Missing)
        }
        Err(_) => Some(PathInspection::Inaccessible),
    }
}

fn path_escapes_workspace_with(
    root: &Path,
    resolved: &Path,
    inspect: &mut impl FnMut(&Path) -> Option<PathInspection>,
) -> Option<bool> {
    let Ok(relative) = resolved.strip_prefix(root) else {
        return Some(true);
    };
    let mut current = root.to_path_buf();

    for component in relative.components() {
        let Component::Normal(component) = component else {
            if component == Component::CurDir {
                continue;
            }
            return Some(true);
        };
        current.push(component);
        match inspect(&current)? {
            PathInspection::Missing => return Some(false),
            PathInspection::Existing => {}
            PathInspection::Symlink(resolved) => {
                if !resolved.starts_with(root) {
                    return Some(true);
                }
                current = resolved;
            }
            PathInspection::Inaccessible => return Some(true),
        }
    }
    Some(false)
}

fn requires_approval(reason: &str) -> CommandPolicyDecision {
    CommandPolicyDecision::RequiresApproval {
        reason: reason.to_string(),
    }
}

fn reject(reason: &str) -> CommandPolicyDecision {
    CommandPolicyDecision::Reject {
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{path_escapes_workspace_with, PathInspection};
    use std::path::Path;

    #[test]
    fn deep_missing_path_stops_at_its_first_missing_prefix() {
        let root = Path::new("/workspace");
        let candidate = root.join(
            std::iter::repeat_n("missing", 128)
                .collect::<Vec<_>>()
                .join("/"),
        );
        let mut inspections = 0;

        let escaped = path_escapes_workspace_with(root, &candidate, &mut |_| {
            inspections += 1;
            Some(PathInspection::Missing)
        });

        assert_eq!(escaped, Some(false));
        assert_eq!(inspections, 1);
    }
}
