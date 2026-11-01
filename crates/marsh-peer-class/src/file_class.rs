//! File classification shared with the kernel program: the names and places that make an inode's
//! content carry a class (`docs/threat-model.md` §8.3). `crates/marsh-bpf/src/marsh.h` mirrors it.

/// A taint receipt's sender when the reader read a classified file, not another root's data.
pub const CLASSIFIED: u64 = u64::MAX;
/// Name rules the kernel holds.
pub const MAX_NAME_RULES: usize = 32;
/// Bytes of a name rule's prefix, and of its suffix.
pub const NAME_GLOB: usize = 32;

/// What an inode's name and place classify it as.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InodeLabel {
    /// Taint classes (bit IDs) a read of the inode carries.
    pub classes: u64,
    /// A directory's: classes every entry beneath it carries.
    pub subtree: u64,
    /// A directory's: scopes its subdirectories inherit.
    pub deep: u64,
    /// A directory's: scopes of its own entries only.
    pub direct: u64,
}

impl InodeLabel {
    /// Whether the label classifies nothing and scopes nothing.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.classes | self.subtree | self.deep | self.direct == 0
    }

    /// Adds everything `other` holds.
    pub const fn merge(&mut self, other: Self) {
        self.classes |= other.classes;
        self.subtree |= other.subtree;
        self.deep |= other.deep;
        self.direct |= other.direct;
    }

    /// What the entry `name` of this directory carries, exactly as the kernel computes it when
    /// the entry is created, linked or renamed there.
    #[must_use]
    pub fn child(&self, rules: &[NameRule], name: &[u8], directory: bool) -> Self {
        let mut child = Self {
            classes: self.subtree,
            subtree: if directory { self.subtree } else { 0 },
            deep: if directory { self.deep } else { 0 },
            direct: 0,
        };
        let scopes = self.deep | self.direct;
        for rule in rules {
            if rule.scope & scopes != 0 && rule.matches(name) {
                child.classes |= rule.classes;
                if directory {
                    child.subtree |= rule.subtree;
                    child.direct |= rule.direct;
                }
            }
        }
        child
    }
}

/// Classifies an entry whose parent directory holds one of `scope` and whose name is `prefix`
/// exactly, or, with `star`, `prefix`, anything, then `suffix`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NameRule {
    /// Parent scopes (deep or direct) the rule applies under; zero disables it.
    pub scope: u64,
    /// Classes a matched entry carries.
    pub classes: u64,
    /// A matched directory's subtree classes.
    pub subtree: u64,
    /// A matched directory's direct scopes.
    pub direct: u64,
    /// Significant prefix bytes.
    pub prefix_len: u8,
    /// Significant suffix bytes.
    pub suffix_len: u8,
    /// Nonzero when the name is `prefix*suffix` rather than exactly `prefix`.
    pub star: u8,
    /// Padding; zero.
    pub unused: [u8; 5],
    /// The literal prefix.
    pub prefix: [u8; NAME_GLOB],
    /// The literal suffix.
    pub suffix: [u8; NAME_GLOB],
}

impl NameRule {
    /// A rule for names matching `glob` under `scope`: a literal, or one `*` between a literal
    /// prefix and suffix of at most [`NAME_GLOB`] bytes each. `None` for any other glob, which the
    /// kernel cannot evaluate.
    #[must_use]
    pub fn new(glob: &str, scope: u64) -> Option<Self> {
        if glob.is_empty() || glob.contains(['?', '[', ']', '{', '}', '/', '\\']) {
            return None;
        }
        let (prefix, suffix, star) = match glob.split_once('*') {
            Some((_, rest)) if rest.contains('*') => return None,
            Some((prefix, suffix)) => (prefix, suffix, 1),
            None => (glob, "", 0),
        };
        if prefix.len() > NAME_GLOB || suffix.len() > NAME_GLOB {
            return None;
        }
        let mut rule = Self {
            scope,
            prefix_len: u8::try_from(prefix.len()).ok()?,
            suffix_len: u8::try_from(suffix.len()).ok()?,
            star,
            ..Self::default()
        };
        rule.prefix[..prefix.len()].copy_from_slice(prefix.as_bytes());
        rule.suffix[..suffix.len()].copy_from_slice(suffix.as_bytes());
        Some(rule)
    }

    /// Whether the whole name `name` matches.
    #[must_use]
    pub fn matches(&self, name: &[u8]) -> bool {
        let prefix = &self.prefix[..usize::from(self.prefix_len).min(NAME_GLOB)];
        let suffix = &self.suffix[..usize::from(self.suffix_len).min(NAME_GLOB)];
        if self.star == 0 {
            return name == prefix;
        }
        name.len() >= prefix.len() + suffix.len() && name.starts_with(prefix) && name.ends_with(suffix)
    }

    /// Adds `rule` to `rules`, merged into the one with the same scope and name if any, so a
    /// table never holds the same match twice.
    pub fn join(rules: &mut Vec<Self>, rule: Self) {
        let bare = |rule: &Self| Self {
            classes: 0,
            subtree: 0,
            direct: 0,
            ..*rule
        };
        match rules.iter_mut().find(|known| bare(known) == bare(&rule)) {
            Some(known) => {
                known.classes |= rule.classes;
                known.subtree |= rule.subtree;
                known.direct |= rule.direct;
            }
            None => rules.push(rule),
        }
    }
}

// Mirrored by `_Static_assert`s in marsh.h.
const _: () = assert!(size_of::<InodeLabel>() == 32);
const _: () = assert!(size_of::<NameRule>() == 104);
