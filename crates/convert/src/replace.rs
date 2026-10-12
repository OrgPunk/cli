//! `replacements` from sfdx-project.json, applied to source files on their way to mdapi the
//! way SDR applies them (`convert/replacements.ts`):
//!
//! - an entry targets files by `filename` (a suffix of the absolute path) or `glob`
//!   (`**/<glob>`), and finds a literal `stringToReplace` or a `regexToReplace`, everywhere;
//! - entries whose `replaceWhenEnv` conditions do not all hold are dropped up front;
//! - the value comes from `replaceWithEnv` (an unset variable is an error unless
//!   `allowUnsetEnvVariable`) or `replaceWithFile` (trimmed, must not be empty), and is only
//!   needed once a file matches: an unset variable for a file nobody deploys is no error;
//! - the replacement text expands `$&`, `$1`, `$$`… like JavaScript's `String.replace`;
//! - binary files are left alone;
//! - a `filename` entry that finds nothing is a warning.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Mutex;

use globset::{GlobBuilder, GlobMatcher};
use orgpunk_config::sfdx_project::Replacement;
use regex::{Captures, Regex};

use crate::{Error, Result};

#[derive(Debug, Default)]
pub struct Replacements {
    rules: Vec<Rule>,
    warnings: Mutex<BTreeSet<String>>,
}

#[derive(Debug)]
struct Rule {
    target: Target,
    find: Regex,
    /// The replacement text, or why there is none.
    with: std::result::Result<String, String>,
    /// As SDR names it in warnings: the regex source.
    label: String,
}

#[derive(Debug)]
enum Target {
    File(String),
    Glob(GlobMatcher),
}

/// What SDR treats as text without looking (`isTextFile`); anything else is checked.
const TEXT_EXTENSIONS: &[&str] = &[
    "cls", "xml", "json", "js", "css", "html", "htm", "txt", "md",
];

impl Replacements {
    /// The entries of a project rooted at `root`; `env` reads environment variables.
    pub fn new(
        root: &Path,
        entries: &[Replacement],
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self> {
        let mut rules = Vec::new();
        for (i, r) in entries.iter().enumerate() {
            let invalid = |reason: String| Error::Replacement { index: i, reason };
            let applies = r
                .replace_when_env
                .iter()
                .all(|c| env(&c.env).as_deref() == Some(c.value_text().as_str()));
            if !applies {
                continue;
            }
            let target = match (&r.filename, &r.glob) {
                (Some(f), _) => Target::File(f.replace('\\', "/")),
                (None, Some(g)) => Target::Glob(
                    GlobBuilder::new(&format!("**/{g}"))
                        .literal_separator(true)
                        .build()
                        .map_err(|e| invalid(e.to_string()))?
                        .compile_matcher(),
                ),
                (None, None) => return Err(invalid("needs filename or glob".into())),
            };
            let (find, label) = match (&r.string_to_replace, &r.regex_to_replace) {
                (Some(s), _) => {
                    let re = Regex::new(&regex::escape(s)).expect("escaped literal");
                    let label = format!("/{}/g", js_escape(s));
                    (re, label)
                }
                (None, Some(p)) => (
                    Regex::new(p).map_err(|e| invalid(format!("regexToReplace: {e}")))?,
                    format!("/{p}/g"),
                ),
                (None, None) => {
                    return Err(invalid("needs stringToReplace or regexToReplace".into()));
                }
            };
            let with = match (&r.replace_with_env, &r.replace_with_file) {
                (Some(name), _) => match env(name) {
                    Some(v) => Ok(v),
                    None if r.allow_unset_env_variable => Ok(String::new()),
                    None => Err(format!(
                        "\"{name}\" is in sfdx-project.json as a value for \"replaceWithEnv\" \
                         property, but it's not set in your environment"
                    )),
                },
                (None, Some(f)) => {
                    let path = root.join(f);
                    match std::fs::read_to_string(&path) {
                        Ok(s) if !s.trim().is_empty() => Ok(s.trim().to_owned()),
                        _ => Err(format!(
                            "the file \"{}\" specified in the \"replacements\" property of \
                             sfdx-project.json could not be read",
                            path.display()
                        )),
                    }
                }
                (None, None) => {
                    return Err(invalid("needs replaceWithEnv or replaceWithFile".into()));
                }
            };
            rules.push(Rule {
                target,
                find,
                with,
                label,
            });
        }
        Ok(Self {
            rules,
            warnings: Mutex::default(),
        })
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    fn matching(&self, path: &Path) -> impl Iterator<Item = &Rule> {
        let posix = path.to_string_lossy().replace('\\', "/");
        self.rules.iter().filter(move |r| match &r.target {
            Target::File(f) => posix.ends_with(f.as_str()),
            Target::Glob(g) => g.is_match(&posix),
        })
    }

    /// Whether reading `path` may change its bytes.
    pub fn applies_to(&self, path: &Path) -> bool {
        self.matching(path).next().is_some()
    }

    /// The file's bytes with the replacements that target it.
    pub fn read(&self, path: &Path) -> Result<Vec<u8>> {
        let bytes = std::fs::read(path).map_err(|source| Error::Io {
            path: path.to_owned(),
            source,
        })?;
        let rules: Vec<&Rule> = self.matching(path).collect();
        if rules.is_empty() || !is_text(path, &bytes) {
            return Ok(bytes);
        }
        let Ok(mut text) = String::from_utf8(bytes.clone()) else {
            return Ok(bytes);
        };
        for r in rules {
            let with = r.with.as_ref().map_err(|reason| Error::ReplacementValue {
                path: path.to_owned(),
                reason: reason.clone(),
            })?;
            if r.find.is_match(&text) {
                text = js_replace(&r.find, &text, with);
            } else if matches!(r.target, Target::File(_)) {
                self.warnings.lock().expect("not poisoned").insert(format!(
                    "Your sfdx-project.json specifies that {} should be replaced in {}, \
                     but it was not found.",
                    r.label,
                    path.display()
                ));
            }
        }
        Ok(text.into_bytes())
    }

    /// Warnings so far: `filename` entries that found nothing.
    pub fn warnings(&self) -> Vec<String> {
        self.warnings
            .lock()
            .expect("not poisoned")
            .iter()
            .cloned()
            .collect()
    }
}

/// SDR's text check: a known text extension, or no NUL byte and valid UTF-8 in the head.
fn is_text(path: &Path, bytes: &[u8]) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default();
    if TEXT_EXTENSIONS.contains(&ext) {
        return true;
    }
    let head = &bytes[..bytes.len().min(1024)];
    !head.contains(&0)
        && match std::str::from_utf8(head) {
            Ok(_) => true,
            // A multi-byte character cut at the end of the head is still text.
            Err(e) => e.error_len().is_none(),
        }
}

/// How `RegExp.prototype.toString` shows an escaped literal: SDR escapes `/` too.
fn js_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if "-/\\^$*+?.()|[]{}".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `input.replace(/re/g, template)` in JavaScript: `$$`, `$&`, `` $` ``, `$'`, `$n`, `$nn`
/// and `$<name>` expand; anything else after `$` stays as is.
fn js_replace(re: &Regex, input: &str, template: &str) -> String {
    let named = re.capture_names().flatten().next().is_some();
    let mut out = String::with_capacity(input.len());
    let mut last = 0;
    for caps in re.captures_iter(input) {
        let m = caps.get(0).expect("group 0");
        out.push_str(&input[last..m.start()]);
        expand(&caps, input, template, named, &mut out);
        last = m.end();
    }
    out.push_str(&input[last..]);
    out
}

fn expand(caps: &Captures, input: &str, template: &str, named: bool, out: &mut String) {
    let m = caps.get(0).expect("group 0");
    let groups = caps.len() - 1;
    let group = |n: usize| caps.get(n).map_or("", |g| g.as_str());
    let mut rest = template;
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let mut chars = after.chars();
        let consumed = match chars.next() {
            Some('$') => {
                out.push('$');
                1
            }
            Some('&') => {
                out.push_str(m.as_str());
                1
            }
            Some('`') => {
                out.push_str(&input[..m.start()]);
                1
            }
            Some('\'') => {
                out.push_str(&input[m.end()..]);
                1
            }
            Some(d) if d.is_ascii_digit() => {
                let one = d.to_digit(10).expect("digit") as usize;
                let two = chars
                    .next()
                    .and_then(|e| e.to_digit(10))
                    .map(|e| one * 10 + e as usize);
                match two {
                    Some(n) if n >= 1 && n <= groups => {
                        out.push_str(group(n));
                        2
                    }
                    _ if one >= 1 && one <= groups => {
                        out.push_str(group(one));
                        1
                    }
                    _ => {
                        out.push('$');
                        0
                    }
                }
            }
            Some('<') if named => match after.find('>') {
                Some(end) => {
                    out.push_str(caps.name(&after[1..end]).map_or("", |g| g.as_str()));
                    end + 1
                }
                None => {
                    out.push('$');
                    0
                }
            },
            _ => {
                out.push('$');
                0
            }
        };
        rest = &after[consumed..];
    }
    out.push_str(rest);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn entry(json: &str) -> Replacement {
        serde_json::from_str(json).unwrap()
    }

    fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn js_replacement_patterns() {
        let re = Regex::new("(a)(b)?").unwrap();
        assert_eq!(
            js_replace(&re, "xab", "[$&|$1|$2|$$|$3|$]"),
            "x[ab|a|b|$|$3|$]"
        );
        assert_eq!(js_replace(&re, "xay", "<$`$'>"), "x<xy>y");
        let named = Regex::new("(?<v>\\d+)").unwrap();
        assert_eq!(js_replace(&named, "v1 v22", "#$<v>"), "v#1 v#22");
        let lit = Regex::new(&regex::escape("%%X%%")).unwrap();
        assert_eq!(js_replace(&lit, "a %%X%% b %%X%%", "$1"), "a $1 b $1");
    }

    #[test]
    fn replaces_by_filename_and_glob() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("fa/classes/A.cls");
        let n = d.path().join("fa/networks/N.network-meta.xml");
        std::fs::create_dir_all(a.parent().unwrap()).unwrap();
        std::fs::create_dir_all(n.parent().unwrap()).unwrap();
        std::fs::write(&a, "user = '%%USER%%';\n").unwrap();
        std::fs::write(&n, "<a>%%USER%% %%USER%%</a>").unwrap();
        std::fs::write(d.path().join("value.txt"), "  from file \n").unwrap();
        let r = Replacements::new(
            d.path(),
            &[
                entry(r#"{"filename": "fa/classes/A.cls", "stringToReplace": "%%USER%%", "replaceWithEnv": "U"}"#),
                entry(r#"{"glob": "fa/**/networks/*.xml", "regexToReplace": "%%(\\w+)%%", "replaceWithFile": "value.txt"}"#),
                entry(r#"{"filename": "fa/classes/A.cls", "stringToReplace": "nothing", "replaceWithEnv": "U"}"#),
            ],
            env(&[("U", "me@x.com")]),
        )
        .unwrap();
        assert_eq!(r.read(&a).unwrap(), b"user = 'me@x.com';\n");
        assert_eq!(r.read(&n).unwrap(), b"<a>from file from file</a>");
        assert_eq!(r.warnings().len(), 1, "{:?}", r.warnings());
        assert!(r.warnings()[0].contains("/nothing/g"));
    }

    #[test]
    fn env_conditions_and_unset_values() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("A.cls");
        std::fs::write(&a, "X").unwrap();
        let rules = [entry(
            r#"{"filename": "A.cls", "stringToReplace": "X", "replaceWithEnv": "V", "replaceWhenEnv": [{"env": "STAGE", "value": "prod"}]}"#,
        )];
        let off = Replacements::new(d.path(), &rules, env(&[("STAGE", "dev")])).unwrap();
        assert!(off.is_empty());
        let unset = Replacements::new(d.path(), &rules, env(&[("STAGE", "prod")])).unwrap();
        assert!(matches!(
            unset.read(&a),
            Err(Error::ReplacementValue { .. })
        ));
        // An unset variable only matters for files that are read.
        assert!(
            unset
                .read(&d.path().join("missing.cls"))
                .is_err_and(|e| matches!(e, Error::Io { .. }))
        );

        let allowed = Replacements::new(
            d.path(),
            &[entry(r#"{"filename": "A.cls", "stringToReplace": "X", "replaceWithEnv": "V", "allowUnsetEnvVariable": true}"#)],
            env(&[]),
        )
        .unwrap();
        assert_eq!(allowed.read(&a).unwrap(), b"");
    }

    #[test]
    fn binary_files_are_left_alone() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("logo.png");
        std::fs::write(&p, b"\x89PNG\0X").unwrap();
        let r = Replacements::new(
            d.path(),
            &[entry(
                r#"{"glob": "*.png", "stringToReplace": "X", "replaceWithEnv": "V"}"#,
            )],
            env(&[("V", "Y")]),
        )
        .unwrap();
        assert_eq!(r.read(&p).unwrap(), b"\x89PNG\0X");
    }
}
