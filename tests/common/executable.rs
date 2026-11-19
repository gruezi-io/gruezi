use anyhow::{Result, anyhow};
use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
};

/// Publish an executable script atomically.
///
/// The write runs in a dedicated writer subprocess: file descriptors opened
/// in this process would be inherited by every `Command` child spawned
/// concurrently by the parallel tests (fork copies the whole fd table and
/// CLOEXEC only closes at the child's execve), and such an inherited
/// writable handle keeps ETXTBSY alive on the script inode long after this
/// function returns - the child, not the writer, is then the last holder.
/// The writer owns a private fd table nothing else can clone from, writes a
/// unique temporary file, chmods it and renames it into place, then exits;
/// this function awaits that exit, so on return no process holds the final
/// path open for write and a later exec can never observe a writer.
///
/// # Errors
///
/// Returns errors from spawning or waiting on the writer, from writing its
/// stdin, or a non-zero writer exit.
pub fn write_executable(path: &Path, contents: &str) -> Result<()> {
    static TEMP: AtomicU64 = AtomicU64::new(0);
    let temp = path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    let mut child = Command::new("sh")
        .arg("-c")
        .arg("cat > \"$1\" && chmod 755 \"$1\" && mv \"$1\" \"$2\"")
        .arg("write-executable")
        .arg(&temp)
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()?;
    let Some(mut stdin) = child.stdin.take() else {
        return Err(anyhow!("writer subprocess stdin unavailable"));
    };
    stdin.write_all(contents.as_bytes())?;
    // Close stdin so `cat` sees EOF before we wait for the writer to finish.
    drop(stdin);
    let status = child.wait()?;
    if !status.success() {
        return Err(anyhow!("writer subprocess failed with {status}"));
    }
    Ok(())
}
