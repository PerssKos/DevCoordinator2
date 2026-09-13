use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const MAX_WORK_CONTEXT_BYTES: usize = 2048;

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkContext {
    #[schemars(range(min = 1, max = 1))]
    pub version: u8,
    #[schemars(length(min = 1, max = 256))]
    pub native_project_id: String,
    #[schemars(length(min = 1, max = 256))]
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 256))]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 256))]
    pub operation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 256))]
    pub workstream_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 256))]
    pub outcome_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 256))]
    pub experiment_ref: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkSource {
    Environment,
    #[default]
    Request,
}

#[derive(Clone, Copy, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize)]
pub enum WorkDiagnostic {
    #[serde(rename = "work_context_invalid")]
    Invalid,
    #[serde(rename = "work_context_too_large")]
    TooLarge,
    #[serde(rename = "work_context_session_conflict")]
    SessionConflict,
}

impl WorkDiagnostic {
    pub fn code(self) -> &'static str {
        match self {
            Self::Invalid => "work_context_invalid",
            Self::TooLarge => "work_context_too_large",
            Self::SessionConflict => "work_context_session_conflict",
        }
    }
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkAttribution {
    pub context: Option<WorkContext>,
    pub source: WorkSource,
    pub diagnostic: Option<WorkDiagnostic>,
}

impl WorkContext {
    pub fn parse(raw: &str) -> Result<Self, WorkDiagnostic> {
        if raw.len() > MAX_WORK_CONTEXT_BYTES {
            return Err(WorkDiagnostic::TooLarge);
        }
        let context: Self = serde_json::from_str(raw).map_err(|_| WorkDiagnostic::Invalid)?;
        context.validate()?;
        Ok(context)
    }

    pub fn validate(&self) -> Result<(), WorkDiagnostic> {
        if self.version != 1
            || self.native_project_id.trim().is_empty()
            || self.thread_id.trim().is_empty()
            || [
                Some(&self.native_project_id),
                Some(&self.thread_id),
                self.turn_id.as_ref(),
                self.operation_id.as_ref(),
                self.workstream_id.as_ref(),
                self.outcome_id.as_ref(),
                self.experiment_ref.as_ref(),
            ]
            .into_iter()
            .flatten()
            .any(|value| value.len() > 256 || value.chars().any(char::is_control))
        {
            return Err(WorkDiagnostic::Invalid);
        }
        if serde_json::to_vec(self).map_or(true, |encoded| encoded.len() > MAX_WORK_CONTEXT_BYTES) {
            return Err(WorkDiagnostic::TooLarge);
        }
        Ok(())
    }
}

impl crate::ClientContext {
    pub fn work_attribution(&self) -> Option<WorkAttribution> {
        if self.work.is_none() && self.work_source.is_none() && self.work_diagnostic.is_none() {
            return None;
        }
        let mut context = self.work.clone();
        let mut diagnostic = self.work_diagnostic;
        if let Some(work) = &context {
            if let Err(error) = work.validate() {
                diagnostic = Some(error);
            } else if self
                .session
                .as_ref()
                .is_some_and(|session| session != &work.thread_id)
            {
                diagnostic = Some(WorkDiagnostic::SessionConflict);
            }
        }
        if diagnostic.is_some() {
            context = None;
        }
        Some(WorkAttribution {
            context,
            source: self.work_source.unwrap_or_default(),
            diagnostic,
        })
    }
}

pub(crate) fn sanitize_request(value: &mut serde_json::Value) {
    let Some(client) = value
        .get_mut("client")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };
    let mut diagnostic = None;
    if let Some(work) = client.get("work").filter(|work| !work.is_null()) {
        diagnostic = serde_json::to_string(work)
            .ok()
            .and_then(|encoded| WorkContext::parse(&encoded).err());
    }
    if let Some(source) = client.get("work_source").filter(|source| !source.is_null())
        && serde_json::from_value::<WorkSource>(source.clone()).is_err()
    {
        diagnostic = Some(WorkDiagnostic::Invalid);
        client.remove("work_source");
    }
    if let Some(code) = client.get("work_diagnostic").filter(|code| !code.is_null())
        && serde_json::from_value::<WorkDiagnostic>(code.clone()).is_err()
    {
        diagnostic = Some(WorkDiagnostic::Invalid);
    }
    if let Some(diagnostic) = diagnostic {
        client.remove("work");
        client.insert(
            "work_diagnostic".into(),
            serde_json::to_value(diagnostic).expect("diagnostic serialization"),
        );
    }
}

#[cfg(test)]
#[path = "work_context_tests.rs"]
mod tests;
