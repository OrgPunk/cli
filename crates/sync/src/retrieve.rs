//! Retrieving members from the org as mdapi files.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Cursor, Read};
use std::time::{Duration, Instant};

use orgpunk_convert::registry;
use orgpunk_convert::to_source::Files;
use orgpunk_sfapi::{Client, RetrieveStatus};

use crate::{Error, Result};

pub struct Retrieved {
    pub files: Files,
    /// The org's warnings, e.g. components it does not have.
    pub messages: Vec<String>,
    /// Members of types opk does not know, not retrieved.
    pub skipped: Vec<String>,
}

/// Components per retrieve, well under the 10,000 files a retrieve may hold.
const BATCH: usize = 2000;

/// Retrieves `Type:Name` members and waits until `timeout` for each batch.
pub fn retrieve(client: &Client, members: &[String], timeout: Duration) -> Result<Retrieved> {
    let reg = registry::get();
    let mut skipped = Vec::new();
    // Members that come back in one file (an object's fields, all labels) stay in one batch:
    // a later batch would overwrite the file with only its own members.
    let mut groups: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for m in members {
        match m.split_once(':') {
            Some((kind, name)) if reg.by_name(kind).is_some() => {
                let group = match reg.parent(kind) {
                    Some(p) if kind == "CustomLabel" => p.name.clone(),
                    Some(p) => format!("{}:{}", p.name, name.split('.').next().unwrap_or(name)),
                    None => format!("{kind}:{name}"),
                };
                groups
                    .entry(group.to_lowercase())
                    .or_default()
                    .push((kind.to_owned(), name.to_owned()));
            }
            _ => skipped.push(m.clone()),
        }
    }
    let mut out = Retrieved {
        files: Files::new(),
        messages: Vec::new(),
        skipped,
    };
    let mut batch: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut size = 0;
    let groups: Vec<_> = groups.into_values().collect();
    for (i, group) in groups.iter().enumerate() {
        size += group.len();
        for (kind, name) in group {
            batch.entry(kind.clone()).or_default().insert(name.clone());
        }
        if size >= BATCH || i + 1 == groups.len() {
            one(client, &std::mem::take(&mut batch), timeout, &mut out)?;
            size = 0;
        }
    }
    Ok(out)
}

fn one(
    client: &Client,
    types: &BTreeMap<String, BTreeSet<String>>,
    timeout: Duration,
    out: &mut Retrieved,
) -> Result<()> {
    let id = client.retrieve(types)?;
    let deadline = Instant::now() + timeout;
    let mut pause = Duration::from_millis(500);
    let result = loop {
        let (status, r) = client.check_retrieve(&id)?;
        if let RetrieveStatus::Done(done) = status {
            break done.map(|()| r);
        }
        if Instant::now() >= deadline {
            return Err(Error::Retrieve(format!(
                "{id} still running after the wait"
            )));
        }
        std::thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_secs(5));
    };
    let r = result.map_err(|reason| Error::Retrieve(format!("{id}: {reason}")))?;
    for m in r.messages {
        // Messages without a problem say nothing; the same problem comes once per member.
        let blank = m.split_once(": ").is_none_or(|(_, p)| p.trim().is_empty());
        if !blank && !out.messages.contains(&m) {
            out.messages.push(m);
        }
    }
    let bad = |e: &dyn std::fmt::Display| Error::Retrieve(format!("{id}: zip: {e}"));
    let mut zip = zip::ZipArchive::new(Cursor::new(r.zip)).map_err(|e| bad(&e))?;
    for i in 0..zip.len() {
        let mut f = zip.by_index(i).map_err(|e| bad(&e))?;
        if f.is_dir() {
            continue;
        }
        let name = f.name().replace('\\', "/");
        let mut data = Vec::new();
        f.read_to_end(&mut data).map_err(|e| bad(&e))?;
        out.files.insert(name, data);
    }
    Ok(())
}
