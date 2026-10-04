use super::*;
use devcoordinator2_api::work_context::{WorkContext, WorkDiagnostic, WorkSource};
use std::ffi::OsStr;

impl Cli {
    pub fn client_context(&self) -> ClientContext {
        let environment = std::env::var_os("DEVCOORDINATOR_WORK_CONTEXT");
        let mut context = self.client_context_with_work(environment.as_deref());
        // Kept outside the v1 work envelope so older Coordinator releases retain attribution.
        if let Some(work) = context.work.as_mut()
            && let Ok(capability) = std::env::var("CODEX_ALARM_CONTEXT")
            && capability.len() <= 512
        {
            work.alarm = serde_json::from_str(&capability).ok();
            if work.alarm.is_some()
                && let Some(path) = std::env::var_os("CODEX_ALARM_ACTIVATION")
            {
                use std::io::Write;
                let activation = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path);
                match activation {
                    Ok(mut file) => {
                        if file.write_all(b"codex.alarm-route.v1\n").is_err() {
                            work.alarm = None;
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(_) => {
                        work.alarm = None;
                    }
                }
                // Registration is advertised only after the existing runtime bridge
                // has consumed the activation request into its durable route store.
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
                let mut delay = std::time::Duration::from_millis(5);
                while work.alarm.is_some() && std::path::Path::new(&path).exists() {
                    if std::time::Instant::now() >= deadline {
                        work.alarm = None;
                        break;
                    }
                    std::thread::sleep(delay.min(std::time::Duration::from_millis(100)));
                    delay = (delay * 2).min(std::time::Duration::from_millis(100));
                }
            }
        }
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
            model: self.model.clone(),
            effort: self.effort.clone(),
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
