use flash_core::ToolRisk;

/// Classify the risk of a shell command string.
pub(super) fn command_risk(input: &str) -> ToolRisk {
    let command = input.trim().to_ascii_lowercase();
    if contains_command(
        &command,
        &["curl", "wget", "nc", "ncat", "ssh", "scp", "ftp"],
    ) || command.contains("http://")
        || command.contains("https://")
    {
        ToolRisk::Network
    } else if command.contains("rm -rf")
        || command.starts_with("rm ")
        || command.contains("git clean")
        || command.contains("git reset --hard")
        || command.contains("find ") && command.contains("-delete")
        || command.contains("shutdown")
        || command.contains("mkfs")
        || command.contains("rmtree")
    {
        ToolRisk::Destructive
    } else if !command.contains([';', '|', '>', '<', '`'])
        && !command.contains("$(")
        && is_allowlisted_command(&command)
    {
        ToolRisk::Read
    } else {
        ToolRisk::Destructive
    }
}

fn contains_command(command: &str, names: &[&str]) -> bool {
    command
        .split(|character: char| character.is_whitespace() || ";|&()".contains(character))
        .any(|token| names.contains(&token))
}

fn is_allowlisted_command(command: &str) -> bool {
    [
        "pwd",
        "ls",
        "rg",
        "git status",
        "git diff",
        "git log",
        "git show",
        "cargo test",
        "cargo check",
        "cargo clippy",
        "cargo fmt",
    ]
    .iter()
    .any(|allowed| command == *allowed || command.starts_with(&format!("{allowed} ")))
}
