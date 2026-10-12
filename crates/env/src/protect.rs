//! Protect: what an org's policies allow, decided before anything is built or sent.
//!
//! Policies come from every environment that names the org by Org Id (the master is one of
//! them) and add up: an alias pointing at a protected org never gets a softer rule.

use std::collections::BTreeSet;
use std::fmt;

use globset::{GlobBuilder, GlobMatcher};
use orgpunk_config::project::{Policy, ProjectConfig};
use orgpunk_repo::State;

/// The policies of one org and where each comes from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rules {
    pub policies: BTreeSet<Policy>,
    /// Every source with a `branch` policy and its allowed branches; all must allow the branch.
    pub branches: Vec<(String, Vec<String>)>,
    /// `env.prod`, `env.hotfix`, …
    pub sources: Vec<String>,
}

impl Rules {
    pub fn for_org(config: &ProjectConfig, org_id: &str) -> Self {
        let mut r = Self::default();
        let mut add = |source: String, policies: &[Policy], branches: &[String]| {
            r.policies.extend(policies.iter().copied());
            if policies.contains(&Policy::Branch) {
                r.branches.push((source.clone(), branches.to_vec()));
            }
            r.sources.push(source);
        };
        for e in config.envs_of(org_id) {
            add(format!("env.{}", e.name), &e.protect, &e.branches);
        }
        r
    }

    pub fn is_protected(&self) -> bool {
        !self.policies.is_empty()
    }

    pub fn has(&self, p: Policy) -> bool {
        self.policies.contains(&p)
    }

    /// `env.prod (confirm, validate-first, branch)`.
    pub fn describe(&self) -> String {
        let list: Vec<&str> = self.policies.iter().map(|p| p.as_str()).collect();
        format!("{} ({})", self.sources.join(", "), list.join(", "))
    }
}

/// What the deploy asks for.
#[derive(Debug, Clone, Default)]
pub struct Request<'a> {
    pub org_id: &'a str,
    pub alias: &'a str,
    /// Prompts are possible: not CI and a terminal on stdin.
    pub interactive: bool,
    pub confirm_org: Option<&'a str>,
    /// Quick deploy of an earlier validation.
    pub quick: bool,
}

/// How the deploy has to go.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Decision {
    /// Ask for the org alias on the terminal before sending.
    pub ask_confirmation: bool,
    /// checkOnly first, then quick deploy of that validation.
    pub validate_first: bool,
    /// HEAD of the working copy when a `branch` policy read it.
    pub commit: Option<String>,
}

/// A deploy protect does not allow; exit code 2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub policy: Policy,
    pub reason: String,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "protect `{}`: {}", self.policy, self.reason)
    }
}

impl std::error::Error for Refusal {}

/// Checks everything that can be known before building the package. `repo` is read only for
/// a `branch` policy.
pub fn check(
    rules: &Rules,
    req: &Request,
    repo: impl FnOnce() -> Result<State, String>,
) -> Result<Decision, Refusal> {
    let refuse = |policy, reason: String| Err(Refusal { policy, reason });
    let mut d = Decision::default();
    if rules.has(Policy::Locked) {
        return refuse(
            Policy::Locked,
            format!("{} is locked: no deploys", rules.describe()),
        );
    }
    if rules.has(Policy::Branch) {
        let state = match repo() {
            Ok(s) => s,
            Err(e) => {
                return refuse(
                    Policy::Branch,
                    format!("cannot read the git repository: {e}"),
                );
            }
        };
        let Some(branch) = state.branch.as_deref() else {
            return refuse(
                Policy::Branch,
                "HEAD is detached; check out an allowed branch".into(),
            );
        };
        for (source, patterns) in &rules.branches {
            if !patterns
                .iter()
                .any(|p| glob(p).is_some_and(|g| g.is_match(branch)))
            {
                return refuse(
                    Policy::Branch,
                    format!(
                        "branch `{branch}` is not allowed by {source} ({})",
                        patterns.join(", ")
                    ),
                );
            }
        }
        if !state.is_clean() {
            let shown: Vec<&str> = state.dirty.iter().take(5).map(String::as_str).collect();
            let more = state.dirty.len().saturating_sub(shown.len());
            return refuse(
                Policy::Branch,
                format!(
                    "the working tree has uncommitted changes: {}{}",
                    shown.join(", "),
                    if more > 0 {
                        format!(" and {more} more")
                    } else {
                        String::new()
                    }
                ),
            );
        }
        match &state.upstream {
            None => {
                return refuse(
                    Policy::Branch,
                    format!("branch `{branch}` has no upstream; push it first"),
                );
            }
            Some(u) if !state.in_sync() => {
                return refuse(
                    Policy::Branch,
                    format!(
                        "HEAD is not at {} (as of the last fetch); push or pull first",
                        u.name
                    ),
                );
            }
            Some(_) => {}
        }
        d.commit = state.head;
    }
    if rules.has(Policy::Confirm) {
        match req.confirm_org {
            Some(id) if orgpunk_config::project::same_org(id, req.org_id) => {}
            Some(id) => {
                return refuse(
                    Policy::Confirm,
                    format!("--confirm-org {id} is not the target org {}", req.org_id),
                );
            }
            None if req.interactive => d.ask_confirmation = true,
            None => {
                return refuse(
                    Policy::Confirm,
                    format!(
                        "pass --confirm-org {} to deploy to {} without a prompt",
                        req.org_id, req.alias
                    ),
                );
            }
        }
    }
    d.validate_first = rules.has(Policy::ValidateFirst) && !req.quick;
    Ok(d)
}

/// `no-destructive`: checked once the package is known.
pub fn check_destructive(rules: &Rules, destructive: &[String]) -> Result<(), Refusal> {
    if rules.has(Policy::NoDestructive) && !destructive.is_empty() {
        return Err(Refusal {
            policy: Policy::NoDestructive,
            reason: format!(
                "{} allows no deletions, the package has {}",
                rules.describe(),
                destructive.join(", ")
            ),
        });
    }
    Ok(())
}

fn glob(pattern: &str) -> Option<GlobMatcher> {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .ok()
        .map(|g| g.compile_matcher())
}

#[cfg(test)]
mod tests {
    use super::*;
    use orgpunk_repo::Upstream;

    const PROD: &str = "00D000000000001AAA";
    const QA: &str = "00D000000000002AAA";

    fn config() -> ProjectConfig {
        ProjectConfig::parse(&format!(
            r#"
            [env.uat]
            org = "{QA}"
            protect = ["branch", "validate-first"]
            branches = ["release/*"]
            [env.prod]
            org = "{PROD}"
            master = true
            [env.hotfix]
            org = "{PROD}"
            protect = ["no-destructive"]
            "#
        ))
        .unwrap()
    }

    fn repo(branch: &str, dirty: &[&str], synced: bool) -> impl FnOnce() -> Result<State, String> {
        let s = State {
            branch: Some(branch.to_owned()),
            head: Some("aaa".into()),
            upstream: Some(Upstream {
                name: format!("origin/{branch}"),
                commit: if synced { "aaa" } else { "bbb" }.into(),
            }),
            dirty: dirty.iter().map(|s| (*s).to_owned()).collect(),
        };
        move || Ok(s)
    }

    fn req<'a>(org: &'a str) -> Request<'a> {
        Request {
            org_id: org,
            alias: "x",
            ..Request::default()
        }
    }

    #[test]
    fn policies_add_up_by_org_id() {
        let r = Rules::for_org(&config(), "00D000000000001");
        assert_eq!(r.sources, ["env.hotfix", "env.prod"]);
        assert!(r.has(Policy::Confirm) && r.has(Policy::NoDestructive));
        assert!(!Rules::for_org(&config(), "00D000000000009AAA").is_protected());
    }

    #[test]
    fn branch_rules() {
        let r = Rules::for_org(&config(), QA);
        let ok = check(&r, &req(QA), repo("release/1.2", &[], true)).unwrap();
        assert!(ok.validate_first && !ok.ask_confirmation);
        assert_eq!(ok.commit.as_deref(), Some("aaa"));

        for (branch, dirty, synced, want) in [
            ("feature/x", &[][..], true, "not allowed"),
            ("release/1/2", &[][..], true, "not allowed"),
            ("release/1", &["a.cls"][..], true, "uncommitted"),
            ("release/1", &[][..], false, "not at origin/release/1"),
        ] {
            let e = check(&r, &req(QA), repo(branch, dirty, synced)).unwrap_err();
            assert_eq!(e.policy, Policy::Branch);
            assert!(e.reason.contains(want), "{branch}: {e}");
        }
        let e = check(&r, &req(QA), || Err("not a repo".into())).unwrap_err();
        assert!(e.reason.contains("not a repo"));
    }

    #[test]
    fn confirmation() {
        let r = Rules::for_org(&config(), PROD);
        let main = || repo("main", &[], true);
        let e = check(&r, &req(PROD), main()).unwrap_err();
        assert_eq!(e.policy, Policy::Confirm);
        assert!(e.reason.contains("--confirm-org"));

        let interactive = Request {
            interactive: true,
            ..req(PROD)
        };
        assert!(check(&r, &interactive, main()).unwrap().ask_confirmation);

        let flag = Request {
            confirm_org: Some("00D000000000001"),
            ..req(PROD)
        };
        assert!(!check(&r, &flag, main()).unwrap().ask_confirmation);

        let wrong = Request {
            confirm_org: Some(QA),
            interactive: true,
            ..req(PROD)
        };
        assert_eq!(
            check(&r, &wrong, main()).unwrap_err().policy,
            Policy::Confirm
        );

        let quick = Request {
            quick: true,
            ..flag
        };
        assert!(!check(&r, &quick, main()).unwrap().validate_first);
    }

    #[test]
    fn locked_and_destructive() {
        let c = ProjectConfig::parse(&format!(
            "[env.prod]\norg = \"{PROD}\"\nmaster = true\nprotect = [\"locked\"]"
        ))
        .unwrap();
        let r = Rules::for_org(&c, PROD);
        let e = check(&r, &req(PROD), || panic!("git is not read when locked")).unwrap_err();
        assert_eq!(e.policy, Policy::Locked);

        let r = Rules::for_org(&config(), PROD);
        assert!(check_destructive(&r, &[]).is_ok());
        let e = check_destructive(&r, &["destructiveChangesPost.xml".into()]).unwrap_err();
        assert_eq!(e.policy, Policy::NoDestructive);
    }

    #[test]
    fn ci_detection() {
        assert!(crate::ci_mode(true, None));
        assert!(crate::ci_mode(false, Some("true")));
        assert!(!crate::ci_mode(false, Some("false")));
        assert!(!crate::ci_mode(false, None));
    }
}
