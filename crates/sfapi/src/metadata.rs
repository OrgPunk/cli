//! SOAP Metadata API calls the REST API lacks: `listMetadata` and `retrieve`.
//!
//! SOAP reports an expired session as an `INVALID_SESSION_ID` fault rather than a 401, so
//! these calls refresh and retry on that fault.

use crate::{Client, Error, Result};

const NS: &str = "http://soap.sforce.com/2006/04/metadata";
/// listMetadata takes at most three queries per call.
const QUERIES_PER_CALL: usize = 3;

/// One `listMetadata` query: a type, and the folder for in-folder types.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ListQuery {
    pub kind: String,
    pub folder: Option<String>,
}

/// A component as `listMetadata` reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileProperties {
    pub kind: String,
    pub full_name: String,
    pub created_date: String,
    pub last_modified_date: String,
    pub last_modified_by_id: String,
    pub last_modified_by_name: String,
    /// Empty for standard components.
    pub id: String,
    /// `unmanaged`, `installed` (from a managed package), …
    pub manageable_state: String,
}

/// What a retrieve brought back.
#[derive(Debug, Clone, Default)]
pub struct Retrieved {
    /// The zip: package.xml and the type directories at its root (single package).
    pub zip: Vec<u8>,
    /// `fileName: problem` warnings, e.g. components the org does not have.
    pub messages: Vec<String>,
}

/// Retrieve status, polled with `checkRetrieveStatus`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetrieveStatus {
    Running,
    Done(std::result::Result<(), String>),
}

/// A metadata type the org supports, from `describeMetadata`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MetadataObject {
    pub xml_name: String,
    pub in_folder: bool,
    /// Child types listed on their own, e.g. CustomField of CustomObject.
    pub child_xml_names: Vec<String>,
}

impl Client {
    /// The metadata types the org supports.
    pub fn describe_metadata(&self) -> Result<Vec<MetadataObject>> {
        let body = format!(
            "<m:describeMetadata><m:asOfVersion>{}</m:asOfVersion></m:describeMetadata>",
            self.version
        );
        parse_describe(&self.soap(&body)?)
    }

    /// Starts a retrieve of package.xml-style `types` (type → members, `*` allowed); returns
    /// its id.
    pub fn retrieve(
        &self,
        types: &std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    ) -> Result<String> {
        let mut body = format!(
            "<m:retrieve><m:retrieveRequest><m:apiVersion>{v}</m:apiVersion>\
             <m:singlePackage>true</m:singlePackage><m:unpackaged>",
            v = self.version
        );
        for (name, members) in types {
            body += "<m:types>";
            for m in members {
                body += &format!("<m:members>{}</m:members>", escape(m));
            }
            body += &format!("<m:name>{}</m:name></m:types>", escape(name));
        }
        body += &format!(
            "<m:version>{}</m:version></m:unpackaged></m:retrieveRequest></m:retrieve>",
            self.version
        );
        let xml = self.soap(&body)?;
        first_text(&xml, "id")
            .ok_or_else(|| Error::Decode(format!("retrieve: no id in {}", truncate(&xml))))
    }

    /// One poll of a retrieve: running, or done with its zip.
    pub fn check_retrieve(&self, id: &str) -> Result<(RetrieveStatus, Retrieved)> {
        let body = format!(
            "<m:checkRetrieveStatus><m:asyncProcessId>{}</m:asyncProcessId>\
             <m:includeZip>true</m:includeZip></m:checkRetrieveStatus>",
            escape(id)
        );
        parse_retrieve(&self.soap(&body)?)
    }

    /// Components of the given types (and folders) in the org.
    pub fn list_metadata(&self, queries: &[ListQuery]) -> Result<Vec<FileProperties>> {
        let mut out = Vec::new();
        for chunk in queries.chunks(QUERIES_PER_CALL) {
            let mut body = String::from("<m:listMetadata>");
            for q in chunk {
                body += "<m:queries>";
                if let Some(f) = &q.folder {
                    body += &format!("<m:folder>{}</m:folder>", escape(f));
                }
                body += &format!("<m:type>{}</m:type></m:queries>", escape(&q.kind));
            }
            body += &format!(
                "<m:asOfVersion>{}</m:asOfVersion></m:listMetadata>",
                self.version
            );
            out.extend(parse_list(&self.soap(&body)?)?);
        }
        Ok(out)
    }

    /// Sends one SOAP Metadata API body; refreshes the session once on INVALID_SESSION_ID.
    fn soap(&self, body: &str) -> Result<String> {
        let url = format!(
            "{}/services/Soap/m/{}",
            self.creds.borrow().instance_url.trim_end_matches('/'),
            self.version
        );
        let call = |token: &str| -> Result<std::result::Result<String, Fault>> {
            let envelope = format!(
                "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
                 <env:Envelope xmlns:env=\"http://schemas.xmlsoap.org/soap/envelope/\" xmlns:m=\"{NS}\">\
                 <env:Header><m:SessionHeader><m:sessionId>{}</m:sessionId></m:SessionHeader></env:Header>\
                 <env:Body>{body}</env:Body></env:Envelope>",
                escape(token)
            );
            let resp = self
                .http
                .post(&url)
                .header("Content-Type", "text/xml; charset=utf-8")
                .header("SOAPAction", "\"\"")
                .body(envelope)
                .send()?;
            let status = resp.status().as_u16();
            let text = resp.text()?;
            Ok(match fault(&text) {
                Some(f) => Err(f),
                None if (200..300).contains(&status) => Ok(text),
                None => Err(Fault {
                    code: format!("HTTP_{status}"),
                    message: text.chars().take(300).collect(),
                    status,
                }),
            })
        };
        let token = self.creds.borrow().access_token.clone();
        let first = call(&token)?;
        let result = match (first, &self.refresher) {
            (Err(f), Some(refresh)) if f.code.ends_with("INVALID_SESSION_ID") => {
                let fresh = refresh().map_err(Error::Unauthorized)?;
                let token = fresh.access_token.clone();
                *self.creds.borrow_mut() = fresh;
                call(&token)?
            }
            (r, _) => r,
        };
        result.map_err(|f| Error::Api {
            status: f.status,
            code: f.code,
            message: f.message,
        })
    }
}

struct Fault {
    code: String,
    message: String,
    status: u16,
}

fn fault(xml: &str) -> Option<Fault> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    let f = doc.descendants().find(|n| n.has_tag_name("Fault"))?;
    let text = |tag: &str| {
        f.descendants()
            .find(|n| n.has_tag_name(tag))
            .and_then(|n| n.text())
            .unwrap_or_default()
            .trim()
            .to_owned()
    };
    Some(Fault {
        code: text("faultcode"),
        message: text("faultstring"),
        status: 500,
    })
}

fn parse_list(xml: &str) -> Result<Vec<FileProperties>> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| Error::Decode(e.to_string()))?;
    let mut out = Vec::new();
    for r in doc.descendants().filter(|n| n.has_tag_name("result")) {
        let text = |tag: &str| {
            r.children()
                .find(|n| n.has_tag_name(tag))
                .and_then(|n| n.text())
                .unwrap_or_default()
                .to_owned()
        };
        out.push(FileProperties {
            kind: text("type"),
            full_name: text("fullName"),
            created_date: text("createdDate"),
            last_modified_date: text("lastModifiedDate"),
            last_modified_by_id: text("lastModifiedById"),
            last_modified_by_name: text("lastModifiedByName"),
            id: text("id"),
            manageable_state: text("manageableState"),
        });
    }
    Ok(out)
}

fn parse_describe(xml: &str) -> Result<Vec<MetadataObject>> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| Error::Decode(e.to_string()))?;
    let mut out = Vec::new();
    for o in doc
        .descendants()
        .filter(|n| n.has_tag_name("metadataObjects"))
    {
        let texts = |tag: &str| -> Vec<String> {
            o.children()
                .filter(|n| n.has_tag_name(tag))
                .filter_map(|n| n.text())
                .map(|t| t.trim().to_owned())
                .collect()
        };
        out.push(MetadataObject {
            xml_name: texts("xmlName").into_iter().next().unwrap_or_default(),
            in_folder: texts("inFolder").first().map(String::as_str) == Some("true"),
            child_xml_names: texts("childXmlNames"),
        });
    }
    Ok(out)
}

fn first_text(xml: &str, tag: &str) -> Option<String> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    doc.descendants()
        .find(|n| n.has_tag_name(tag))
        .and_then(|n| n.text())
        .map(|t| t.trim().to_owned())
}

fn truncate(s: &str) -> String {
    s.chars().take(300).collect()
}

fn parse_retrieve(xml: &str) -> Result<(RetrieveStatus, Retrieved)> {
    use base64::Engine;
    let doc = roxmltree::Document::parse(xml).map_err(|e| Error::Decode(e.to_string()))?;
    let result = doc
        .descendants()
        .find(|n| n.has_tag_name("result"))
        .ok_or_else(|| Error::Decode(format!("checkRetrieveStatus: {}", truncate(xml))))?;
    let text = |tag: &str| {
        result
            .children()
            .find(|n| n.has_tag_name(tag))
            .and_then(|n| n.text())
            .unwrap_or_default()
            .trim()
            .to_owned()
    };
    let mut out = Retrieved::default();
    for m in result.children().filter(|n| n.has_tag_name("messages")) {
        let field = |tag: &str| {
            m.children()
                .find(|n| n.has_tag_name(tag))
                .and_then(|n| n.text())
                .unwrap_or_default()
                .trim()
                .to_owned()
        };
        out.messages
            .push(format!("{}: {}", field("fileName"), field("problem")));
    }
    if text("done") != "true" {
        return Ok((RetrieveStatus::Running, out));
    }
    let status = text("status");
    if status != "Succeeded" {
        let reason = match text("errorMessage") {
            m if m.is_empty() => status,
            m => format!("{status}: {m}"),
        };
        return Ok((RetrieveStatus::Done(Err(reason)), out));
    }
    let zip: String = text("zipFile").split_whitespace().collect();
    out.zip = base64::engine::general_purpose::STANDARD
        .decode(zip)
        .map_err(|e| Error::Decode(format!("retrieve zip: {e}")))?;
    Ok((RetrieveStatus::Done(Ok(())), out))
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_list_metadata_response() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<soapenv:Envelope xmlns:soapenv="http://schemas.xmlsoap.org/soap/envelope/" xmlns="http://soap.sforce.com/2006/04/metadata">
<soapenv:Body><listMetadataResponse>
<result><createdById>005A</createdById><createdByName>Ann</createdByName><createdDate>2026-01-01T00:00:00.000Z</createdDate>
<fileName>classes/A.cls</fileName><fullName>A</fullName><id>01pA</id><lastModifiedById>005B</lastModifiedById>
<lastModifiedByName>Bob</lastModifiedByName><lastModifiedDate>2026-10-11T12:00:03.000Z</lastModifiedDate>
<manageableState>unmanaged</manageableState><type>ApexClass</type></result>
<result><fullName>Account.Foo__c</fullName><type>CustomField</type><lastModifiedDate>2026-10-10T00:00:00.000Z</lastModifiedDate></result>
</listMetadataResponse></soapenv:Body></soapenv:Envelope>"#;
        let r = parse_list(xml).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(
            (
                r[0].kind.as_str(),
                r[0].full_name.as_str(),
                r[0].last_modified_by_name.as_str()
            ),
            ("ApexClass", "A", "Bob")
        );
        assert_eq!(r[1].full_name, "Account.Foo__c");
        assert!(fault(xml).is_none());
    }

    #[test]
    fn parses_describe_metadata() {
        let xml = r#"<Envelope><Body><describeMetadataResponse><result>
<metadataObjects><childXmlNames>CustomField</childXmlNames><childXmlNames>ListView</childXmlNames><directoryName>objects</directoryName><inFolder>false</inFolder><metaFile>false</metaFile><suffix>object</suffix><xmlName>CustomObject</xmlName></metadataObjects>
<metadataObjects><directoryName>reports</directoryName><inFolder>true</inFolder><xmlName>Report</xmlName></metadataObjects>
</result></describeMetadataResponse></Body></Envelope>"#;
        let d = parse_describe(xml).unwrap();
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].child_xml_names, ["CustomField", "ListView"]);
        assert!(d[1].in_folder && d[1].xml_name == "Report");
    }

    #[test]
    fn parses_retrieve_status() {
        let running = r#"<Envelope><Body><checkRetrieveStatusResponse><result><done>false</done><id>09S1</id><status>InProgress</status></result></checkRetrieveStatusResponse></Body></Envelope>"#;
        assert_eq!(parse_retrieve(running).unwrap().0, RetrieveStatus::Running);
        let done = r#"<Envelope><Body><checkRetrieveStatusResponse><result><done>true</done><status>Succeeded</status>
<messages><fileName>unpackaged/package.xml</fileName><problem>Entity of type ApexClass named Nope cannot be found</problem></messages>
<zipFile>UEsF
BgAAAAAAAAAAAAAAAAAAAAAAAA==</zipFile></result></checkRetrieveStatusResponse></Body></Envelope>"#;
        let (status, r) = parse_retrieve(done).unwrap();
        assert_eq!(status, RetrieveStatus::Done(Ok(())));
        assert_eq!(&r.zip[..2], b"PK");
        assert!(r.messages[0].contains("cannot be found"));
        let failed = r#"<Envelope><Body><r><result><done>true</done><status>Failed</status><errorMessage>bad</errorMessage></result></r></Body></Envelope>"#;
        assert_eq!(
            parse_retrieve(failed).unwrap().0,
            RetrieveStatus::Done(Err("Failed: bad".into()))
        );
    }

    #[test]
    fn reads_faults() {
        let xml = r#"<soapenv:Envelope xmlns:soapenv="http://schemas.xmlsoap.org/soap/envelope/"><soapenv:Body><soapenv:Fault>
<faultcode>sf:INVALID_SESSION_ID</faultcode><faultstring>INVALID_SESSION_ID: Session expired</faultstring>
</soapenv:Fault></soapenv:Body></soapenv:Envelope>"#;
        let f = fault(xml).unwrap();
        assert_eq!(f.code, "sf:INVALID_SESSION_ID");
        assert!(f.message.contains("expired"));
    }
}
