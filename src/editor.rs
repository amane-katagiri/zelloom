use std::io::Write;
use std::path::Path;
use std::process::{Command, ExitStatus};

pub fn name() -> String {
    ["VISUAL", "EDITOR"]
        .iter()
        .filter_map(|key| std::env::var(key).ok())
        .find(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "vi".to_string())
}

pub fn open_hint() -> String {
    format!("press Enter again to write the task in {}", name())
}

// Runs through sh so that editors with arguments such as "code --wait" work, as git does.
fn command(editor: &str, path: &Path) -> Command {
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(format!("{editor} \"$@\""))
        .arg(editor)
        .arg(path);
    command
}

pub fn edit(
    path: &Path,
    run: impl FnOnce(Command) -> std::io::Result<ExitStatus>,
) -> anyhow::Result<()> {
    let editor = name();
    let status = run(command(&editor, path))
        .map_err(|e| anyhow::anyhow!("failed to run editor {editor:?}: {e}"))?;
    if !status.success() {
        anyhow::bail!("editor {editor:?} exited with {status}");
    }
    Ok(())
}

pub fn compose(
    initial: &str,
    run: impl FnOnce(Command) -> std::io::Result<ExitStatus>,
) -> anyhow::Result<Option<String>> {
    let path = std::env::temp_dir().join(format!(
        "loom-task-{}-{}.md",
        std::process::id(),
        ulid::Ulid::generate()
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    file.write_all(initial.as_bytes())?;
    drop(file);

    let result = edit(&path, run).and_then(|()| Ok(std::fs::read_to_string(&path)?));
    let _ = std::fs::remove_file(&path);
    let text = result?;
    let text = text.trim();
    Ok((!text.is_empty()).then(|| text.to_string()))
}
