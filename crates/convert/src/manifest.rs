//! package.xml: types with their members and the API version.

use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// Type → members; a member may be the `*` wildcard.
    pub types: BTreeMap<String, BTreeSet<String>>,
    pub version: Option<String>,
}

impl Manifest {
    pub fn parse(xml: &str) -> Result<Self, String> {
        let doc = roxmltree::Document::parse(xml).map_err(|e| e.to_string())?;
        let root = doc.root_element();
        if root.tag_name().name() != "Package" {
            return Err(format!(
                "root element is <{}>, expected <Package>",
                root.tag_name().name()
            ));
        }
        let mut m = Self::default();
        for node in root.children().filter(|n| n.is_element()) {
            match node.tag_name().name() {
                "types" => {
                    let child_text = |tag: &'static str| {
                        node.children()
                            .filter(move |n| n.has_tag_name(tag))
                            .map(|n| n.text().unwrap_or_default().trim().to_owned())
                    };
                    let name = child_text("name").next().ok_or("<types> without <name>")?;
                    m.types
                        .entry(name)
                        .or_default()
                        .extend(child_text("members"));
                }
                "version" => m.version = node.text().map(|t| t.trim().to_owned()),
                _ => {}
            }
        }
        Ok(m)
    }

    pub fn add(&mut self, kind: &str, member: &str) {
        self.types
            .entry(kind.to_owned())
            .or_default()
            .insert(member.to_owned());
    }

    pub fn to_xml(&self) -> String {
        let mut s = String::from(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <Package xmlns=\"http://soap.sforce.com/2006/04/metadata\">\n",
        );
        for (name, members) in &self.types {
            s += "    <types>\n";
            for m in members {
                s += &format!("        <members>{}</members>\n", escape(m));
            }
            s += &format!("        <name>{}</name>\n    </types>\n", escape(name));
        }
        if let Some(v) = &self.version {
            s += &format!("    <version>{}</version>\n", escape(v));
        }
        s += "</Package>\n";
        s
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<Package xmlns="http://soap.sforce.com/2006/04/metadata">
    <!-- comment -->
    <types><members>B</members><members>A</members><name>ApexClass</name></types>
    <types><members>Account-Account &amp; Co</members><name>Layout</name></types>
    <version>62.0</version>
</Package>"#;
        let m = Manifest::parse(xml).unwrap();
        assert_eq!(m.version.as_deref(), Some("62.0"));
        assert_eq!(m.types["ApexClass"].iter().collect::<Vec<_>>(), ["A", "B"]);
        assert!(m.types["Layout"].contains("Account-Account & Co"));
        assert_eq!(Manifest::parse(&m.to_xml()).unwrap(), m);
    }

    #[test]
    fn minimal_and_invalid() {
        assert_eq!(Manifest::parse("<Package/>").unwrap(), Manifest::default());
        assert!(Manifest::parse("<Other/>").is_err());
        assert!(Manifest::parse("<Package><types/></Package>").is_err());
        assert!(Manifest::parse("not xml").is_err());
    }
}
