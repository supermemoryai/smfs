//! Path-string helpers.
//!
//! supermemoryfs operates on inode numbers internally, but the user-facing
//! surface (CLI arguments, test fixtures, the eventual `SupermemoryFs`
//! path-to-document mapping) still needs to take slash-separated string paths
//! and turn them into component sequences. These helpers do that cleanly and
//! reject paths that would escape the filesystem root.

use super::error::{VfsError, VfsResult};
use super::mode::MAX_NAME_LEN;

/// Normalise a slash-separated path into a sequence of clean components.
///
/// Semantics:
///
/// - Leading, trailing, and repeated `/` are collapsed.
/// - `.` components are removed.
/// - `..` pops the last component; popping past the root is rejected with
///   [`VfsError::InvalidPath`].
/// - Components containing a NUL byte are rejected.
/// - Components longer than [`MAX_NAME_LEN`] are rejected with
///   [`VfsError::NameTooLong`].
/// - Empty input and `"/"` both normalise to an empty vector (the root).
pub fn normalize(path: &str) -> VfsResult<Vec<String>> {
    let mut result: Vec<String> = Vec::new();
    for component in path.split('/') {
        if component.is_empty() || component == "." {
            continue;
        }
        if component == ".." {
            if result.is_empty() {
                return Err(VfsError::InvalidPath(format!("path escapes root: {path}")));
            }
            result.pop();
            continue;
        }
        if component.contains('\0') {
            return Err(VfsError::InvalidPath(format!(
                "NUL byte in component: {component}"
            )));
        }
        if component.len() > MAX_NAME_LEN {
            return Err(VfsError::NameTooLong(component.len()));
        }
        result.push(component.to_string());
    }
    Ok(result)
}

/// Split a path into `(parent components, final name)`.
///
/// Returns [`VfsError::RootOperation`] for `"/"`, the empty string, or any
/// path that normalises to the root (e.g. `"/foo/.."`). The caller typically
/// uses this to resolve the parent directory and then apply an operation to
/// the final name inside it.
pub fn parent_and_name(path: &str) -> VfsResult<(Vec<String>, String)> {
    let mut components = normalize(path)?;
    let name = components.pop().ok_or(VfsError::RootOperation)?;
    Ok((components, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_root() {
        assert_eq!(normalize("/").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn test_normalize_empty() {
        assert_eq!(normalize("").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn test_normalize_simple() {
        assert_eq!(normalize("/foo/bar").unwrap(), vec!["foo", "bar"]);
    }

    #[test]
    fn test_normalize_double_slash() {
        assert_eq!(normalize("/foo//bar").unwrap(), vec!["foo", "bar"]);
    }

    #[test]
    fn test_normalize_trailing_slash() {
        assert_eq!(normalize("/foo/bar/").unwrap(), vec!["foo", "bar"]);
    }

    #[test]
    fn test_normalize_dot() {
        assert_eq!(normalize("/foo/./bar").unwrap(), vec!["foo", "bar"]);
    }

    #[test]
    fn test_normalize_dotdot() {
        assert_eq!(normalize("/foo/../bar").unwrap(), vec!["bar"]);
    }

    #[test]
    fn test_normalize_nested_dotdot() {
        assert_eq!(normalize("/a/b/c/../../d").unwrap(), vec!["a", "d"]);
    }

    #[test]
    fn test_normalize_escape_rejected() {
        assert!(matches!(
            normalize("/../foo"),
            Err(VfsError::InvalidPath(_))
        ));
    }

    #[test]
    fn test_normalize_deep_escape_rejected() {
        assert!(matches!(
            normalize("/foo/../../bar"),
            Err(VfsError::InvalidPath(_))
        ));
    }

    #[test]
    fn test_normalize_nul_rejected() {
        assert!(matches!(
            normalize("/foo\0bar"),
            Err(VfsError::InvalidPath(_))
        ));
    }

    #[test]
    fn test_normalize_name_too_long_rejected() {
        let long = "a".repeat(MAX_NAME_LEN + 1);
        let path = format!("/{long}");
        assert!(matches!(normalize(&path), Err(VfsError::NameTooLong(_))));
    }

    #[test]
    fn test_normalize_max_name_accepted() {
        let name = "a".repeat(MAX_NAME_LEN);
        let path = format!("/{name}");
        let result = normalize(&path).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].len(), MAX_NAME_LEN);
    }

    #[test]
    fn test_parent_and_name_simple() {
        let (parent, name) = parent_and_name("/foo/bar.txt").unwrap();
        assert_eq!(parent, vec!["foo"]);
        assert_eq!(name, "bar.txt");
    }

    #[test]
    fn test_parent_and_name_shallow() {
        let (parent, name) = parent_and_name("/file").unwrap();
        assert_eq!(parent, Vec::<String>::new());
        assert_eq!(name, "file");
    }

    #[test]
    fn test_parent_and_name_root_rejected() {
        assert!(matches!(parent_and_name("/"), Err(VfsError::RootOperation)));
    }

    #[test]
    fn test_parent_and_name_empty_rejected() {
        assert!(matches!(parent_and_name(""), Err(VfsError::RootOperation)));
    }
}
