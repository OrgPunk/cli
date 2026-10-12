//! Metadata deploy via REST `metadata/deployRequest`.

use reqwest::blocking::multipart::{Form, Part};
use serde::{Deserialize, Serialize};

use crate::{Client, Error, Result, decode};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TestLevel {
    NoTestRun,
    RunSpecifiedTests,
    RunLocalTests,
    RunAllTestsInOrg,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeployOptions {
    pub check_only: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_level: Option<TestLevel>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub run_tests: Vec<String>,
    pub rollback_on_error: bool,
    pub single_package: bool,
    pub ignore_warnings: bool,
}

impl Default for DeployOptions {
    fn default() -> Self {
        Self {
            check_only: false,
            test_level: None,
            run_tests: Vec::new(),
            rollback_on_error: true,
            single_package: true,
            ignore_warnings: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeployStatus {
    Pending,
    InProgress,
    Succeeded,
    SucceededPartial,
    Failed,
    Canceling,
    Canceled,
}

impl DeployStatus {
    pub fn is_success(self) -> bool {
        self == Self::Succeeded
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeployResult {
    pub id: String,
    pub status: DeployStatus,
    #[serde(default, deserialize_with = "null_as_default")]
    pub done: bool,
    #[serde(default, deserialize_with = "null_as_default")]
    pub success: bool,
    #[serde(default, deserialize_with = "null_as_default")]
    pub check_only: bool,
    #[serde(default)]
    pub state_detail: Option<String>,
    #[serde(default)]
    pub error_message: Option<String>,
    #[serde(default)]
    pub error_status_code: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub number_components_total: u32,
    #[serde(default, deserialize_with = "null_as_default")]
    pub number_components_deployed: u32,
    #[serde(default, deserialize_with = "null_as_default")]
    pub number_component_errors: u32,
    #[serde(default, deserialize_with = "null_as_default")]
    pub number_tests_total: u32,
    #[serde(default, deserialize_with = "null_as_default")]
    pub number_tests_completed: u32,
    #[serde(default, deserialize_with = "null_as_default")]
    pub number_test_errors: u32,
    #[serde(default)]
    pub details: Option<DeployDetails>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeployDetails {
    #[serde(default)]
    pub component_failures: Vec<ComponentMessage>,
    #[serde(default)]
    pub run_test_result: Option<RunTestResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComponentMessage {
    #[serde(default)]
    pub component_type: String,
    pub full_name: String,
    #[serde(default)]
    pub file_name: String,
    #[serde(default)]
    pub problem: Option<String>,
    #[serde(default)]
    pub problem_type: Option<String>,
    #[serde(default)]
    pub line_number: Option<u32>,
    #[serde(default)]
    pub column_number: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunTestResult {
    #[serde(default)]
    pub num_tests_run: u32,
    #[serde(default)]
    pub num_failures: u32,
    #[serde(default)]
    pub failures: Vec<TestFailure>,
    #[serde(default)]
    pub code_coverage_warnings: Vec<CodeCoverageWarning>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TestFailure {
    pub name: String,
    pub method_name: String,
    pub message: String,
    #[serde(default)]
    pub stack_trace: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodeCoverageWarning {
    #[serde(default)]
    pub name: Option<String>,
    pub message: String,
}

/// Salesforce returns `null` instead of 0 and false until the deploy starts.
fn null_as_default<'de, D, T>(d: D) -> std::result::Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

#[derive(Deserialize)]
struct Envelope {
    #[serde(rename = "deployResult")]
    deploy_result: DeployResult,
}

impl Client {
    /// Sends the zip; returns the deploy id.
    pub fn deploy(&self, zip: Vec<u8>, options: &DeployOptions) -> Result<String> {
        let url = self.url("metadata/deployRequest");
        let json = serde_json::json!({ "deployOptions": options }).to_string();
        let resp = self.send(|h, _| {
            let form = Form::new()
                .part(
                    "json",
                    Part::text(json.clone())
                        .mime_str("application/json")
                        .expect("mime"),
                )
                .part(
                    "file",
                    Part::bytes(zip.clone())
                        .file_name("deploy.zip")
                        .mime_str("application/zip")
                        .expect("mime"),
                );
            h.post(&url).multipart(form)
        })?;
        let created: Envelope = decode(resp)?;
        Ok(created.deploy_result.id)
    }

    /// Quick deploy of a previously validated (checkOnly) deploy; returns the new deploy id.
    pub fn quick_deploy(&self, validated_id: &str) -> Result<String> {
        let url = self.url(&format!("metadata/deployRequest/{validated_id}"));
        let body = serde_json::json!({ "validatedDeployRequestId": validated_id });
        let created: Envelope = decode(self.send(|h, _| h.post(&url).json(&body))?)?;
        Ok(created.deploy_result.id)
    }

    pub fn deploy_status(&self, id: &str, include_details: bool) -> Result<DeployResult> {
        let path = format!("metadata/deployRequest/{id}?includeDetails={include_details}");
        let env: Envelope = self.get_json(&path)?;
        if env.deploy_result.id.is_empty() {
            return Err(Error::Decode(format!(
                "empty id in the status of deploy {id}"
            )));
        }
        Ok(env.deploy_result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(name: &str) -> DeployResult {
        let text = match name {
            "ok" => include_str!("../testdata/deploy-status-ok.json"),
            "test-failure" => include_str!("../testdata/deploy-status-test-failure.json"),
            "compile-error" => include_str!("../testdata/deploy-status-compile-error.json"),
            _ => unreachable!(),
        };
        serde_json::from_str::<Envelope>(text)
            .unwrap()
            .deploy_result
    }

    #[test]
    fn parses_success() {
        let r = status("ok");
        assert_eq!(r.status, DeployStatus::Succeeded);
        assert!(r.done && r.success && r.check_only);
        assert_eq!(r.number_tests_completed, 1);
    }

    #[test]
    fn parses_test_failure() {
        let r = status("test-failure");
        assert_eq!(r.status, DeployStatus::Failed);
        let tests = r.details.unwrap().run_test_result.unwrap();
        assert_eq!(tests.num_failures, 1);
        assert_eq!(tests.failures[0].name, "SfrSpikeGreeterTest");
        assert_eq!(tests.failures[0].method_name, "greets");
        assert!(
            tests.failures[0]
                .stack_trace
                .as_deref()
                .unwrap()
                .contains("line 5")
        );
    }

    #[test]
    fn parses_compile_error() {
        let r = status("compile-error");
        let failures = r.details.unwrap().component_failures;
        let broken: Vec<_> = failures
            .iter()
            .filter(|f| f.full_name == "SfrSpikeBroken")
            .collect();
        assert!(!broken.is_empty());
        assert!(
            broken
                .iter()
                .any(|f| f.line_number == Some(4) && f.file_name == "classes/SfrSpikeBroken.cls")
        );
    }

    #[test]
    fn parses_create_response() {
        let env: Envelope =
            serde_json::from_str(include_str!("../testdata/deploy-request.json")).unwrap();
        assert!(env.deploy_result.id.starts_with("0Af"));
    }

    #[test]
    fn options_serialize_like_salesforce() {
        let o = DeployOptions {
            check_only: true,
            test_level: Some(TestLevel::RunSpecifiedTests),
            run_tests: vec!["ATest".into()],
            ..Default::default()
        };
        let v = serde_json::to_value(&o).unwrap();
        assert_eq!(v["checkOnly"], true);
        assert_eq!(v["testLevel"], "RunSpecifiedTests");
        assert_eq!(v["runTests"][0], "ATest");
        assert_eq!(v["singlePackage"], true);
    }
}
