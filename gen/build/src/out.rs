use crate::error::{Error, Result};
use crate::gen::fs;
use crate::paths;
use std::path::{Path, PathBuf};
use std::{env, io};

pub(crate) fn write(path: impl AsRef<Path>, content: &[u8]) -> Result<()> {
    let path = path.as_ref();

    let mut create_dir_error = None;
    if fs::exists(path) {
        if let Ok(existing) = fs::read(path) {
            if existing == content {
                // Avoid bumping modified time with unchanged contents.
                return Ok(());
            }
        }
        best_effort_remove(path);
    } else {
        let parent = path.parent().unwrap();
        create_dir_error = fs::create_dir_all(parent).err();
    }

    match fs::write(path, content) {
        // As long as write succeeded, ignore any create_dir_all error.
        Ok(()) => Ok(()),
        // If create_dir_all and write both failed, prefer the first error.
        Err(err) => Err(Error::Fs(create_dir_error.unwrap_or(err))),
    }
}

pub(crate) fn symlink_file(original: impl AsRef<Path>, link: impl AsRef<Path>) -> Result<()> {
    let original = original.as_ref();
    let link = link.as_ref();

    let original = best_effort_relativize_symlink(original, link);

    let mut create_dir_error = None;
    if fs::exists(link) {
        best_effort_remove(link);
    } else {
        let parent = link.parent().unwrap();
        create_dir_error = fs::create_dir_all(parent).err();
    }

    match paths::symlink_or_copy(original, link) {
        // As long as symlink_or_copy succeeded, ignore any create_dir_all error.
        Ok(()) => Ok(()),
        Err(err) => {
            if err.kind() == io::ErrorKind::AlreadyExists {
                // This is fine, a different simultaneous build script already
                // created the same link or copy. The cxx_build target directory
                // is laid out such that the same path never refers to two
                // different targets during the same multi-crate build, so if
                // some other build script already created the same path then we
                // know it refers to the identical target that the current build
                // script was trying to create.
                Ok(())
            } else {
                // If create_dir_all and symlink_or_copy both failed, prefer the
                // first error.
                Err(Error::Fs(create_dir_error.unwrap_or(err)))
            }
        }
    }
}

pub(crate) fn symlink_dir(original: impl AsRef<Path>, link: impl AsRef<Path>) -> Result<()> {
    let original = best_effort_relativize_symlink(original.as_ref(), link.as_ref());
    let link = link.as_ref();

    let mut create_dir_error = None;
    if fs::exists(link) {
        best_effort_remove(link);
    } else {
        let parent = link.parent().unwrap();
        create_dir_error = fs::create_dir_all(parent).err();
    }

    match fs::symlink_dir(original, link) {
        // As long as symlink_dir succeeded, ignore any create_dir_all error.
        Ok(()) => Ok(()),
        // If create_dir_all and symlink_dir both failed, prefer the first error.
        Err(err) => Err(Error::Fs(create_dir_error.unwrap_or(err))),
    }
}

fn best_effort_remove(path: &Path) {
    use std::fs;

    if cfg!(windows) {
        // On Windows, the correct choice of remove_file vs remove_dir needs to
        // be used according to what the symlink *points to*. Trying to use
        // remove_file to remove a symlink which points to a directory fails
        // with "Access is denied".
        if let Ok(metadata) = fs::metadata(path) {
            if metadata.is_dir() {
                let _ = fs::remove_dir_all(path);
            } else {
                let _ = fs::remove_file(path);
            }
        } else if fs::symlink_metadata(path).is_ok() {
            // The symlink might exist but be dangling, in which case there is
            // no standard way to determine what "kind" of symlink it is. Try
            // deleting both ways.
            if fs::remove_dir_all(path).is_err() {
                let _ = fs::remove_file(path);
            }
        }
    } else {
        // On non-Windows, we check metadata not following symlinks. All
        // symlinks are removed using remove_file.
        if let Ok(metadata) = fs::symlink_metadata(path) {
            if metadata.is_dir() {
                let _ = fs::remove_dir_all(path);
            } else {
                let _ = fs::remove_file(path);
            }
        }
    }
}

fn best_effort_relativize_symlink(original: impl AsRef<Path>, link: impl AsRef<Path>) -> PathBuf {
    let original = original.as_ref();
    let link = link.as_ref();

    // relativization only makes sense if there is a semantically meaningful root between the two
    // (aka it's unlikely that a user moving a directory will cause a break).
    // e.g. /Volumes/code/library/src/lib.rs and /Volumes/code/library/target/path/to/something.a
    // have a meaningful shared root of /Volumes/code/library, as the person who moves target
    // out of library would expect it to break.
    // on the other hand, /Volumes/code/library/src/lib.rs and /Volumes/shared_target do not, since
    // moving library to a different location should not be expected to break things.
    let likely_no_semantic_root = env::var_os("CARGO_TARGET_DIR").is_some();

    if likely_no_semantic_root
        || original.is_relative()
        || link.is_relative()
        || path_contains_intermediate_components(original)
        || path_contains_intermediate_components(link)
    {
        return original.to_path_buf();
    }

    let shared_root = shared_root(original, link);

    if shared_root == PathBuf::new() {
        return original.to_path_buf();
    }

    let relative_original = original.strip_prefix(&shared_root).expect("unreachable");
    let mut link = link
        .parent()
        .expect("we know that link is an absolute path, so at least one parent exists")
        .to_path_buf();

    let mut path_to_shared_root = PathBuf::new();
    while link != shared_root {
        path_to_shared_root.push("..");
        assert!(
            link.pop(),
            "we know there is a shared root of nonzero size, so this should never return 'no parent'"
        );
    }

    path_to_shared_root.join(relative_original)
}

fn path_contains_intermediate_components(path: impl AsRef<Path>) -> bool {
    path.as_ref().iter().any(|segment| segment == "..")
}

fn shared_root(left: &Path, right: &Path) -> PathBuf {
    let mut shared_root = PathBuf::new();
    let mut left = left.iter();
    let mut right = right.iter();
    loop {
        let left = left.next();
        let right = right.next();

        if left != right || left.is_none() {
            return shared_root;
        }
        shared_root.push(left.unwrap());
    }
}

#[cfg(test)]
mod tests {
    use crate::out::best_effort_relativize_symlink;

    #[cfg(not(windows))]
    #[test]
    fn test_relativize_symlink_unix() {
        assert_eq!(
            best_effort_relativize_symlink("/foo/bar/baz", "/foo/spam/eggs")
                .to_str()
                .unwrap(),
            "../bar/baz"
        );
        assert_eq!(
            best_effort_relativize_symlink("/foo/bar/../baz", "/foo/spam/eggs")
                .to_str()
                .unwrap(),
            "/foo/bar/../baz"
        );
        assert_eq!(
            best_effort_relativize_symlink("/foo/bar/baz", "/foo/spam/./eggs")
                .to_str()
                .unwrap(),
            "../bar/baz"
        );
    }

    #[cfg(windows)]
    #[test]
    fn test_relativize_symlink_windows() {
        use std::path::PathBuf;
        let windows_target: PathBuf = ["c:\\", "windows", "foo"].iter().collect();
        let windows_link: PathBuf = ["c:\\", "users", "link"].iter().collect();
        let windows_different_volume_link: PathBuf = ["d:\\", "users", "link"].iter().collect();

        assert_eq!(
            best_effort_relativize_symlink(windows_target.clone(), windows_link)
                .to_str()
                .unwrap(),
            "..\\windows\\foo"
        );
        assert_eq!(
            best_effort_relativize_symlink(windows_target.clone(), windows_different_volume_link),
            windows_target
        );
    }
}
