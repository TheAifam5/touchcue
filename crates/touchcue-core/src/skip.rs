//! Process names passed over when looking for the requester of a request.
//!
//! The requester is the program started inside the application that led to
//! the request: walking from the application's process down to the client,
//! the first process that is not skipped. Shells, terminal multiplexers,
//! command wrappers and service managers are skipped by default.

/// Process names skipped unless `requester.skip` replaces them.
///
/// Names are compared with `comm`, which the kernel cuts to 15 bytes. An
/// entry ending in `*` matches every name starting with the text before it;
/// `tmux*` covers the server, whose `comm` is `tmux: server`.
pub const DEFAULT_SKIP: &[&str] = &[
    // Shells.
    "sh", "bash", "dash", "zsh", "fish", "nu", "ksh", "mksh", "tcsh", "csh", "elvish", "xonsh",
    // Terminal multiplexers.
    "tmux*", "screen", "zellij", "herdr", "abduco", "dtach", // Command wrappers.
    "timeout", "nice", "nohup", "setsid", "stdbuf", "time", "xargs", "flock", "ionice", "chrt",
    "taskset", "env", "sudo", "doas", "su", "run0", // Service managers.
    "systemd", "init",
];

/// The effective set of skipped process names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkipList {
    patterns: Vec<String>,
}

impl Default for SkipList {
    /// Returns the names in [`DEFAULT_SKIP`].
    fn default() -> Self {
        Self::new(None, &[])
    }
}

impl SkipList {
    /// Returns `skip`, or [`DEFAULT_SKIP`] when it is `None`, followed by
    /// `extend`. Entries use the pattern form of [`DEFAULT_SKIP`].
    #[must_use]
    pub fn new(skip: Option<&[String]>, extend: &[String]) -> Self {
        let base: Vec<String> = match skip {
            Some(skip) => skip.to_vec(),
            None => DEFAULT_SKIP.iter().map(|&name| name.to_owned()).collect(),
        };
        Self {
            patterns: base.into_iter().chain(extend.iter().cloned()).collect(),
        }
    }

    /// Returns whether a process named `name` is skipped; matching is exact
    /// and case-sensitive.
    #[must_use]
    pub fn skips(&self, name: &str) -> bool {
        self.patterns.iter().any(|pattern| matches(pattern, name))
    }

    /// Returns the entries in order.
    #[must_use]
    pub fn patterns(&self) -> &[String] {
        &self.patterns
    }
}

/// Returns whether `pattern` is a valid entry: a non-empty name, or a
/// non-empty prefix followed by one `*`.
#[must_use]
pub fn is_pattern(pattern: &str) -> bool {
    let name = pattern.strip_suffix('*').unwrap_or(pattern);
    !name.is_empty() && !name.contains('*')
}

fn matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => name == pattern,
    }
}

/// Returns the index of the requester among processes ordered client first,
/// given whether each is skipped: the topmost process that is not skipped.
///
/// `skipped` covers the processes strictly below the application's process,
/// or the whole walk when there is no application. Returns `None` when every
/// process is skipped.
#[must_use]
pub fn requester(skipped: &[bool]) -> Option<usize> {
    skipped.iter().rposition(|&skipped| !skipped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_error::TestResult;

    fn owned(names: &[&str]) -> Vec<String> {
        names.iter().map(|&name| name.to_owned()).collect()
    }

    #[test]
    fn defaults_skip_shells_multiplexers_wrappers_and_managers() {
        let list = SkipList::default();
        for name in [
            "bash",
            "nu",
            "tmux: server",
            "herdr",
            "sudo",
            "env",
            "systemd",
        ] {
            assert!(list.skips(name), "{name}");
        }
        for name in [
            "claude",
            "git",
            "gpg",
            "ssh",
            "release.sh",
            "Bash",
            "bash-5.2",
        ] {
            assert!(!list.skips(name), "{name}");
        }
    }

    #[test]
    fn skip_replaces_and_extend_skip_appends() {
        let extend = owned(&["nvim", "just-*"]);
        let list = SkipList::new(None, &extend);
        assert!(list.skips("nvim") && list.skips("just-run") && list.skips("bash"));
        assert!(!list.skips("just"));
        let replaced = SkipList::new(Some(&owned(&["make"])), &extend);
        assert!(replaced.skips("make") && replaced.skips("nvim"));
        assert!(!replaced.skips("bash"));
        let none = SkipList::new(Some(&[]), &[]);
        assert!(!none.skips("bash"));
        assert_eq!(none.patterns(), &[] as &[String]);
    }

    #[test]
    fn patterns_are_validated() {
        for valid in ["mise", "git-*", "a*"] {
            assert!(is_pattern(valid), "{valid}");
        }
        for invalid in ["", "*", "a*b", "a**", "*a"] {
            assert!(!is_pattern(invalid), "{invalid}");
        }
        assert!(DEFAULT_SKIP.iter().all(|entry| is_pattern(entry)));
    }

    #[test]
    fn configuration_guide_lists_the_defaults() -> TestResult {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/guide/configuration.md");
        let guide = std::fs::read_to_string(path)?;
        let (_, list) = guide
            .split_once("[requester]\nskip = [")
            .ok_or("no skip list in the guide")?;
        let (list, _) = list.split_once(']').ok_or("unclosed skip list")?;
        let listed: Vec<&str> = list
            .split(',')
            .map(|entry| entry.trim().trim_matches('"'))
            .filter(|entry| !entry.is_empty())
            .collect();
        assert_eq!(listed, DEFAULT_SKIP);
        Ok(())
    }

    #[test]
    fn requester_is_the_topmost_process_not_skipped() {
        // gpg ← git ← bash ← claude ← nu ← herdr ← herdr ← nu
        let walk = [false, false, true, false, true, true, true, true];
        assert_eq!(requester(&walk), Some(3));
        // gpg ← git ← zsh
        assert_eq!(requester(&[false, false, true]), Some(1));
        assert_eq!(requester(&[true, true]), None);
        assert_eq!(requester(&[]), None);
    }
}
