//! `--metadata` selectors: `Type`, `Type:Name`, `Type:Prefix*`.

use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::registry;
use crate::source::{Output, Resolved};
use crate::{Error, Result};

/// Components of one type, all of them or those whose full name matches `name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selector {
    pub kind: String,
    /// `*` matches any run of characters; `None` is the whole type.
    pub name: Option<String>,
}

impl FromStr for Selector {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let (kind, name) = match s.split_once(':') {
            Some((k, n)) => (k.trim(), Some(n.trim())),
            None => (s.trim(), None),
        };
        if kind.is_empty() || name.is_some_and(str::is_empty) {
            return Err(Error::Selector(s.to_owned()));
        }
        if registry::get().by_name(kind).is_none() {
            return Err(Error::UnknownType(kind.to_owned()));
        }
        Ok(Self {
            kind: kind.to_owned(),
            name: name.filter(|n| *n != "*").map(str::to_owned),
        })
    }
}

impl fmt::Display for Selector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.name {
            Some(n) => write!(f, "{}:{n}", self.kind),
            None => f.write_str(&self.kind),
        }
    }
}

impl Selector {
    pub fn matches(&self, kind: &str, full_name: &str) -> bool {
        kind == self.kind && self.name.as_deref().is_none_or(|n| glob(n, full_name))
    }
}

/// `*` matches any run of characters, everything else matches itself.
fn glob(pattern: &str, text: &str) -> bool {
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or_default();
    let Some(mut rest) = text.strip_prefix(first) else {
        return false;
    };
    let mut parts: Vec<&str> = parts.collect();
    let Some(last) = parts.pop() else {
        return rest.is_empty();
    };
    for p in parts {
        match rest.find(p) {
            Some(i) => rest = &rest[i + p.len()..],
            None => return false,
        }
    }
    rest.len() >= last.len() && rest.ends_with(last)
}

impl Resolved {
    /// Keeps what has source files under `paths` or matches `selectors`; a component kept by
    /// either is deployed once. A decomposed component selected through some of its children
    /// keeps its parent file and only those children, as SDR does when deploying a single
    /// field. Every selector must match something.
    pub fn retain(&mut self, paths: &[PathBuf], selectors: &[Selector]) -> Result<()> {
        let mut picked: HashSet<PathBuf> = HashSet::new();
        for s in selectors {
            let mut found = false;
            for c in &self.components {
                if s.matches(&c.kind, &c.full_name) {
                    found = true;
                    for (_, o) in &c.files {
                        picked.extend(o.sources().into_iter().map(Path::to_owned));
                    }
                }
                for ch in c
                    .children
                    .iter()
                    .filter(|m| s.matches(&m.kind, &m.full_name))
                {
                    found = true;
                    match &ch.path {
                        Some(p) => {
                            picked.insert(p.clone());
                        }
                        None => {
                            return Err(Error::ChildWithoutFile {
                                member: format!("{}:{}", ch.kind, ch.full_name),
                                parent: c.kind.clone(),
                            });
                        }
                    }
                }
            }
            if !found {
                return Err(Error::NoMatch(s.to_string()));
            }
        }
        self.retain_where(|p| picked.contains(p) || paths.iter().any(|w| p.starts_with(w)));
        Ok(())
    }

    fn retain_where(&mut self, keep_file: impl Fn(&Path) -> bool) {
        self.components.retain_mut(|c| {
            let mut keep = false;
            for (_, o) in &mut c.files {
                match o {
                    Output::Recompose(r) if !r.parent.as_deref().is_some_and(&keep_file) => {
                        r.children.retain(|ch| keep_file(&ch.path));
                        if !r.children.is_empty() {
                            keep = true;
                            c.listed = false;
                            c.children
                                .retain(|m| m.path.as_deref().is_some_and(&keep_file));
                        }
                    }
                    o => keep |= o.sources().iter().any(|p| keep_file(p)),
                }
            }
            keep
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_matches() {
        let s: Selector = "ApexClass:Foo*Test".parse().unwrap();
        assert!(s.matches("ApexClass", "FooBarTest"));
        assert!(s.matches("ApexClass", "FooTest"));
        assert!(!s.matches("ApexClass", "FooBar"));
        assert!(!s.matches("ApexTrigger", "FooTest"));

        let all: Selector = "ApexClass:*".parse().unwrap();
        assert_eq!(all.name, None);
        assert!(all.matches("ApexClass", "X"));
        assert_eq!("ApexClass".parse::<Selector>().unwrap(), all);

        let exact: Selector = "Layout:Account-Account Layout".parse().unwrap();
        assert!(exact.matches("Layout", "Account-Account Layout"));
        assert!(!exact.matches("Layout", "Account-Account Layout 2"));
    }

    #[test]
    fn rejects_bad_selectors() {
        assert!(matches!(
            "Nope:X".parse::<Selector>(),
            Err(Error::UnknownType(_))
        ));
        assert!(matches!(
            "ApexClass:".parse::<Selector>(),
            Err(Error::Selector(_))
        ));
        assert!(matches!(":X".parse::<Selector>(), Err(Error::Selector(_))));
    }

    #[test]
    fn glob_edges() {
        assert!(glob("a*b*c", "aXbYc"));
        assert!(glob("a*a", "aa"));
        assert!(!glob("a*a", "a"));
        assert!(glob("*", ""));
        assert!(!glob("abc", "abcd"));
    }
}
