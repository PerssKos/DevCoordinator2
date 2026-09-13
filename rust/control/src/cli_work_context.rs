use super::*;
use devcoordinator2_api::work_context::{WorkContext, WorkDiagnostic, WorkSource};
use std::ffi::OsStr;

impl Cli {
    pub fn client_context(&self) -> ClientContext {
        let environment = std::env::var_os("DEVCOORDINATOR_WORK_CONTEXT");
        let context = self.client_context_with_work(environment.as_deref());
        if let Some(diagnostic) = context.work_diagnostic {
            eprintln!(
                "devcoordinator2: {} (attribution omitted)",
                diagnostic.code()
            );
        }
        context
    }

    fn client_context_with_work(&self, environment: Option<&OsStr>) -> ClientContext {
        let mut context = ClientContext {
            kind: self.client.into(),
            session: self.session.clone(),
            ..ClientContext::default()
        };
        let Some(environment) = environment else {
            return context;
        };
        context.work_source = Some(WorkSource::Environment);
        let parsed = environment
            .to_str()
            .ok_or(WorkDiagnostic::Invalid)
            .and_then(WorkContext::parse);
        match parsed {
            Ok(work)
                if self
                    .session
                    .as_ref()
                    .is_some_and(|session| session != &work.thread_id) =>
            {
                context.work_diagnostic = Some(WorkDiagnostic::SessionConflict)
            }
            Ok(work) => {
                context.session = self
                    .session
                    .clone()
                    .or_else(|| Some(work.thread_id.clone()));
                context.work = Some(work);
            }
            Err(diagnostic) => context.work_diagnostic = Some(diagnostic),
        }
        context
    }
}

#[cfg(test)]
#[path = "cli_work_context_tests.rs"]
mod tests;
