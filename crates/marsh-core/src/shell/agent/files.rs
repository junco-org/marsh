//! A policy's propagated path classes, compiled for the kernel's file classification
//! (`docs/threat-model.md` §8.3): name rules over directory scopes, and the directories that
//! start with those scopes.
//!
//! A pattern compiles when every component but a leading `**` is a name with at most one `*`
//! (at most [`NAME_GLOB`](marsh_peer_class::NAME_GLOB) bytes on either side), and a `**` is
//! otherwise only its last component. The kernel adds every matching class rather than taking
//! the first matching pattern, so it may classify a file whose first match is another class:
//! that over-taints and never under-taints.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use marsh_instrument::{FileClasses, InodeLabel, NameRule};
use marsh_lib::RecoverPoison as _;
use marsh_peer_class::MAX_NAME_RULES;

use super::AgentPolicy;
use crate::shell::answers::taint_id;

/// Scope bits by the place each stands for, shared by every policy and view, so roots of the
/// same policy install the same rules.
static SCOPES: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn scope(place: &str) -> io::Result<u64> {
    let mut scopes = SCOPES.lock().recover();
    let id = match scopes.iter().position(|known| known == place) {
        Some(id) => id,
        None if scopes.len() == 64 => {
            return Err(io::Error::other(
                "privileged file classification holds at most 64 directory scopes",
            ));
        }
        None => {
            scopes.push(place.to_owned());
            scopes.len() - 1
        }
    };
    Ok(1 << id)
}

impl AgentPolicy {
    /// What the kernel classifies files by for a view at `view`, whose paths the policy's
    /// relative patterns label. Outside patterns are anchored at their deepest existing
    /// directory.
    ///
    /// # Errors
    /// When a propagated class's pattern does not compile, or the rules or scopes exceed the
    /// kernel's tables.
    pub(in crate::shell) fn kernel_files(&self, view: &Path) -> io::Result<FileClasses> {
        let mut files = Compiler::default();
        let propagated = |label: &str| self.propagated.contains(label);
        for (pattern, label) in self.inside.iter().filter(|(_, label)| propagated(label)) {
            let class = 1 << taint_id(label)?;
            let components: Vec<&str> = pattern.as_str().split('/').collect();
            files
                .pattern("source", view, &components, class)
                .map_err(|error| refused(pattern.as_str(), label, &error))?;
        }
        for (pattern, label, _) in self.outside.iter().filter(|(_, label, _)| propagated(label)) {
            let class = 1 << taint_id(label)?;
            let text = pattern.as_str();
            let components: Vec<&str> = text.split('/').filter(|part| !part.is_empty()).collect();
            let (anchor, depth) = outside_anchor(&components);
            files
                .pattern(
                    &anchor.display().to_string(),
                    &anchor,
                    &components[depth..],
                    class,
                )
                .map_err(|error| refused(text, label, &error))?;
        }
        if files.0.rules.len() > MAX_NAME_RULES {
            return Err(io::Error::other(format!(
                "propagated class patterns need {} kernel name rules; at most {MAX_NAME_RULES} fit",
                files.0.rules.len()
            )));
        }
        Ok(files.0)
    }
}

fn refused(pattern: &str, label: &str, error: &io::Error) -> io::Error {
    io::Error::other(format!(
        "class pattern {pattern:?} of propagated class {label} cannot be classified in the kernel: {error}"
    ))
}

/// The deepest existing directory along an absolute pattern's literal components, short of
/// the last one any rule must name, and how many components it consumes: a rule, not a label
/// on one inode, then classifies the named entry even if it is replaced.
fn outside_anchor(components: &[&str]) -> (PathBuf, usize) {
    let literal = components
        .iter()
        .position(|component| component.contains(['*', '?', '[']))
        .unwrap_or(components.len());
    let mut depth = literal.saturating_sub(1);
    loop {
        let path: PathBuf = std::iter::once("/").chain(components[..depth].iter().copied()).collect();
        if depth == 0 {
            return (path, 0);
        }
        // Rules match the names the kernel loads, so anchor where symlinks lead.
        if let Ok(real) = path.canonicalize()
            && real.is_dir()
        {
            return (real, depth);
        }
        depth -= 1;
    }
}

#[derive(Default)]
struct Compiler(FileClasses);

impl Compiler {
    fn anchor(&mut self, path: &Path) -> &mut InodeLabel {
        let anchors = &mut self.0.anchors;
        let index = if let Some(index) = anchors.iter().position(|(known, _)| known == path) {
            index
        } else {
            anchors.push((path.to_owned(), InodeLabel::default()));
            anchors.len() - 1
        };
        &mut anchors[index].1
    }

    /// Classifies what `components` name beneath `anchor`, the directory `place` stands for.
    fn pattern(
        &mut self,
        place: &str,
        anchor: &Path,
        components: &[&str],
        class: u64,
    ) -> io::Result<()> {
        match components {
            [] => Err(io::Error::other("it names no entry beneath its anchor")),
            ["**"] => {
                self.anchor(anchor).subtree |= class;
                Ok(())
            }
            ["**", rest @ ..] => {
                let place = format!("{place}/**");
                let deep = scope(&place)?;
                self.anchor(anchor).deep |= deep;
                self.under(&place, deep, rest, class)
            }
            _ => {
                let direct = scope(place)?;
                self.anchor(anchor).direct |= direct;
                self.under(place, direct, components, class)
            }
        }
    }

    /// Rules for `components` beneath directories holding `scope`, which stands for `place`.
    fn under(&mut self, place: &str, scope_bit: u64, components: &[&str], class: u64) -> io::Result<()> {
        let [name, rest @ ..] = components else {
            return Err(io::Error::other("it names no entry beneath its anchor"));
        };
        let mut rule = NameRule::new(name, scope_bit).ok_or_else(|| {
            io::Error::other(format!(
                "{name:?} is not a name with at most one `*` and short enough literal parts"
            ))
        })?;
        match rest {
            [] => rule.classes = class,
            ["**"] => rule.subtree = class,
            ["**", ..] => {
                return Err(io::Error::other(
                    "a `**` may only lead the pattern or end it",
                ));
            }
            _ => {
                let place = format!("{place}/{name}");
                let next = scope(&place)?;
                rule.direct = next;
                self.under(&place, next, rest, class)?;
            }
        }
        NameRule::join(&mut self.0.rules, rule);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::shell::agent::AgentRule;

    fn classes(files: &FileClasses, path: &[(&str, bool)]) -> u64 {
        let (_, mut label) = files.anchors[0];
        for (name, directory) in path {
            label = label.child(&files.rules, name.as_bytes(), *directory);
        }
        label.classes
    }

    /// The kernel's labels agree with the policy's classes on the paths below, and add only
    /// classes of later-matching patterns.
    #[test]
    fn compiled_rules_classify_like_the_policy() {
        let policy = AgentPolicy::new()
            .class("**/.env", "secret")
            .unwrap()
            .class("**/*.pem", "secret")
            .unwrap()
            .class("keys/**", "secret")
            .unwrap()
            .class("config/prod/*.json", "secret")
            .unwrap()
            .class("**/__pycache__/**", "cache")
            .unwrap()
            .class("**", "src")
            .unwrap()
            .rule(AgentRule::no_egress_after_read("secret"));
        let view = Path::new("/view");
        let files = policy.kernel_files(view).unwrap();
        assert_eq!(files.anchors.len(), 1);
        let secret = 1 << taint_id("secret").unwrap();
        for path in [
            ".env",
            "a/b/.env",
            "cert.pem",
            "deep/x/y.pem",
            "keys/id",
            "keys/sub/id",
            "config/prod/db.json",
            "README.md",
            "a/.envrc",
            "keys",
            "config/prod/sub/db.json",
            "config/dev/db.json",
            "__pycache__/m.pyc",
        ] {
            let parts: Vec<&str> = path.split('/').collect();
            let components: Vec<(&str, bool)> = parts
                .iter()
                .enumerate()
                .map(|(index, part)| (*part, index + 1 < parts.len()))
                .collect();
            let expected = policy.inside_class(Path::new(path)) == "secret";
            assert_eq!(classes(&files, &components) & secret != 0, expected, "{path}");
        }
    }

    /// The shipped policy must fit the kernel even where none of its home directories exist, so
    /// every outside pattern is anchored at `/` and spends a rule per component.
    #[test]
    fn the_recommended_policy_fits_the_kernel_tables() {
        let policy = AgentPolicy::recommended()
            .unwrap()
            .for_home(Path::new("/nonexistent/operator"))
            .unwrap();
        let files = policy.kernel_files(Path::new("/view")).unwrap();
        assert!(files.rules.len() <= MAX_NAME_RULES, "{}", files.rules.len());
        let secret = 1 << taint_id("secret").unwrap();
        let root = files.anchors.iter().find(|(path, _)| path == Path::new("/")).unwrap().1;
        let ssh = [("nonexistent", true), ("operator", true), (".ssh", true), ("id_rsa", false)]
            .iter()
            .fold(root, |label, (name, directory)| {
                label.child(&files.rules, name.as_bytes(), *directory)
            });
        assert_ne!(ssh.classes & secret, 0);
    }

    #[test]
    fn uncompilable_propagated_patterns_are_refused() {
        for pattern in ["src/**/.env", "**/*.env.*", "a/[bc]/.env"] {
            let policy = AgentPolicy::new()
                .class(pattern, "secret")
                .unwrap()
                .rule(AgentRule::no_egress_after_read("secret"));
            assert!(policy.kernel_files(Path::new("/view")).is_err(), "{pattern}");
        }
        // Only propagated classes reach the kernel.
        let policy = AgentPolicy::new().class("src/**/x", "tests").unwrap();
        assert!(policy.kernel_files(Path::new("/view")).unwrap().rules.is_empty());
    }
}
