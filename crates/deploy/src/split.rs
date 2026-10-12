//! Splitting an mdapi directory that exceeds the deploy limits into several deploys.
//!
//! Components that reference each other (a strongly connected set in the dependency graph)
//! always go into the same part. The sets are ordered so that each goes after everything it
//! depends on, and packed greedily into parts that fit the limits. Every part is a separate
//! deploy with its own package.xml: a later part relies on the earlier ones already being in
//! the org, so the parts are atomic one by one, not as a whole.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashMap};
use std::io::Write;
use std::path::Path;

use orgpunk_convert::deps::Scanner;
use orgpunk_convert::manifest::Manifest;
use orgpunk_convert::mdapi::{self, MdapiDir};
use orgpunk_convert::registry;

use crate::{Content, Error, Limits, Package, Result, check, mib, zip_entries};

/// A deploy split into parts.
pub struct Plan {
    dir: MdapiDir,
    part_of: Vec<usize>,
    /// (kind, lowercase name) → component.
    by_key: HashMap<(String, String), usize>,
    pub parts: Vec<Part>,
    limits: Limits,
}

#[derive(Debug, Default)]
pub struct Part {
    /// Indexes into the directory's components, in deploy order.
    pub components: Vec<usize>,
    pub files: usize,
    pub unzipped_bytes: u64,
    /// Estimated zip size.
    pub zip_bytes: u64,
    /// Contains Apex classes or triggers, so tests may be required.
    pub has_apex: bool,
}

#[derive(Debug, Default, Clone, Copy)]
struct Size {
    files: usize,
    bytes: u64,
    zipped: u64,
}

impl std::ops::Add for Size {
    type Output = Self;
    fn add(self, o: Self) -> Self {
        Self {
            files: self.files + o.files,
            bytes: self.bytes + o.bytes,
            zipped: self.zipped + o.zipped,
        }
    }
}

impl Size {
    fn fits(self, budget: Self) -> bool {
        self.files <= budget.files && self.bytes <= budget.bytes && self.zipped <= budget.zipped
    }
}

/// Room in package.xml for one member, uncompressed.
const MANIFEST_ENTRY: u64 = 64;
/// Zip headers per entry (local + central directory + data descriptor) besides the name twice.
const ZIP_ENTRY: u64 = 30 + 46 + 16;

/// Reads the directory, builds the dependency graph and packs the components into parts.
pub fn plan(dir: &Path, limits: Limits, allow_destructive: bool) -> Result<Plan> {
    let md = mdapi::read(dir)?;
    if !md.destructive.is_empty() && !allow_destructive {
        let names: Vec<_> = md.destructive.iter().map(|f| f.path.as_str()).collect();
        return Err(Error::Destructive(names.join(", ")));
    }

    let mut sizes = vec![Size::default(); md.components.len()];
    let mut scanner = Scanner::new(&md.components);
    for (i, c) in md.components.iter().enumerate() {
        let entry = MANIFEST_ENTRY + (c.kind.len() + c.name.len()) as u64;
        sizes[i] = Size {
            files: 0,
            bytes: entry,
            zipped: entry,
        };
        for f in &c.files {
            let bytes = std::fs::read(&f.abs).map_err(|source| Error::Io {
                path: f.abs.clone(),
                source,
            })?;
            scanner.scan_file(i, &f.path, &bytes);
            sizes[i] = sizes[i]
                + Size {
                    files: 1,
                    bytes: bytes.len() as u64,
                    zipped: deflated_len(&bytes) + ZIP_ENTRY + 2 * f.path.len() as u64,
                };
        }
    }
    let deps = scanner.finish();

    // package.xml and the destructive manifests go with the parts on top of the components.
    let extra: u64 = 1024
        + md.destructive
            .iter()
            .map(|f| f.size + ZIP_ENTRY + 2 * f.path.len() as u64)
            .sum::<u64>();
    let budget = Size {
        files: limits.max_files.saturating_sub(1 + md.destructive.len()),
        bytes: limits.max_unzipped_bytes.saturating_sub(extra),
        // Deflate output differs a little between encoders: keep a margin.
        zipped: (limits.max_zip_bytes / 100 * 97).saturating_sub(extra),
    };

    let mut parts: Vec<Part> = Vec::new();
    let mut part = Part::default();
    let mut used = Size::default();
    for group in deploy_order(&md.components, &deps) {
        let size = group.iter().fold(Size::default(), |acc, &c| acc + sizes[c]);
        if !size.fits(budget) {
            return Err(too_big(&md, &group, size, budget));
        }
        if !(used + size).fits(budget) {
            parts.push(std::mem::take(&mut part));
            used = Size::default();
        }
        used = used + size;
        part.files = used.files;
        part.unzipped_bytes = used.bytes;
        part.zip_bytes = used.zipped;
        part.has_apex |= group
            .iter()
            .any(|&c| matches!(md.components[c].kind.as_str(), "ApexClass" | "ApexTrigger"));
        part.components.extend(group);
    }
    if !part.components.is_empty() || parts.is_empty() {
        parts.push(part);
    }

    let mut part_of = vec![0; md.components.len()];
    for (p, part) in parts.iter().enumerate() {
        for &c in &part.components {
            part_of[c] = p;
        }
    }
    let by_key = md
        .components
        .iter()
        .enumerate()
        .map(|(i, c)| ((c.kind.clone(), c.name.to_lowercase()), i))
        .collect();
    Ok(Plan {
        dir: md,
        part_of,
        by_key,
        parts,
        limits,
    })
}

impl Plan {
    pub fn files(&self) -> usize {
        self.dir.components.iter().map(|c| c.files.len()).sum()
    }

    /// The destructiveChanges*.xml files of the directory.
    pub fn destructive_files(&self) -> Vec<String> {
        self.dir
            .destructive
            .iter()
            .map(|f| f.path.clone())
            .collect()
    }

    /// The most frequent types of a part: `ApexClass 120, CustomObject 30, …`.
    pub fn describe(&self, part: usize) -> String {
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for &c in &self.parts[part].components {
            *counts.entry(&self.dir.components[c].kind).or_default() += 1;
        }
        let mut counts: Vec<_> = counts.into_iter().collect();
        counts.sort_by_key(|&(kind, n)| (Reverse(n), kind));
        let mut s = counts
            .iter()
            .take(4)
            .map(|(kind, n)| format!("{kind} {n}"))
            .collect::<Vec<_>>()
            .join(", ");
        let rest: usize = counts.iter().skip(4).map(|(_, n)| n).sum();
        if rest > 0 {
            s += &format!(", other {rest}");
        }
        s
    }

    /// `Type:Name` of the components in a part.
    pub fn members(&self, part: usize) -> Vec<String> {
        self.parts[part]
            .components
            .iter()
            .map(|&c| {
                let c = &self.dir.components[c];
                format!("{}:{}", c.kind, c.name)
            })
            .collect()
    }

    /// The package.xml of a part: the original entries whose components are in it.
    /// Wildcards go to every part with that type; entries without files go to the last part,
    /// where the deploy reports them.
    pub fn manifest(&self, part: usize) -> Manifest {
        let last = self.parts.len() - 1;
        let mut m = Manifest {
            version: self.dir.manifest.version.clone(),
            ..Manifest::default()
        };
        for (kind, members) in &self.dir.manifest.types {
            for member in members {
                let (owner_kind, owner_name) = registry::owner(kind, member);
                let include = if member == "*" {
                    self.parts[part]
                        .components
                        .iter()
                        .any(|&c| self.dir.components[c].kind == owner_kind)
                } else {
                    match self
                        .by_key
                        .get(&(owner_kind.to_owned(), owner_name.to_lowercase()))
                    {
                        Some(&c) => self.part_of[c] == part,
                        None => part == last,
                    }
                };
                if include {
                    m.add(kind, member);
                }
            }
        }
        m
    }

    /// Zips one part: its components, its package.xml and the destructive manifests that
    /// belong to it (`destructiveChangesPre.xml` to the first part, the others to the last).
    pub fn package(&self, part: usize) -> Result<Package> {
        let manifest = self.manifest(part).to_xml();
        let last = self.parts.len() - 1;
        let mut entries: Vec<(&str, Content)> =
            vec![("package.xml", Content::Bytes(manifest.as_bytes()))];
        let mut destructive = Vec::new();
        for f in &self.dir.destructive {
            let here = if f.path == "destructiveChangesPre.xml" {
                part == 0
            } else {
                part == last
            };
            if here {
                entries.push((&f.path, Content::File(&f.abs)));
                destructive.push(f.path.clone());
            }
        }
        for &c in &self.parts[part].components {
            for f in &self.dir.components[c].files {
                entries.push((&f.path, Content::File(&f.abs)));
            }
        }
        entries.sort_by(|a, b| a.0.cmp(b.0));
        let files = entries.len();
        check(
            "file count",
            files as u64,
            self.limits.max_files as u64,
            |n| n.to_string(),
        )?;
        let (zip, unzipped_bytes) = zip_entries(&entries)?;
        check(
            "unzipped size",
            unzipped_bytes,
            self.limits.max_unzipped_bytes,
            mib,
        )?;
        check("zip size", zip.len() as u64, self.limits.max_zip_bytes, mib)?;
        Ok(Package {
            zip,
            files,
            unzipped_bytes,
            destructive,
        })
    }
}

fn too_big(md: &MdapiDir, group: &[usize], size: Size, budget: Size) -> Error {
    let mut names: Vec<String> = group
        .iter()
        .take(10)
        .map(|&c| format!("{}:{}", md.components[c].kind, md.components[c].name))
        .collect();
    if group.len() > names.len() {
        names.push(format!("… {} more", group.len() - names.len()));
    }
    Error::TooBigToSplit {
        components: group.len(),
        size: format!(
            "{} files of max {}, {} of max {}, zip ~{} of max {}",
            size.files,
            budget.files,
            mib(size.bytes),
            mib(budget.bytes),
            mib(size.zipped),
            mib(budget.zipped)
        ),
        members: names.join(", "),
    }
}

fn deflated_len(bytes: &[u8]) -> u64 {
    let mut enc =
        flate2::write::DeflateEncoder::new(Counter::default(), flate2::Compression::default());
    enc.write_all(bytes).expect("writing to a counter");
    enc.finish().expect("writing to a counter").0
}

#[derive(Default)]
struct Counter(u64);

impl Write for Counter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 += buf.len() as u64;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Groups of components that must deploy together, each after all groups it depends on.
/// Among ready groups, the type rank decides (settings and labels first, profiles last).
fn deploy_order(
    components: &[orgpunk_convert::mdapi::Component],
    deps: &[Vec<usize>],
) -> Vec<Vec<usize>> {
    let sccs = strongly_connected(deps);
    let mut scc_of = vec![0; deps.len()];
    for (s, members) in sccs.iter().enumerate() {
        for &m in members {
            scc_of[m] = s;
        }
    }
    let mut waiting_on = vec![0usize; sccs.len()];
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); sccs.len()];
    for (from, targets) in deps.iter().enumerate() {
        for &to in targets {
            let (a, b) = (scc_of[from], scc_of[to]);
            if a != b {
                dependents[b].push(a);
                waiting_on[a] += 1;
            }
        }
    }
    let key = |s: usize| {
        let rank = sccs[s]
            .iter()
            .map(|&c| registry::rank(&components[c].kind))
            .min()
            .unwrap_or(u8::MAX);
        let first = sccs[s].iter().min().copied().unwrap_or(0);
        Reverse((rank, first, s))
    };
    let mut ready: BinaryHeap<_> = (0..sccs.len())
        .filter(|&s| waiting_on[s] == 0)
        .map(key)
        .collect();
    let mut order = Vec::with_capacity(sccs.len());
    while let Some(Reverse((_, _, s))) = ready.pop() {
        for &d in &dependents[s] {
            waiting_on[d] -= 1;
            if waiting_on[d] == 0 {
                ready.push(key(d));
            }
        }
        let mut members = sccs[s].clone();
        members.sort_unstable();
        order.push(members);
    }
    debug_assert_eq!(order.len(), sccs.len(), "the condensed graph is acyclic");
    order
}

/// Tarjan's algorithm without recursion (the graph may have hundreds of thousands of nodes).
fn strongly_connected(edges: &[Vec<usize>]) -> Vec<Vec<usize>> {
    const UNSEEN: usize = usize::MAX;
    let n = edges.len();
    let mut index = vec![UNSEEN; n];
    let mut low = vec![0; n];
    let mut on_stack = vec![false; n];
    let mut stack = Vec::new();
    let mut sccs = Vec::new();
    let mut next = 0;
    let mut calls: Vec<(usize, usize)> = Vec::new();
    for root in 0..n {
        if index[root] != UNSEEN {
            continue;
        }
        index[root] = next;
        low[root] = next;
        next += 1;
        stack.push(root);
        on_stack[root] = true;
        calls.push((root, 0));
        while let Some(&(v, edge)) = calls.last() {
            if let Some(&w) = edges[v].get(edge) {
                calls.last_mut().expect("not empty").1 += 1;
                if index[w] == UNSEEN {
                    index[w] = next;
                    low[w] = next;
                    next += 1;
                    stack.push(w);
                    on_stack[w] = true;
                    calls.push((w, 0));
                } else if on_stack[w] {
                    low[v] = low[v].min(index[w]);
                }
                continue;
            }
            calls.pop();
            if let Some(&(parent, _)) = calls.last() {
                low[parent] = low[parent].min(low[v]);
            }
            if low[v] == index[v] {
                let mut scc = Vec::new();
                loop {
                    let w = stack.pop().expect("v is on the stack");
                    on_stack[w] = false;
                    scc.push(w);
                    if w == v {
                        break;
                    }
                }
                sccs.push(scc);
            }
        }
    }
    sccs
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::{Cursor, Read};

    fn mdapi(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in files {
            let p = dir.path().join(name);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, body).unwrap();
        }
        dir
    }

    fn zip_text(zip: &[u8], name: &str) -> String {
        let mut a = zip::ZipArchive::new(Cursor::new(zip)).unwrap();
        let mut s = String::new();
        a.by_name(name).unwrap().read_to_string(&mut s).unwrap();
        s
    }

    #[test]
    fn scc_finds_cycles() {
        // 0 → 1 → 2 → 0, 3 → 0, 4 alone
        let edges = vec![vec![1], vec![2], vec![0], vec![0], vec![]];
        let mut sccs: Vec<Vec<usize>> = strongly_connected(&edges)
            .into_iter()
            .map(|mut s| {
                s.sort();
                s
            })
            .collect();
        sccs.sort();
        assert_eq!(sccs, [vec![0, 1, 2], vec![3], vec![4]]);
    }

    #[test]
    fn scc_handles_a_long_chain_without_recursion() {
        let n = 200_000;
        let edges: Vec<Vec<usize>> = (0..n)
            .map(|i| if i + 1 < n { vec![i + 1] } else { vec![] })
            .collect();
        assert_eq!(strongly_connected(&edges).len(), n);
    }

    const PKG: &str = r#"<Package>
        <types><members>Invoice__c</members><name>CustomObject</name></types>
        <types><members>Invoice__c.Amount__c</members><name>CustomField</name></types>
        <types><members>*</members><name>ApexClass</name></types>
        <types><members>InvoiceTrigger</members><name>ApexTrigger</name></types>
        <types><members>Admin</members><name>Profile</name></types>
        <types><members>Ghost</members><name>ApexPage</name></types>
        <version>62.0</version>
    </Package>"#;

    fn repo() -> tempfile::TempDir {
        mdapi(&[
            ("package.xml", PKG),
            (
                "objects/Invoice__c.object",
                "<CustomObject><fields><fullName>Amount__c</fullName></fields></CustomObject>",
            ),
            ("classes/Alpha.cls", "class Alpha { Beta b; Invoice__c i; }"),
            ("classes/Alpha.cls-meta.xml", "<ApexClass/>"),
            ("classes/Beta.cls", "class Beta { Alpha a; }"),
            ("classes/Beta.cls-meta.xml", "<ApexClass/>"),
            (
                "triggers/InvoiceTrigger.trigger",
                "trigger InvoiceTrigger on Invoice__c (before insert) { Alpha.run(); }",
            ),
            ("triggers/InvoiceTrigger.trigger-meta.xml", "<ApexTrigger/>"),
            (
                "profiles/Admin.profile",
                "<Profile><classAccesses><apexClass>Alpha</apexClass></classAccesses></Profile>",
            ),
            ("destructiveChangesPre.xml", "<Package/>"),
            ("destructiveChangesPost.xml", "<Package/>"),
        ])
    }

    /// Room for 4 files per part (package.xml and two destructive files come on top).
    fn tight() -> Limits {
        Limits {
            max_files: 3 + 4,
            ..Limits::default()
        }
    }

    #[test]
    fn parts_follow_dependencies() {
        let d = repo();
        let plan = plan(d.path(), tight(), true).unwrap();
        let parts: Vec<Vec<String>> = (0..plan.parts.len()).map(|p| plan.members(p)).collect();
        assert_eq!(
            parts,
            [
                vec!["CustomObject:Invoice__c"],
                vec!["ApexClass:Alpha", "ApexClass:Beta"],
                vec!["ApexTrigger:InvoiceTrigger", "Profile:Admin"],
            ]
        );
        assert!(!plan.parts[0].has_apex && plan.parts[1].has_apex);
    }

    #[test]
    fn each_part_gets_its_manifest_and_destructive_files() {
        let d = repo();
        let plan = plan(d.path(), tight(), true).unwrap();
        let first = plan.package(0).unwrap();
        let m = Manifest::parse(&zip_text(&first.zip, "package.xml")).unwrap();
        assert_eq!(m.version.as_deref(), Some("62.0"));
        assert_eq!(
            m.types.keys().collect::<Vec<_>>(),
            ["CustomField", "CustomObject"]
        );
        assert_eq!(first.destructive, ["destructiveChangesPre.xml"]);

        let classes =
            Manifest::parse(&zip_text(&plan.package(1).unwrap().zip, "package.xml")).unwrap();
        assert_eq!(classes.types["ApexClass"].iter().collect::<Vec<_>>(), ["*"]);

        let last = plan.package(2).unwrap();
        let m = Manifest::parse(&zip_text(&last.zip, "package.xml")).unwrap();
        assert!(m.types.contains_key("Profile") && m.types.contains_key("ApexTrigger"));
        assert!(
            m.types.contains_key("ApexPage"),
            "unmatched members go last"
        );
        assert_eq!(last.destructive, ["destructiveChangesPost.xml"]);
    }

    #[test]
    fn everything_fits_in_one_part() {
        let d = repo();
        let plan = plan(d.path(), Limits::default(), true).unwrap();
        assert_eq!(plan.parts.len(), 1);
        assert_eq!(plan.parts[0].components.len(), 5);
    }

    #[test]
    fn a_cycle_over_the_limit_cannot_be_split() {
        let d = repo();
        let limits = Limits {
            max_files: 3 + 3,
            ..Limits::default()
        };
        let err = plan(d.path(), limits, true).err().unwrap();
        assert!(
            matches!(err, Error::TooBigToSplit { components: 2, .. }),
            "{err}"
        );
    }

    #[test]
    fn destructive_needs_flag() {
        let d = repo();
        assert!(
            plan(d.path(), Limits::default(), false)
                .err()
                .unwrap()
                .is_refusal()
        );
    }
}
