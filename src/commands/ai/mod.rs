mod claude;
mod codex;

use clap::{Args, Subcommand};
use std::error::Error;
use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path};

#[cfg(any(test, not(unix)))]
use std::fs;

#[cfg(unix)]
use std::ffi::{CStr, CString};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

#[derive(Debug, PartialEq, Eq)]
struct InstallSummary {
    written: usize,
    skipped: usize,
}

#[derive(Debug, PartialEq, Eq)]
enum InstallFileOutcome {
    Written,
    Skipped,
}

fn install_files(
    root: &Path,
    files: &[(&str, &str)],
    force: bool,
) -> Result<InstallSummary, Box<dyn Error>> {
    let mut summary = InstallSummary {
        written: 0,
        skipped: 0,
    };

    for (relative_path, content) in files {
        match install_file(root, Path::new(relative_path), content.as_bytes(), force)? {
            InstallFileOutcome::Written => {
                log::info!("Wrote {}", relative_path);
                summary.written += 1;
            }
            InstallFileOutcome::Skipped => {
                log::info!(
                    "Skipping {} (exists, use --force to overwrite)",
                    relative_path
                );
                summary.skipped += 1;
            }
        }
    }

    Ok(summary)
}

fn safe_components(path: &Path) -> io::Result<Vec<&OsStr>> {
    let components = path
        .components()
        .map(|component| match component {
            Component::Normal(value) => Ok(value),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "skill path must be relative and normalized: {}",
                    path.display()
                ),
            )),
        })
        .collect::<io::Result<Vec<_>>>()?;
    if components.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "skill path cannot be empty",
        ));
    }
    Ok(components)
}

fn symlink_error(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("refusing symlinked skill output path: {}", path.display()),
    )
}

#[cfg(unix)]
fn c_component(component: &OsStr, path: &Path) -> io::Result<CString> {
    CString::new(component.as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("skill path contains a NUL byte: {}", path.display()),
        )
    })
}

#[cfg(unix)]
fn symlink_at(parent: &File, name: &CStr) -> io::Result<Option<bool>> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: parent is an open directory, name is NUL-terminated, and stat
    // points to valid writable storage for the duration of fstatat.
    let result = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result == 0 {
        // SAFETY: fstatat initialized stat after returning success.
        let stat = unsafe { stat.assume_init() };
        return Ok(Some((stat.st_mode & libc::S_IFMT) == libc::S_IFLNK));
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ENOENT) {
        Ok(None)
    } else {
        Err(error)
    }
}

#[cfg(unix)]
fn open_or_create_directory(parent: &File, name: &CStr, path: &Path) -> io::Result<File> {
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW;
    // SAFETY: parent is an open directory and name is a NUL-terminated path
    // component. The returned descriptor is owned immediately on success.
    let mut descriptor = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if descriptor < 0 {
        let error = io::Error::last_os_error();
        if matches!(symlink_at(parent, name)?, Some(true)) {
            return Err(symlink_error(path));
        }
        if error.raw_os_error() != Some(libc::ENOENT) {
            return Err(error);
        }

        // SAFETY: the arguments identify one child of the held parent
        // descriptor. mkdirat cannot escape through another path component.
        let created = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o755) };
        if created < 0 {
            let create_error = io::Error::last_os_error();
            if create_error.raw_os_error() != Some(libc::EEXIST) {
                return Err(create_error);
            }
        }
        // SAFETY: same contract as the first openat call. O_NOFOLLOW rejects a
        // symlink installed between mkdirat and openat.
        descriptor = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
        if descriptor < 0 {
            if matches!(symlink_at(parent, name)?, Some(true)) {
                return Err(symlink_error(path));
            }
            return Err(io::Error::last_os_error());
        }
    }

    // SAFETY: descriptor is newly returned by openat and ownership transfers
    // to File exactly once.
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

#[cfg(unix)]
fn install_file(
    root: &Path,
    relative_path: &Path,
    content: &[u8],
    force: bool,
) -> io::Result<InstallFileOutcome> {
    let components = safe_components(relative_path)?;
    let mut directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(root)?;
    let mut traversed = root.to_path_buf();

    for component in &components[..components.len() - 1] {
        traversed.push(component);
        let name = c_component(component, relative_path)?;
        directory = open_or_create_directory(&directory, &name, &traversed)?;
    }

    let file_name = c_component(components[components.len() - 1], relative_path)?;
    if let Some(is_symlink) = symlink_at(&directory, &file_name)? {
        if is_symlink {
            return Err(symlink_error(&root.join(relative_path)));
        }
        if !force {
            return Ok(InstallFileOutcome::Skipped);
        }
    }

    let flags = libc::O_WRONLY
        | libc::O_CREAT
        | libc::O_CLOEXEC
        | libc::O_NOFOLLOW
        | if force { libc::O_TRUNC } else { libc::O_EXCL };
    // SAFETY: directory is the held parent directory and file_name is one
    // normalized component. O_NOFOLLOW protects the final component.
    let descriptor =
        unsafe { libc::openat(directory.as_raw_fd(), file_name.as_ptr(), flags, 0o644) };
    if descriptor < 0 {
        let error = io::Error::last_os_error();
        if matches!(symlink_at(&directory, &file_name)?, Some(true)) {
            return Err(symlink_error(&root.join(relative_path)));
        }
        if !force && error.raw_os_error() == Some(libc::EEXIST) {
            return Ok(InstallFileOutcome::Skipped);
        }
        return Err(error);
    }

    // SAFETY: descriptor is newly returned by openat and ownership transfers
    // to File exactly once.
    let mut file = unsafe { File::from_raw_fd(descriptor) };
    file.write_all(content)?;
    Ok(InstallFileOutcome::Written)
}

#[cfg(not(unix))]
fn install_file(
    root: &Path,
    relative_path: &Path,
    content: &[u8],
    force: bool,
) -> io::Result<InstallFileOutcome> {
    let components = safe_components(relative_path)?;
    let mut destination = root.to_path_buf();
    for component in components {
        destination.push(component);
        match fs::symlink_metadata(&destination) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(symlink_error(&destination));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    if destination.exists() && !force {
        return Ok(InstallFileOutcome::Skipped);
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut options = OpenOptions::new();
    options.write(true).create(true);
    if force {
        options.truncate(true);
    } else {
        options.create_new(true);
    }
    options.open(destination)?.write_all(content)?;
    Ok(InstallFileOutcome::Written)
}

fn print_summary(agent: &str, summary: &InstallSummary) {
    if summary.written > 0 {
        println!(
            "Installed Hops skills for {agent} ({} files written, {} skipped)",
            summary.written, summary.skipped
        );
    } else {
        println!(
            "All files already exist ({} skipped). Use --force to overwrite.",
            summary.skipped
        );
    }
}

#[derive(Args, Debug)]
pub struct AiArgs {
    #[command(subcommand)]
    pub command: AiCommands,
}

#[derive(Subcommand, Debug)]
pub enum AiCommands {
    /// Install Claude Code skills and configuration for hops
    Claude(claude::ClaudeArgs),
    /// Install Codex CLI agent configuration for hops
    Codex(codex::CodexArgs),
}

pub fn run(args: &AiArgs) -> Result<(), Box<dyn Error>> {
    match &args.command {
        AiCommands::Claude(a) => claude::run(a),
        AiCommands::Codex(a) => codex::run(a),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use uuid::Uuid;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("hops-ai-install-{}", Uuid::new_v4()));
            fs::create_dir_all(&path).expect("create test directory");
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn installer_preserves_existing_files_until_forced() {
        let root = TestDir::new();
        let files = [(".agents/skills/example/SKILL.md", "bundled\n")];

        let initial = install_files(&root.0, &files, false).expect("initial install");
        assert_eq!(
            initial,
            InstallSummary {
                written: 1,
                skipped: 0
            }
        );

        let destination = root.0.join(files[0].0);
        fs::write(&destination, "user version\n").expect("customize installed skill");
        let skipped = install_files(&root.0, &files, false).expect("safe reinstall");
        assert_eq!(
            skipped,
            InstallSummary {
                written: 0,
                skipped: 1
            }
        );
        assert_eq!(
            fs::read_to_string(&destination).expect("read preserved skill"),
            "user version\n"
        );

        let forced = install_files(&root.0, &files, true).expect("forced reinstall");
        assert_eq!(
            forced,
            InstallSummary {
                written: 1,
                skipped: 0
            }
        );
        assert_eq!(
            fs::read_to_string(destination).expect("read overwritten skill"),
            "bundled\n"
        );
    }

    #[test]
    fn installer_rejects_non_normalized_relative_paths() {
        let root = TestDir::new();
        for path in ["../outside", "/tmp/outside", "."] {
            let error = install_file(&root.0, Path::new(path), b"content", false)
                .expect_err("unsafe path should fail");
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
    }

    #[cfg(unix)]
    #[test]
    fn installer_rejects_symlinked_parent_directory() {
        use std::os::unix::fs::symlink;

        let root = TestDir::new();
        let outside = root.0.join("outside");
        fs::create_dir_all(&outside).expect("create outside directory");
        symlink(&outside, root.0.join(".agents")).expect("create parent symlink");

        let error = install_file(
            &root.0,
            Path::new(".agents/skills/example/SKILL.md"),
            b"bundled\n",
            true,
        )
        .expect_err("symlinked parent should fail");

        assert!(error.to_string().contains("symlinked skill output path"));
        assert!(!outside.join("skills/example/SKILL.md").exists());
    }

    #[cfg(unix)]
    #[test]
    fn installer_rejects_dangling_destination_symlink_even_when_forced() {
        use std::os::unix::fs::symlink;

        let root = TestDir::new();
        let parent = root.0.join(".agents/skills/example");
        fs::create_dir_all(&parent).expect("create skill directory");
        let missing_target = root.0.join("outside-skill.md");
        symlink(&missing_target, parent.join("SKILL.md")).expect("create dangling symlink");

        let error = install_file(
            &root.0,
            Path::new(".agents/skills/example/SKILL.md"),
            b"bundled\n",
            true,
        )
        .expect_err("dangling destination symlink should fail");

        assert!(error.to_string().contains("symlinked skill output path"));
        assert!(!missing_target.exists());
    }
}
