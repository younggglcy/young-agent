use crate::workspace::CodingWorkspace;
use std::path::{Component, Path};
use young_tool_runtime::ToolCall;

pub(crate) const MAX_COMMAND_BYTES: usize = 64 * 1024;
const MAX_SIMPLE_COMMANDS: usize = 32;
const MAX_POLICY_WORDS: usize = 256;

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
        let Some(arguments) = call.arguments.as_object() else {
            return reject("run_command arguments must be an object");
        };
        let Some(command) = arguments
            .get("command")
            .and_then(|command| command.as_str())
        else {
            return reject("run_command requires a string 'command' argument");
        };
        if arguments.len() != 1 {
            return reject("run_command does not accept unknown arguments");
        }
        self.classify(workspace, command)
    }

    pub fn classify(&self, workspace: &CodingWorkspace, command: &str) -> CommandPolicyDecision {
        if command.trim().is_empty() {
            return reject("command must not be empty");
        }
        if command.len() > MAX_COMMAND_BYTES {
            return reject("command exceeds the 65536 bytes policy limit");
        }
        let parsed = match ParsedShellCommand::parse(command) {
            Ok(parsed) => parsed,
            Err(()) => return reject("command contains malformed shell syntax"),
        };
        if parsed.commands.len() > MAX_SIMPLE_COMMANDS
            || parsed.commands.iter().map(Vec::len).sum::<usize>() > MAX_POLICY_WORDS
        {
            return reject("command is too complex for the first-phase approval policy");
        }
        for words in &parsed.commands {
            if let Some(decision) = classify_hard_rejection(workspace, words) {
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
            match classify_simple_command(workspace, words) {
                CommandPolicyDecision::Allow => {}
                decision => return decision,
            }
        }
        CommandPolicyDecision::Allow
    }
}

fn classify_simple_command(workspace: &CodingWorkspace, words: &[String]) -> CommandPolicyDecision {
    let Some(program) = words.first().map(String::as_str) else {
        return reject("command contains malformed shell syntax");
    };
    let arguments = &words[1..];

    if arguments
        .iter()
        .filter_map(|word| path_candidate(word))
        .any(|path| escapes_workspace(workspace, path))
    {
        return requires_approval("command may access a path outside the workspace");
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
    if program == "git"
        && arguments
            .iter()
            .any(|argument| long_option_matches_or_abbreviates(argument, "--output"))
    {
        return requires_approval("command may mutate workspace files");
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
    if program == "find"
        && arguments.iter().any(|argument| {
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
    {
        return requires_approval("command may mutate workspace files or execute a helper");
    }
    if program == "sed" && !is_safe_sed_read(arguments) {
        return requires_approval("command may mutate workspace files or execute a helper");
    }

    let allowed = match words {
        [program] if program == "pwd" => true,
        [git, subcommand, ..]
            if git == "git"
                && matches!(
                    subcommand.as_str(),
                    "status" | "diff" | "log" | "show" | "rev-parse" | "ls-files" | "grep"
                ) =>
        {
            true
        }
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
        [command, flag, _program] if command == "command" && flag == "-v" => true,
        [program, arguments @ ..] if program == "sed" && is_safe_sed_read(arguments) => true,
        [program, ..]
            if matches!(
                program.as_str(),
                "rg" | "ls"
                    | "cat"
                    | "grep"
                    | "head"
                    | "tail"
                    | "wc"
                    | "stat"
                    | "file"
                    | "find"
                    | "echo"
                    | "printf"
                    | "true"
                    | "false"
            ) =>
        {
            true
        }
        _ => false,
    };

    if allowed {
        CommandPolicyDecision::Allow
    } else {
        requires_approval("command is not classified as low-risk")
    }
}

fn is_file_compile_option(argument: &str) -> bool {
    long_option_matches_or_abbreviates(argument, "--compile")
        || (argument.starts_with('-') && !argument.starts_with("--") && argument[1..].contains('C'))
}

fn short_option_cluster_contains(argument: &str, option: char) -> bool {
    argument.starts_with('-') && !argument.starts_with("--") && argument[1..].contains(option)
}

fn long_option_matches_or_abbreviates(argument: &str, full_option: &str) -> bool {
    let name = argument.split_once('=').map_or(argument, |(name, _)| name);
    name.starts_with("--") && name.len() > 2 && full_option.starts_with(name)
}

fn classify_hard_rejection(
    workspace: &CodingWorkspace,
    words: &[String],
) -> Option<CommandPolicyDecision> {
    let program = words.first()?;
    let arguments = &words[1..];
    if matches!(program_basename(program), "sudo" | "doas" | "su") {
        return Some(reject("command attempts privilege elevation"));
    }
    if program_basename(program) == "rm"
        && arguments
            .iter()
            .any(|argument| targets_filesystem_root(argument))
    {
        return Some(reject("command targets the filesystem root"));
    }
    if program == "cd"
        && arguments
            .first()
            .is_some_and(|path| escapes_workspace(workspace, path))
    {
        return Some(reject(
            "command changes its working directory outside the workspace",
        ));
    }
    None
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

impl ParsedShellCommand {
    fn parse(command: &str) -> Result<Self, ()> {
        let mut parsed = Self::default();
        let mut words = Vec::new();
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
                            word.push(escaped);
                        } else {
                            word.push('\\');
                            parsed.has_unclassified_syntax = true;
                        }
                        word_started = true;
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
                            word.push(escaped);
                        } else {
                            word.push('\\');
                            parsed.has_unclassified_syntax = true;
                        }
                        word_started = true;
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
                        push_word(&mut words, &mut word, &mut word_started);
                        parsed.has_redirection = true;
                    }
                    '&' => {
                        push_word(&mut words, &mut word, &mut word_started);
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
                        push_word(&mut words, &mut word, &mut word_started);
                        if characters.peek() == Some(&'|') {
                            characters.next();
                        }
                        push_command(&mut parsed.commands, &mut words)?;
                        requires_following_command = true;
                    }
                    ';' | '\n' => {
                        push_word(&mut words, &mut word, &mut word_started);
                        if !words.is_empty() {
                            push_command(&mut parsed.commands, &mut words)?;
                        }
                        requires_following_command = false;
                    }
                    character if character.is_whitespace() => {
                        push_word(&mut words, &mut word, &mut word_started);
                    }
                    _ => {
                        word.push(character);
                        word_started = true;
                    }
                },
            }
        }

        if quote != Quote::None {
            return Err(());
        }
        push_word(&mut words, &mut word, &mut word_started);
        if !words.is_empty() {
            push_command(&mut parsed.commands, &mut words)?;
            requires_following_command = false;
        }
        if parsed.commands.is_empty() || requires_following_command {
            return Err(());
        }
        Ok(parsed)
    }
}

fn push_word(words: &mut Vec<String>, word: &mut String, word_started: &mut bool) {
    if *word_started {
        words.push(std::mem::take(word));
        *word_started = false;
    }
}

fn push_command(commands: &mut Vec<Vec<String>>, words: &mut Vec<String>) -> Result<(), ()> {
    if words.is_empty() {
        return Err(());
    }
    commands.push(std::mem::take(words));
    Ok(())
}

fn path_candidate(word: &str) -> Option<&str> {
    let candidate = word.split_once('=').map_or(word, |(_, value)| value);
    (!candidate.is_empty() && candidate != "." && !candidate.starts_with('-')).then_some(candidate)
}

fn escapes_workspace(workspace: &CodingWorkspace, candidate: &str) -> bool {
    if candidate.starts_with('~') {
        return true;
    }
    let path = Path::new(candidate);
    if path
        .components()
        .any(|component| component == Component::ParentDir)
    {
        return true;
    }

    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace.context().root().join(path)
    };
    if path.is_absolute() && !path.starts_with(workspace.context().root()) {
        return true;
    }

    let mut existing_ancestor = resolved;
    loop {
        match existing_ancestor.canonicalize() {
            Ok(resolved) => return !resolved.starts_with(workspace.context().root()),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                if !existing_ancestor.pop() {
                    return true;
                }
            }
            Err(_) => return true,
        }
    }
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
