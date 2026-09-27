use devcoordinator2_api::rate_card::{List, Mutation, Page, RateCard, Set};
use devcoordinator2_api::{ErrorCode, ProtocolError};
use rusqlite::{Connection, params};

use crate::database::{Database, DatabaseError};

const PAGE_LIMIT: u16 = 100;
const MAX_TEXT: usize = 200;
const RATE_CARD_REVISION_KEY: &str = "usage_rate_card_revision";

#[derive(Clone)]
pub struct RateCardService {
    database: Database,
}

#[derive(Clone, Debug)]
pub(crate) struct RateCardSnapshot {
    pub revision: u64,
    pub cards: Vec<RateCard>,
}

impl RateCardService {
    pub fn new(database: Database) -> Self {
        Self { database }
    }

    pub fn list(&self, params: List) -> Result<Page, ProtocolError> {
        self.database
            .call(move |connection| list_rows(connection, params))
            .map_err(database_error)
    }

    pub fn set(&self, params: Set, actor: &str, now: &str) -> Result<Mutation, ProtocolError> {
        validate_card(&params.card)?;
        let actor = actor.to_owned();
        let now = now.to_owned();
        self.database
            .transaction(move |connection| {
                let current = revision(connection)?;
                if current != params.expected_revision {
                    return Err(ProtocolError::new(
                        ErrorCode::ConfigurationConflict,
                        "rate-card catalog changed; refresh and retry",
                    )
                    .into());
                }
                let exists: bool = connection.query_row(
                    "SELECT EXISTS(SELECT 1 FROM usage_rate_cards WHERE card_id=?1 AND version=?2)",
                    params![params.card.card_id, i64::from(params.card.version)],
                    |row| row.get(0),
                )?;
                if exists {
                    return Err(ProtocolError::new(
                        ErrorCode::ConfigurationConflict,
                        "rate-card versions are immutable; choose a new version",
                    )
                    .into());
                }
                let card = params.card;
                let card_id = card.card_id.clone();
                let version = card.version;
                connection.execute(
                    "INSERT INTO usage_rate_cards(
                        card_id,version,provider,model_pattern,processing_tier,context_tier,
                        effective_from_ms,effective_to_ms,input_usd_micros_per_million,
                        cached_input_usd_micros_per_million,cache_write_usd_micros_per_million,
                        output_usd_micros_per_million,source_ref,active,created_at,created_by
                    ) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
                    params![
                        card.card_id,
                        i64::from(card.version),
                        card.provider,
                        card.model_pattern,
                        card.processing_tier,
                        card.context_tier,
                        i64_value(card.effective_from_ms)?,
                        card.effective_to_ms.map(i64_value).transpose()?,
                        i64_value(card.input_usd_micros_per_million)?,
                        i64_value(card.cached_input_usd_micros_per_million)?,
                        i64_value(card.cache_write_usd_micros_per_million)?,
                        i64_value(card.output_usd_micros_per_million)?,
                        card.source_ref,
                        i64::from(card.active),
                        now,
                        actor,
                    ],
                )?;
                let next = current.saturating_add(1);
                set_revision(connection, next)?;
                Ok(Mutation {
                    revision: next,
                    card_id,
                    version,
                })
            })
            .map_err(database_error)
    }
}

pub(crate) fn snapshot_rows(connection: &Connection) -> rusqlite::Result<RateCardSnapshot> {
    let revision = revision(connection)?;
    let mut statement = connection.prepare(
        "SELECT card_id,version,provider,model_pattern,processing_tier,context_tier,
                effective_from_ms,effective_to_ms,input_usd_micros_per_million,
                cached_input_usd_micros_per_million,cache_write_usd_micros_per_million,
                output_usd_micros_per_million,source_ref,active
         FROM usage_rate_cards WHERE active=1
         ORDER BY provider,model_pattern,processing_tier,context_tier,effective_from_ms,card_id,version",
    )?;
    let cards = statement
        .query_map([], decode_card)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(RateCardSnapshot { revision, cards })
}

fn list_rows(connection: &Connection, params: List) -> Result<Page, DatabaseError> {
    let revision = revision(connection)?;
    let limit = usize::from(params.limit.unwrap_or(PAGE_LIMIT).min(PAGE_LIMIT));
    let offset = usize::try_from(params.offset.unwrap_or(0)).unwrap_or(usize::MAX);
    let effective_at = params.effective_at_ms.map(i64_value).transpose()?;
    let mut sql = String::from(
        "SELECT card_id,version,provider,model_pattern,processing_tier,context_tier,
                effective_from_ms,effective_to_ms,input_usd_micros_per_million,
                cached_input_usd_micros_per_million,cache_write_usd_micros_per_million,
                output_usd_micros_per_million,source_ref,active
         FROM usage_rate_cards WHERE 1=1",
    );
    if !params.include_inactive {
        sql.push_str(" AND active=1");
    }
    if effective_at.is_some() {
        sql.push_str(
            " AND effective_from_ms<=?1 AND (effective_to_ms IS NULL OR effective_to_ms>?1)",
        );
    }
    sql.push_str(" ORDER BY provider,model_pattern,processing_tier,context_tier,effective_from_ms,card_id,version");
    let mut statement = connection.prepare(&sql)?;
    let rows = if let Some(effective_at) = effective_at {
        statement
            .query_map([effective_at], decode_card)?
            .collect::<Result<Vec<_>, _>>()?
    } else {
        statement
            .query_map([], decode_card)?
            .collect::<Result<Vec<_>, _>>()?
    };
    let total = rows.len();
    let cards = rows
        .into_iter()
        .skip(offset)
        .take(limit)
        .collect::<Vec<_>>();
    let next = offset.saturating_add(cards.len());
    Ok(Page {
        revision,
        cards,
        next_offset: (next < total).then_some(u32::try_from(next).unwrap_or(u32::MAX)),
    })
}

fn decode_card(row: &rusqlite::Row<'_>) -> rusqlite::Result<RateCard> {
    Ok(RateCard {
        card_id: row.get(0)?,
        version: row
            .get::<_, i64>(1)?
            .try_into()
            .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(1, 0))?,
        provider: row.get(2)?,
        model_pattern: row.get(3)?,
        processing_tier: row.get(4)?,
        context_tier: row.get(5)?,
        effective_from_ms: u64_value(row.get(6)?)?,
        effective_to_ms: row.get::<_, Option<i64>>(7)?.map(u64_value).transpose()?,
        input_usd_micros_per_million: u64_value(row.get(8)?)?,
        cached_input_usd_micros_per_million: u64_value(row.get(9)?)?,
        cache_write_usd_micros_per_million: u64_value(row.get(10)?)?,
        output_usd_micros_per_million: u64_value(row.get(11)?)?,
        source_ref: row.get(12)?,
        active: row.get::<_, i64>(13)? != 0,
    })
}

fn validate_card(card: &RateCard) -> Result<(), ProtocolError> {
    for (label, value) in [
        ("card id", card.card_id.as_str()),
        ("provider", card.provider.as_str()),
        ("model pattern", card.model_pattern.as_str()),
        ("processing tier", card.processing_tier.as_str()),
        ("context tier", card.context_tier.as_str()),
        ("source reference", card.source_ref.as_str()),
    ] {
        if value.trim().is_empty() || value.len() > MAX_TEXT {
            return Err(ProtocolError::new(
                ErrorCode::ParamsInvalid,
                format!("{label} must be between 1 and {MAX_TEXT} characters"),
            ));
        }
    }
    if card.version == 0
        || card
            .effective_to_ms
            .is_some_and(|end| end <= card.effective_from_ms)
    {
        return Err(ProtocolError::new(
            ErrorCode::ParamsInvalid,
            "rate-card version and effective dates are invalid",
        ));
    }
    Ok(())
}

fn revision(connection: &Connection) -> rusqlite::Result<u64> {
    connection
        .query_row(
            "SELECT COALESCE((SELECT value FROM meta WHERE key=?1),'0')",
            [RATE_CARD_REVISION_KEY],
            |row| row.get::<_, String>(0),
        )?
        .parse::<u64>()
        .map_err(|_| rusqlite::Error::InvalidQuery)
}

fn set_revision(connection: &Connection, revision: u64) -> rusqlite::Result<()> {
    connection.execute(
        "INSERT OR REPLACE INTO meta(key,value) VALUES(?1,?2)",
        params![RATE_CARD_REVISION_KEY, revision.to_string()],
    )?;
    Ok(())
}

fn i64_value(value: u64) -> Result<i64, ProtocolError> {
    i64::try_from(value).map_err(|_| {
        ProtocolError::new(
            ErrorCode::ParamsInvalid,
            "rate-card numeric value is too large",
        )
    })
}

fn u64_value(value: i64) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, value))
}

fn database_error(error: DatabaseError) -> ProtocolError {
    match error {
        DatabaseError::Domain(error) => error,
        other => ProtocolError::new(ErrorCode::InternalError, "rate-card catalog failed")
            .with_detail(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn card(version: u32, effective_from_ms: u64) -> RateCard {
        RateCard {
            card_id: "local-standard".into(),
            version,
            provider: "openai".into(),
            model_pattern: "gpt-custom*".into(),
            processing_tier: "standard".into(),
            context_tier: "short".into(),
            effective_from_ms,
            effective_to_ms: None,
            input_usd_micros_per_million: 1_000_000,
            cached_input_usd_micros_per_million: 100_000,
            cache_write_usd_micros_per_million: 1_250_000,
            output_usd_micros_per_million: 5_000_000,
            source_ref: "https://example.test/rates".into(),
            active: true,
        }
    }

    #[test]
    fn rate_cards_are_seeded_versioned_and_optimistically_concurrent() {
        let temporary = tempdir().unwrap();
        let database = Database::open(temporary.path().join("authority.sqlite3")).unwrap();
        let service = RateCardService::new(database);
        let seeded = service.list(List::default()).unwrap();
        assert_eq!(seeded.revision, 1);
        assert!(
            seeded
                .cards
                .iter()
                .any(|card| card.model_pattern == "gpt-6-sol")
        );

        let added = service
            .set(
                Set {
                    expected_revision: seeded.revision,
                    card: card(1, 1_000),
                },
                "uid:1",
                "2026-09-27T00:00:00Z",
            )
            .unwrap();
        assert_eq!(added.revision, 2);
        let effective = service
            .list(List {
                effective_at_ms: Some(1_000),
                ..Default::default()
            })
            .unwrap();
        assert!(
            effective
                .cards
                .iter()
                .any(|value| value.card_id == "local-standard")
        );

        let conflict = service.set(
            Set {
                expected_revision: added.revision,
                card: card(1, 1_000),
            },
            "uid:1",
            "2026-09-27T00:00:00Z",
        );
        assert_eq!(conflict.unwrap_err().code, ErrorCode::ConfigurationConflict);
    }
}
