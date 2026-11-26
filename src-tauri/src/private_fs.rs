// the files Nudge writes hold the user's reminder text, so on macOS and Linux
// they are created readable by their owner only: mode 600 for files and 700 for
// folders. Windows keeps the permissions of the user's own folders

use std::fs::{self, File};
use std::io;
use std::path::Path;

#[cfg(unix)]
pub fn create_dir_all(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new().recursive(true).mode(0o700).create(path)
}

#[cfg(not(unix))]
pub fn create_dir_all(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)
}

// like File::create, but the file is mode 600 even when it already existed
#[cfg(unix)]
pub fn create(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let file = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

#[cfg(not(unix))]
pub fn create(path: &Path) -> io::Result<File> {
    File::create(path)
}

pub fn write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    create(path)?.write_all(bytes)
}

// files an earlier version made were readable by other accounts: set the
// folder, and every file and folder inside it, to the private modes. a link is
// left alone, because changing its mode would change what it points at
#[cfg(unix)]
pub fn tighten(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fn set(path: &Path, mode: u32) {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
    }
    if !fs::symlink_metadata(dir).is_ok_and(|meta| meta.is_dir()) {
        return;
    }
    set(dir, 0o700);
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            tighten(&entry.path());
        } else if kind.is_file() {
            set(&entry.path(), 0o600);
        }
    }
}

#[cfg(not(unix))]
pub fn tighten(_: &Path) {}

#[cfg(all(test, unix))]
mod tests {
    use super::{create_dir_all, tighten, write};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    fn mode(path: &std::path::Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nudge-private-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn new_folders_and_files_are_private() {
        let dir = scratch("new");
        create_dir_all(&dir.join("inner")).unwrap();
        write(&dir.join("inner").join("config.json"), b"{}").unwrap();
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join("inner")), 0o700);
        assert_eq!(mode(&dir.join("inner").join("config.json")), 0o600);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn writing_over_an_open_file_makes_it_private() {
        let dir = scratch("over");
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("config.json");
        fs::write(&file, "old").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        write(&file, b"new").unwrap();
        assert_eq!(mode(&file), 0o600);
        assert_eq!(fs::read_to_string(&file).unwrap(), "new");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tightening_fixes_older_files_and_leaves_links_alone() {
        let dir = scratch("tighten");
        let inner = dir.join("diagnostics");
        fs::create_dir_all(&inner).unwrap();
        fs::write(dir.join("config.json"), "{}").unwrap();
        fs::write(inner.join("journal.json"), "{}").unwrap();
        let outside = scratch("outside");
        fs::write(&outside, "x").unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o644)).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("link")).unwrap();
        for path in [&dir, &inner] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        for path in [dir.join("config.json"), inner.join("journal.json")] {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        }
        tighten(&dir);
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&inner), 0o700);
        assert_eq!(mode(&dir.join("config.json")), 0o600);
        assert_eq!(mode(&inner.join("journal.json")), 0o600);
        assert_eq!(mode(&outside), 0o644, "the target of a link keeps its mode");
        fs::remove_dir_all(&dir).unwrap();
        fs::remove_file(&outside).unwrap();
    }
}
