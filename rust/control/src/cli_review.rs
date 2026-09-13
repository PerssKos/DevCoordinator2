use super::*;
use std::io::Read;

#[derive(Debug, Subcommand)]
pub(super) enum ReviewCommand {
    Prepare {
        #[arg(long)]
        repository_id: String,
        #[arg(long)]
        workstream_id: Option<String>,
        #[arg(long)]
        window_start_ms: u64,
        #[arg(long)]
        window_end_ms: u64,
        #[arg(long, default_value_t = 0)]
        offset: u32,
        #[arg(long, default_value_t = 10)]
        limit: u8,
        #[arg(long)]
        before_decision_seq: Option<u32>,
    },
    Record {
        #[arg(long)]
        record_id: Option<String>,
        #[arg(long)]
        expected_revision: u32,
        #[arg(long)]
        file: PathBuf,
    },
    Show {
        reference: String,
    },
    List {
        #[arg(long)]
        repository_id: String,
        #[arg(long)]
        record_id: Option<String>,
        #[arg(long, default_value_t = 0)]
        offset: u32,
        #[arg(long, default_value_t = 10)]
        limit: u8,
    },
}

impl ReviewCommand {
    pub(super) fn into_invocation(self) -> Result<Invocation, CliValidationError> {
        match self {
            Self::Prepare {
                repository_id,
                workstream_id,
                window_start_ms,
                window_end_ms,
                offset,
                limit,
                before_decision_seq,
            } => remote(
                "review.prepare",
                json!({"repository_id":repository_id,"workstream_id":workstream_id,"window_start_ms":window_start_ms,"window_end_ms":window_end_ms,"offset":offset,"limit":limit,"before_decision_seq":before_decision_seq}),
            ),
            Self::Record {
                record_id,
                expected_revision,
                file,
            } => remote(
                "review.record",
                json!({"record_id":record_id,"expected_revision":expected_revision,"record":bounded_file(&file)?}),
            ),
            Self::Show { reference } => remote("review.receipt", json!({"reference":reference})),
            Self::List {
                repository_id,
                record_id,
                offset,
                limit,
            } => remote(
                "review.show",
                json!({"repository_id":repository_id,"record_id":record_id,"offset":offset,"limit":limit}),
            ),
        }
    }
}

pub(super) fn bounded_file(path: &PathBuf) -> Result<Value, CliValidationError> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(8193).read_to_end(&mut bytes))
        .map_err(|_| invalid("Cannot read review/evidence input"))?;
    if bytes.len() > 8192 {
        return Err(invalid("Review/evidence input exceeds 8 KiB"));
    }
    serde_json::from_slice(&bytes).map_err(|_| invalid("Invalid review/evidence JSON"))
}
