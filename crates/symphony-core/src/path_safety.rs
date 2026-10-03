//! Path helpers with Elixir semantics.
//!
//! - [`expand_path`] is Elixir's `Path.expand/1,2`: `~`/`~/` -> `$HOME`, made absolute against the CWD
//!   (or a base), `.`/`..` removed **lexically**. `~user` is not expanded.
//! - [`canonicalize`] is `PathSafety.canonicalize/1`: a symlink-resolving realpath that tolerates a
//!   missing tail. Unlike Elixir it stops after [`MAX_SYMLINK_HOPS`] symlinks with an `eloop` error
//!   instead of recursing forever.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use crate::error::IoReason;

/// Maximum number of symlinks followed by [`canonicalize`] before failing with `eloop`
/// (matches the usual OS `ELOOP` limit).
pub const MAX_SYMLINK_HOPS: usize = 40;

/// Path canonicalization failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    /// `{:path_canonicalize_failed, expanded_path, reason}`.
    #[error("path_canonicalize_failed: {}: {reason}", path.display())]
    CanonicalizeFailed {
        /// The lexically expanded input path.
        path: PathBuf,
        /// POSIX reason (`enametoolong`, `eacces`, `enotdir`, `eloop`, ...).
        reason: IoReason,
    },
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

fn current_dir() -> PathBuf {
    // The CWD can only be unavailable if it was deleted under us; "/" keeps the result absolute.
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
}

fn expand_home(path: &Path) -> PathBuf {
    let mut components = path.components();
    match components.next() {
        Some(Component::Normal(first)) if first == "~" => match home_dir() {
            Some(home) => home.join(components.as_path()),
            None => path.to_path_buf(),
        },
        _ => path.to_path_buf(),
    }
}

fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                // Popping at the root is a no-op, like Elixir's `Path.expand("/..") == "/"`.
                out.pop();
            }
            Component::Normal(segment) => out.push(segment),
        }
    }
    if out.as_os_str().is_empty() {
        out.push("/");
    }
    out
}

/// Elixir `Path.expand(path)` / `Path.expand(path, base)`.
///
/// `base` (when given) is itself expanded against the CWD first.
pub fn expand_path(path: impl AsRef<Path>, base: Option<&Path>) -> PathBuf {
    let path = expand_home(path.as_ref());
    let absolute = if path.has_root() {
        path
    } else {
        let base = match base {
            Some(base) => expand_path(base, None),
            None => current_dir(),
        };
        base.join(path)
    };
    normalize_lexically(&absolute)
}

fn segments(path: &Path) -> (PathBuf, VecDeque<OsString>) {
    let mut root = PathBuf::new();
    let mut segs = VecDeque::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => root.push(prefix.as_os_str()),
            Component::RootDir => root.push(Component::RootDir.as_os_str()),
            Component::Normal(segment) => segs.push_back(segment.to_os_string()),
            // `expand_path` already removed `.` and `..`.
            Component::CurDir | Component::ParentDir => {}
        }
    }
    if root.as_os_str().is_empty() {
        root.push("/");
    }
    (root, segs)
}

fn join_all(root: &Path, segs: impl IntoIterator<Item = impl AsRef<Path>>) -> PathBuf {
    let mut out = root.to_path_buf();
    for seg in segs {
        out.push(seg);
    }
    out
}

/// `PathSafety.canonicalize/1`: expand, then resolve symlinks segment by segment.
///
/// - an existing symlink is replaced by its (expanded) target and the walk restarts from the target root;
/// - a missing segment (`ENOENT`) returns the path with the rest appended unresolved;
/// - any other `lstat`/`readlink` error returns [`PathError::CanonicalizeFailed`] with the expanded input;
/// - more than [`MAX_SYMLINK_HOPS`] symlinks fail with reason `eloop`.
pub fn canonicalize(path: impl AsRef<Path>) -> Result<PathBuf, PathError> {
    let expanded = expand_path(path.as_ref(), None);
    let fail = |reason: IoReason| PathError::CanonicalizeFailed {
        path: expanded.clone(),
        reason,
    };

    let (mut root, mut pending) = segments(&expanded);
    let mut resolved: Vec<OsString> = Vec::new();
    let mut hops = 0usize;

    while let Some(segment) = pending.pop_front() {
        let parent = join_all(&root, &resolved);
        let candidate = parent.join(&segment);
        match fs::symlink_metadata(&candidate) {
            Ok(meta) if meta.file_type().is_symlink() => {
                hops += 1;
                if hops > MAX_SYMLINK_HOPS {
                    return Err(fail(IoReason::named(io::ErrorKind::Other, "eloop")));
                }
                let target = fs::read_link(&candidate).map_err(|e| fail(IoReason::from_io(&e)))?;
                let resolved_target = expand_path(&target, Some(&parent));
                let (target_root, mut target_segments) = segments(&resolved_target);
                target_segments.extend(pending.drain(..));
                root = target_root;
                pending = target_segments;
                resolved.clear();
            }
            Ok(_) => resolved.push(segment),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                let mut out = join_all(&root, &resolved);
                out.push(&segment);
                for rest in pending {
                    out.push(rest);
                }
                return Ok(out);
            }
            Err(err) => return Err(fail(IoReason::from_io(&err))),
        }
    }

    Ok(join_all(&root, &resolved))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_path_is_lexical() {
        assert_eq!(expand_path("/a/b/../c/./d", None), PathBuf::from("/a/c/d"));
        assert_eq!(expand_path("/..", None), PathBuf::from("/"));
        assert_eq!(
            expand_path("a/../../..", Some(Path::new("/x"))),
            PathBuf::from("/")
        );
        assert_eq!(
            expand_path("", Some(Path::new("/x/y"))),
            PathBuf::from("/x/y")
        );
        assert_eq!(
            expand_path("rel", Some(Path::new("/base/dir/"))),
            PathBuf::from("/base/dir/rel")
        );
        assert_eq!(
            expand_path("/abs", Some(Path::new("/ignored"))),
            PathBuf::from("/abs")
        );
        assert_eq!(expand_path("/trailing/", None), PathBuf::from("/trailing"));
    }

    #[test]
    fn expand_path_expands_home_but_not_tilde_user() {
        if let Some(home) = home_dir() {
            assert_eq!(expand_path("~", None), normalize_lexically(&home));
            assert_eq!(
                expand_path("~/x", None),
                normalize_lexically(&home.join("x"))
            );
        }
        assert_eq!(
            expand_path("~user/x", Some(Path::new("/b"))),
            PathBuf::from("/b/~user/x")
        );
    }

    #[test]
    fn expand_path_relative_uses_cwd() {
        assert_eq!(expand_path("x", None), current_dir().join("x"));
    }
}
