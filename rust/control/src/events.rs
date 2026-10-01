//! Durable owned-state events and one shared wait/deadline scheduler.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use devcoordinator2_api::params::{EventCategory, EventFilter, EventWait};
use devcoordinator2_api::results::{
    EventDelivery, EventWaitResult, HeartbeatDue, OwnedEvent, OwnedEventRecord,
};
use devcoordinator2_api::{ErrorCode, ProtocolError};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::sync::oneshot;

use crate::database::{Database, DatabaseError};

const MAX_FILTERS: usize = 32;
const MAX_BATCH: usize = 100;
const MAX_BATCH_BYTES: usize = 220 * 1024;
const MAX_SUBSCRIPTIONS: usize = 256;
const MAX_RETAINED_EVENTS: usize = 1_024;
const MAX_EVENT_BYTES: usize = 8_192;
const MAX_KIND_BYTES: usize = 96;
const COMMAND_CAPACITY: usize = 512;

pub type VisibilityProvider =
    Arc<dyn Fn() -> Result<EventVisibility, ProtocolError> + Send + Sync + 'static>;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EventVisibility {
    pub unrestricted: bool,
    pub repository_ids: BTreeSet<String>,
    pub deployment_ids: BTreeSet<String>,
}

impl EventVisibility {
    pub fn unrestricted() -> Self {
        Self {
            unrestricted: true,
            ..Self::default()
        }
    }
}

#[derive(Clone, Debug)]
pub struct NewEvent {
    pub occurred_at: String,
    pub event: OwnedEvent,
    pub dedupe_key: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PublishReceipt {
    pub cursor: u64,
    pub duplicate: bool,
}

#[derive(Clone)]
pub struct EventService {
    database: Database,
    dispatcher: Arc<Dispatcher>,
    next_subscription_id: Arc<AtomicU64>,
}

struct Dispatcher {
    sender: mpsc::SyncSender<Command>,
}

impl Drop for Dispatcher {
    fn drop(&mut self) {
        let _ = self.sender.send(Command::Stop);
    }
}

#[derive(Debug)]
pub struct EventSubscription {
    id: u64,
    receiver: Option<oneshot::Receiver<Result<EventWaitResult, ProtocolError>>>,
    sender: mpsc::SyncSender<Command>,
}

impl EventSubscription {
    pub async fn receive(mut self) -> Result<EventWaitResult, ProtocolError> {
        let receiver = self.receiver.take().ok_or_else(|| {
            ProtocolError::new(
                ErrorCode::InternalError,
                "event subscription was already consumed",
            )
        })?;
        receiver.await.map_err(|_| {
            ProtocolError::new(
                ErrorCode::DaemonUnavailable,
                "event scheduler is unavailable",
            )
        })?
    }

    #[cfg(test)]
    pub fn blocking_receive(mut self) -> Result<EventWaitResult, ProtocolError> {
        let receiver = self.receiver.take().ok_or_else(|| {
            ProtocolError::new(
                ErrorCode::InternalError,
                "event subscription was already consumed",
            )
        })?;
        receiver.blocking_recv().map_err(|_| {
            ProtocolError::new(
                ErrorCode::DaemonUnavailable,
                "event scheduler is unavailable",
            )
        })?
    }
}

impl Drop for EventSubscription {
    fn drop(&mut self) {
        let _ = self.sender.send(Command::Cancel(self.id));
    }
}

enum Command {
    Register(Subscription),
    Published,
    Cancel(u64),
    Stop,
}

struct Subscription {
    id: u64,
    cursor: Option<u64>,
    filters: Vec<PreparedFilter>,
    limit: usize,
    visibility: EventVisibility,
    visibility_provider: VisibilityProvider,
    response: oneshot::Sender<Result<EventWaitResult, ProtocolError>>,
}

#[derive(Clone)]
struct PreparedFilter {
    filter: EventFilter,
    deadline: Option<Instant>,
}

impl EventService {
    pub fn new(database: Database) -> Result<Self, ProtocolError> {
        let (sender, receiver) = mpsc::sync_channel(COMMAND_CAPACITY);
        let scheduler_database = database.clone();
        thread::Builder::new()
            .name("devcoordinator2-event-scheduler".to_owned())
            .spawn(move || scheduler_loop(scheduler_database, receiver))
            .map_err(|error| {
                ProtocolError::new(ErrorCode::InternalError, "cannot start event scheduler")
                    .with_detail(error.to_string())
            })?;
        Ok(Self {
            database,
            dispatcher: Arc::new(Dispatcher { sender }),
            next_subscription_id: Arc::new(AtomicU64::new(1)),
        })
    }

    pub fn publish(&self, event: NewEvent) -> Result<PublishReceipt, ProtocolError> {
        let receipt = append_event(&self.database, event)?;
        self.dispatcher
            .sender
            .send(Command::Published)
            .map_err(|_| {
                ProtocolError::new(
                    ErrorCode::DaemonUnavailable,
                    "event scheduler is unavailable",
                )
            })?;
        Ok(receipt)
    }

    pub fn subscribe(
        &self,
        request: EventWait,
        visibility: EventVisibility,
    ) -> Result<EventSubscription, ProtocolError> {
        let provider: VisibilityProvider = Arc::new(move || Ok(visibility.clone()));
        self.subscribe_with_visibility(request, provider)
    }

    pub fn subscribe_with_visibility(
        &self,
        mut request: EventWait,
        visibility_provider: VisibilityProvider,
    ) -> Result<EventSubscription, ProtocolError> {
        let visibility = visibility_provider()?;
        let filters = prepare_filters(request.filters, &visibility)?;
        if !(1..=MAX_BATCH).contains(&usize::from(request.limit)) {
            return Err(ProtocolError::new(
                ErrorCode::ParamsInvalid,
                "event wait limit must be between 1 and 100",
            ));
        }
        if request.cursor.is_none() {
            request.cursor = Some(history_head(&self.database)?);
        }
        let id = self.next_subscription_id.fetch_add(1, Ordering::Relaxed);
        let (response, receiver) = oneshot::channel();
        self.dispatcher
            .sender
            .try_send(Command::Register(Subscription {
                id,
                cursor: request.cursor,
                filters,
                limit: usize::from(request.limit),
                visibility,
                visibility_provider,
                response,
            }))
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => ProtocolError::new(
                    ErrorCode::Busy,
                    "event wait scheduler backpressure limit reached",
                ),
                mpsc::TrySendError::Disconnected(_) => ProtocolError::new(
                    ErrorCode::DaemonUnavailable,
                    "event scheduler is unavailable",
                ),
            })?;
        Ok(EventSubscription {
            id,
            receiver: Some(receiver),
            sender: self.dispatcher.sender.clone(),
        })
    }
}

fn history_head(database: &Database) -> Result<u64, ProtocolError> {
    database
        .call(|connection| {
            let head = connection.query_row(
                "SELECT COALESCE(MAX(cursor),0) FROM owned_events",
                [],
                |row| row.get::<_, i64>(0),
            )?;
            u64::try_from(head).map_err(|_| {
                DatabaseError::Domain(ProtocolError::new(
                    ErrorCode::InternalError,
                    "owned event history head is invalid",
                ))
            })
        })
        .map_err(database_error)
}

fn prepare_filters(
    filters: Vec<EventFilter>,
    visibility: &EventVisibility,
) -> Result<Vec<PreparedFilter>, ProtocolError> {
    if filters.is_empty() || filters.len() > MAX_FILTERS {
        return Err(ProtocolError::new(
            ErrorCode::ParamsInvalid,
            "event wait requires between 1 and 32 filters",
        ));
    }
    let mut identifiers = HashSet::new();
    let now_wall = OffsetDateTime::now_utc();
    let now_mono = Instant::now();
    filters
        .into_iter()
        .map(|filter| {
            if !valid_filter_id(&filter.filter_id) || !identifiers.insert(filter.filter_id.clone())
            {
                return Err(ProtocolError::new(
                    ErrorCode::ParamsInvalid,
                    "event filter ids must be unique safe identifiers",
                ));
            }
            if filter.categories.len() > 6
                || filter.kinds.len() > 32
                || filter.repository_ids.len() > 32
                || filter.deployment_ids.len() > 32
                || filter.kinds.iter().any(|kind| !valid_kind(kind))
                || filter
                    .repository_ids
                    .iter()
                    .any(|identifier| !valid_domain_id(identifier, 'r'))
                || filter
                    .deployment_ids
                    .iter()
                    .any(|identifier| !valid_domain_id(identifier, 'd'))
            {
                return Err(ProtocolError::new(
                    ErrorCode::ParamsInvalid,
                    "event filter values are invalid or exceed their bounds",
                ));
            }
            if !visibility.unrestricted
                && (filter
                    .repository_ids
                    .iter()
                    .any(|id| !visibility.repository_ids.contains(id))
                    || filter
                        .deployment_ids
                        .iter()
                        .any(|id| !visibility.deployment_ids.contains(id)))
            {
                return Err(ProtocolError::new(
                    ErrorCode::PermissionDenied,
                    "event filter requests an unauthorized scope",
                ));
            }
            let deadline = filter
                .deadline_at
                .as_deref()
                .map(|value| {
                    let parsed = OffsetDateTime::parse(value, &Rfc3339).map_err(|_| {
                        ProtocolError::new(
                            ErrorCode::ParamsInvalid,
                            "event filter deadline_at must be an RFC 3339 timestamp",
                        )
                    })?;
                    let delay = if parsed <= now_wall {
                        Duration::ZERO
                    } else {
                        Duration::try_from(parsed - now_wall).map_err(|_| {
                            ProtocolError::new(
                                ErrorCode::ParamsInvalid,
                                "event filter deadline is outside the supported clock range",
                            )
                        })?
                    };
                    now_mono.checked_add(delay).ok_or_else(|| {
                        ProtocolError::new(
                            ErrorCode::ParamsInvalid,
                            "event filter deadline is outside the supported clock range",
                        )
                    })
                })
                .transpose()?;
            Ok(PreparedFilter { filter, deadline })
        })
        .collect()
}

fn scheduler_loop(database: Database, receiver: mpsc::Receiver<Command>) {
    let mut subscriptions = HashMap::<u64, Subscription>::new();
    loop {
        let command = match next_deadline(&subscriptions) {
            Some(deadline) => {
                match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(command) => Some(command),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            None => match receiver.recv() {
                Ok(command) => Some(command),
                Err(_) => break,
            },
        };
        match command {
            Some(Command::Register(mut subscription)) => {
                if subscriptions.len() >= MAX_SUBSCRIPTIONS {
                    let _ = subscription.response.send(Err(ProtocolError::new(
                        ErrorCode::Busy,
                        "event wait subscription limit reached",
                    )));
                    continue;
                }
                match evaluate(&database, &mut subscription, Instant::now()) {
                    Ok(Some(result)) => {
                        let _ = subscription.response.send(Ok(result));
                    }
                    Ok(None) => {
                        subscriptions.insert(subscription.id, subscription);
                    }
                    Err(error) => {
                        let _ = subscription.response.send(Err(error));
                    }
                }
            }
            Some(Command::Published) | None => {
                evaluate_all(&database, &mut subscriptions);
            }
            Some(Command::Cancel(id)) => {
                subscriptions.remove(&id);
            }
            Some(Command::Stop) => break,
        }
    }
}

fn next_deadline(subscriptions: &HashMap<u64, Subscription>) -> Option<Instant> {
    subscriptions
        .values()
        .flat_map(|subscription| subscription.filters.iter())
        .filter_map(|filter| filter.deadline)
        .min()
}

fn evaluate_all(database: &Database, subscriptions: &mut HashMap<u64, Subscription>) {
    let identifiers = subscriptions.keys().copied().collect::<Vec<_>>();
    for id in identifiers {
        let Some(mut subscription) = subscriptions.remove(&id) else {
            continue;
        };
        match evaluate(database, &mut subscription, Instant::now()) {
            Ok(Some(result)) => {
                let _ = subscription.response.send(Ok(result));
            }
            Ok(None) => {
                subscriptions.insert(id, subscription);
            }
            Err(error) => {
                let _ = subscription.response.send(Err(error));
            }
        }
    }
}

fn evaluate(
    database: &Database,
    subscription: &mut Subscription,
    now: Instant,
) -> Result<Option<EventWaitResult>, ProtocolError> {
    let visibility = (subscription.visibility_provider)()?;
    authorize_prepared_filters(&subscription.filters, &visibility)?;
    subscription.visibility = visibility;
    let (_, history_head, records) = load_events(
        database,
        subscription.cursor,
        subscription.visibility.unrestricted,
    )?;
    let mut events = Vec::new();
    let mut event_bytes = 0_usize;
    for event in records {
        if !subscription.visibility.allows(&event) {
            continue;
        }
        let filter_ids = subscription
            .filters
            .iter()
            .filter(|filter| matches_filter(&filter.filter, &event))
            .map(|filter| filter.filter.filter_id.clone())
            .collect::<Vec<_>>();
        if !filter_ids.is_empty() {
            let delivery = EventDelivery { filter_ids, event };
            let delivery_bytes = serde_json::to_vec(&delivery)
                .map_err(|error| {
                    ProtocolError::new(ErrorCode::InternalError, "cannot encode owned event")
                        .with_detail(error.to_string())
                })?
                .len();
            if !events.is_empty() && event_bytes.saturating_add(delivery_bytes) > MAX_BATCH_BYTES {
                break;
            }
            event_bytes = event_bytes.saturating_add(delivery_bytes);
            events.push(delivery);
            if events.len() == subscription.limit {
                break;
            }
        }
    }
    let satisfied = events
        .iter()
        .flat_map(|delivery| delivery.filter_ids.iter().cloned())
        .collect::<HashSet<_>>();
    let heartbeat_due = subscription
        .filters
        .iter()
        .filter(|filter| !satisfied.contains(&filter.filter.filter_id))
        .filter(|filter| filter.deadline.is_some_and(|deadline| deadline <= now))
        .filter_map(|filter| {
            filter
                .filter
                .deadline_at
                .clone()
                .map(|deadline_at| HeartbeatDue {
                    filter_id: filter.filter.filter_id.clone(),
                    deadline_at,
                })
        })
        .collect::<Vec<_>>();
    if events.is_empty() && heartbeat_due.is_empty() {
        subscription.cursor = Some(history_head);
        return Ok(None);
    }
    let cursor = events
        .last()
        .map_or(history_head, |delivery| delivery.event.cursor);
    Ok(Some(EventWaitResult {
        cursor,
        events,
        heartbeat_due,
    }))
}

fn authorize_prepared_filters(
    filters: &[PreparedFilter],
    visibility: &EventVisibility,
) -> Result<(), ProtocolError> {
    if !visibility.unrestricted
        && filters.iter().any(|prepared| {
            prepared
                .filter
                .repository_ids
                .iter()
                .any(|id| !visibility.repository_ids.contains(id))
                || prepared
                    .filter
                    .deployment_ids
                    .iter()
                    .any(|id| !visibility.deployment_ids.contains(id))
        })
    {
        return Err(ProtocolError::new(
            ErrorCode::PermissionDenied,
            "event filter requests an unauthorized scope",
        ));
    }
    Ok(())
}

fn append_event(database: &Database, event: NewEvent) -> Result<PublishReceipt, ProtocolError> {
    validate_new_event(&event)?;
    let category = category_name(category(&event.event)).to_owned();
    let kind = kind(&event.event).to_owned();
    let repository_id = repository_id(&event.event).map(str::to_owned);
    let deployment_id = deployment_id(&event.event).map(str::to_owned);
    let payload = serde_json::to_string(&event.event).map_err(|error| {
        ProtocolError::new(ErrorCode::InternalError, "cannot encode owned event")
            .with_detail(error.to_string())
    })?;
    let occurred_at = event.occurred_at;
    let dedupe_key = event.dedupe_key;
    database
        .transaction(move |transaction| {
            let changed = transaction.execute(
                "INSERT INTO owned_events(occurred_at,category,kind,repository_id,deployment_id,payload_json,dedupe_key) \
                 VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(dedupe_key) DO NOTHING",
                rusqlite::params![
                    occurred_at,
                    category,
                    kind,
                    repository_id,
                    deployment_id,
                    payload,
                    dedupe_key
                ],
            )?;
            let cursor = if changed == 0 {
                let stored = transaction.query_row(
                    "SELECT cursor FROM owned_events WHERE dedupe_key=?1",
                    [dedupe_key.as_deref()],
                    |row| row.get::<_, i64>(0),
                )?;
                u64::try_from(stored).map_err(|_| {
                    DatabaseError::Domain(ProtocolError::new(
                        ErrorCode::InternalError,
                        "stored owned event cursor is invalid",
                    ))
                })?
            } else {
                u64::try_from(transaction.last_insert_rowid()).map_err(|_| {
                    DatabaseError::Domain(ProtocolError::new(
                        ErrorCode::InternalError,
                        "owned event cursor is invalid",
                    ))
                })?
            };
            transaction.execute(
                "DELETE FROM owned_events WHERE cursor < COALESCE(\
                   (SELECT cursor FROM owned_events ORDER BY cursor DESC LIMIT 1 OFFSET ?1), 0)",
                [i64::try_from(MAX_RETAINED_EVENTS - 1).expect("retention fits i64")],
            )?;
            Ok(PublishReceipt {
                cursor,
                duplicate: changed == 0,
            })
        })
        .map_err(database_error)
}

fn load_events(
    database: &Database,
    cursor: Option<u64>,
    disclose_bounds: bool,
) -> Result<(u64, u64, Vec<OwnedEventRecord>), ProtocolError> {
    let database_cursor = cursor
        .map(i64::try_from)
        .transpose()
        .map_err(|_| ProtocolError::new(ErrorCode::ParamsInvalid, "event cursor is too large"))?;
    database
        .call(move |connection| {
            let (floor, head) = connection.query_row(
                "SELECT COALESCE(MIN(cursor),0),COALESCE(MAX(cursor),0) FROM owned_events",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )?;
            let floor = u64::try_from(floor).map_err(|_| {
                DatabaseError::Domain(ProtocolError::new(
                    ErrorCode::InternalError,
                    "owned event history floor is invalid",
                ))
            })?;
            let head = u64::try_from(head).map_err(|_| {
                DatabaseError::Domain(ProtocolError::new(
                    ErrorCode::InternalError,
                    "owned event history head is invalid",
                ))
            })?;
            let Some(database_cursor) = database_cursor else {
                return Ok((floor, head, Vec::new()));
            };
            let cursor = u64::try_from(database_cursor).expect("validated cursor");
            if cursor > head {
                return Err(DatabaseError::Domain(
                    ProtocolError::new(
                        ErrorCode::CursorStale,
                        "event cursor is ahead of retained history",
                    )
                    .with_detail(if disclose_bounds {
                        format!("history_floor={floor}; history_head={head}")
                    } else {
                        String::new()
                    }),
                ));
            }
            if floor > 0 && cursor.saturating_add(1) < floor {
                return Err(DatabaseError::Domain(
                    ProtocolError::new(
                        ErrorCode::CursorStale,
                        "event cursor is older than retained history",
                    )
                    .with_detail(if disclose_bounds {
                        format!("history_floor={floor}; history_head={head}")
                    } else {
                        String::new()
                    }),
                ));
            }
            let mut statement = connection.prepare(
                "SELECT cursor,occurred_at,category,kind,repository_id,deployment_id,payload_json \
                 FROM owned_events WHERE cursor>?1 ORDER BY cursor LIMIT ?2",
            )?;
            let rows = statement.query_map(
                rusqlite::params![database_cursor, i64::try_from(MAX_RETAINED_EVENTS).unwrap()],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, String>(6)?,
                    ))
                },
            )?;
            let mut events = Vec::new();
            for row in rows {
                let (
                    cursor,
                    occurred_at,
                    stored_category,
                    stored_kind,
                    stored_repository,
                    stored_deployment,
                    payload,
                ) = row?;
                let cursor = u64::try_from(cursor).map_err(|_| {
                    DatabaseError::Domain(ProtocolError::new(
                        ErrorCode::InternalError,
                        "stored owned event cursor is invalid",
                    ))
                })?;
                let event = serde_json::from_str::<OwnedEvent>(&payload).map_err(|error| {
                    DatabaseError::Domain(
                        ProtocolError::new(
                            ErrorCode::InternalError,
                            "stored owned event is invalid",
                        )
                        .with_detail(error.to_string()),
                    )
                })?;
                if stored_category != category_name(category(&event))
                    || stored_kind != kind(&event)
                    || stored_repository.as_deref() != repository_id(&event)
                    || stored_deployment.as_deref() != deployment_id(&event)
                {
                    return Err(DatabaseError::Domain(ProtocolError::new(
                        ErrorCode::InternalError,
                        "stored owned event metadata does not match its typed payload",
                    )));
                }
                events.push(OwnedEventRecord {
                    cursor,
                    occurred_at,
                    event,
                });
            }
            Ok((floor, head, events))
        })
        .map_err(database_error)
}

fn validate_new_event(event: &NewEvent) -> Result<(), ProtocolError> {
    OffsetDateTime::parse(&event.occurred_at, &Rfc3339).map_err(|_| {
        ProtocolError::new(
            ErrorCode::ParamsInvalid,
            "owned event occurred_at must be an RFC 3339 timestamp",
        )
    })?;
    if !valid_kind(kind(&event.event))
        || event
            .dedupe_key
            .as_ref()
            .is_some_and(|key| key.is_empty() || key.len() > 160 || !key.is_ascii())
    {
        return Err(ProtocolError::new(
            ErrorCode::ParamsInvalid,
            "owned event kind or dedupe key is invalid",
        ));
    }
    let payload = serde_json::to_vec(&event.event).map_err(|error| {
        ProtocolError::new(ErrorCode::InternalError, "cannot encode owned event")
            .with_detail(error.to_string())
    })?;
    if payload.len() > MAX_EVENT_BYTES {
        return Err(ProtocolError::new(
            ErrorCode::ParamsInvalid,
            "owned event exceeds 8 KiB",
        ));
    }
    Ok(())
}

impl EventVisibility {
    fn allows(&self, event: &OwnedEventRecord) -> bool {
        if self.unrestricted {
            return true;
        }
        if matches!(event.event, OwnedEvent::Feedback(_))
            || matches!(&event.event, OwnedEvent::Other(other) if other.review.is_some())
        {
            return false;
        }
        if let Some(deployment_id) = deployment_id(&event.event)
            && self.deployment_ids.contains(deployment_id)
        {
            return true;
        }
        repository_id(&event.event)
            .is_some_and(|repository_id| self.repository_ids.contains(repository_id))
    }
}

fn matches_filter(filter: &EventFilter, record: &OwnedEventRecord) -> bool {
    (filter.categories.is_empty() || filter.categories.contains(&category(&record.event)))
        && (filter.kinds.is_empty()
            || filter
                .kinds
                .iter()
                .any(|value| value == kind(&record.event)))
        && (filter.repository_ids.is_empty()
            || repository_id(&record.event)
                .is_some_and(|value| filter.repository_ids.iter().any(|id| id == value)))
        && (filter.deployment_ids.is_empty()
            || deployment_id(&record.event)
                .is_some_and(|value| filter.deployment_ids.iter().any(|id| id == value)))
}

pub fn category(event: &OwnedEvent) -> EventCategory {
    match event {
        OwnedEvent::Test(_) => EventCategory::Test,
        OwnedEvent::Deployment(_) => EventCategory::Deployment,
        OwnedEvent::Planning(_) => EventCategory::Planning,
        OwnedEvent::Health(_) => EventCategory::Health,
        OwnedEvent::Feedback(_) => EventCategory::Feedback,
        OwnedEvent::Other(_) => EventCategory::Other,
    }
}

pub fn kind(event: &OwnedEvent) -> &str {
    match event {
        OwnedEvent::Test(event) => &event.kind,
        OwnedEvent::Deployment(event) => &event.kind,
        OwnedEvent::Planning(event) => &event.kind,
        OwnedEvent::Health(event) => &event.kind,
        OwnedEvent::Feedback(event) => &event.kind,
        OwnedEvent::Other(event) => &event.kind,
    }
}

pub fn repository_id(event: &OwnedEvent) -> Option<&str> {
    match event {
        OwnedEvent::Test(event) => Some(&event.repository_id),
        OwnedEvent::Deployment(event) => event.repository_id.as_deref(),
        OwnedEvent::Planning(event) => Some(&event.repository_id),
        OwnedEvent::Health(event) => event.repository_id.as_deref(),
        OwnedEvent::Feedback(event) => Some(&event.repository_id),
        OwnedEvent::Other(event) => event.repository_id.as_deref(),
    }
}

pub fn deployment_id(event: &OwnedEvent) -> Option<&str> {
    match event {
        OwnedEvent::Deployment(event) => Some(&event.deployment_id),
        OwnedEvent::Health(event) => event.deployment_id.as_deref(),
        OwnedEvent::Other(event) => event.deployment_id.as_deref(),
        OwnedEvent::Test(_) | OwnedEvent::Planning(_) | OwnedEvent::Feedback(_) => None,
    }
}

fn category_name(category: EventCategory) -> &'static str {
    match category {
        EventCategory::Test => "test",
        EventCategory::Deployment => "deployment",
        EventCategory::Planning => "planning",
        EventCategory::Health => "health",
        EventCategory::Feedback => "feedback",
        EventCategory::Other => "other",
    }
}

fn valid_filter_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn valid_kind(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_KIND_BYTES
        && value.as_bytes()[0].is_ascii_lowercase()
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
}

fn valid_domain_id(value: &str, prefix: char) -> bool {
    value.len() == 17
        && value.starts_with(prefix)
        && value[1..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn database_error(error: DatabaseError) -> ProtocolError {
    match error {
        DatabaseError::Domain(error) => error,
        other => ProtocolError::new(ErrorCode::InternalError, "owned event journal failed")
            .with_detail(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use devcoordinator2_api::results::{DeploymentOwnedEvent, HealthOwnedEvent, OtherOwnedEvent};
    use std::sync::Mutex;
    use tempfile::tempdir;

    const REPOSITORY_A: &str = "r1111111111111111";
    const REPOSITORY_B: &str = "r2222222222222222";
    const DEPLOYMENT_A: &str = "d1111111111111111";

    fn service() -> (tempfile::TempDir, Database, EventService) {
        let temporary = tempdir().unwrap();
        let database = Database::open(temporary.path().join("authority.sqlite3")).unwrap();
        let service = EventService::new(database.clone()).unwrap();
        (temporary, database, service)
    }

    fn other(kind: &str, repository_id: Option<&str>, occurred_at: &str) -> NewEvent {
        NewEvent {
            occurred_at: occurred_at.to_owned(),
            event: OwnedEvent::Other(OtherOwnedEvent {
                review: None,
                kind: kind.to_owned(),
                repository_id: repository_id.map(str::to_owned),
                deployment_id: None,
                subject_kind: "fixture".to_owned(),
                subject_id: "bounded".to_owned(),
            }),
            dedupe_key: None,
        }
    }

    fn deployment(kind: &str, occurred_at: &str) -> NewEvent {
        NewEvent {
            occurred_at: occurred_at.to_owned(),
            event: OwnedEvent::Deployment(DeploymentOwnedEvent {
                kind: kind.to_owned(),
                repository_id: Some(REPOSITORY_A.to_owned()),
                deployment_id: DEPLOYMENT_A.to_owned(),
                component: None,
                state: Some("running".to_owned()),
            }),
            dedupe_key: None,
        }
    }

    fn filter(filter_id: &str, category: EventCategory) -> EventFilter {
        EventFilter {
            filter_id: filter_id.to_owned(),
            categories: vec![category],
            kinds: Vec::new(),
            repository_ids: Vec::new(),
            deployment_ids: Vec::new(),
            deadline_at: None,
        }
    }

    fn wait(cursor: Option<u64>, filters: Vec<EventFilter>) -> EventWait {
        EventWait {
            cursor,
            filters,
            limit: 100,
        }
    }

    fn timestamp(offset_milliseconds: i64) -> String {
        (OffsetDateTime::now_utc() + time::Duration::milliseconds(offset_milliseconds))
            .format(&Rfc3339)
            .unwrap()
    }

    async fn receive(subscription: EventSubscription) -> Result<EventWaitResult, ProtocolError> {
        tokio::time::timeout(Duration::from_secs(3), subscription.receive())
            .await
            .expect("event wait deadline")
    }

    #[tokio::test]
    async fn registration_is_state_first_when_publication_races_the_scheduler() {
        let (_temporary, _database, service) = service();
        for sequence in 0..64 {
            let subscription = service
                .subscribe(
                    wait(None, vec![filter("test-events", EventCategory::Other)]),
                    EventVisibility::unrestricted(),
                )
                .unwrap();
            service
                .publish(other(
                    "fixture.changed",
                    Some(REPOSITORY_A),
                    &timestamp(i64::from(sequence)),
                ))
                .unwrap();
            let result = receive(subscription).await.unwrap();
            assert_eq!(result.events.len(), 1);
            assert_eq!(result.events[0].filter_ids, ["test-events"]);
        }
    }

    #[tokio::test]
    async fn one_scheduler_groups_due_filters_and_mixes_events_with_heartbeats() {
        let (_temporary, _database, service) = service();
        service
            .publish(deployment("deployment.started", &timestamp(-500)))
            .unwrap();
        let mut due_a = filter("health-a", EventCategory::Health);
        let mut due_b = filter("health-b", EventCategory::Health);
        let mut satisfied = filter("deployment-due", EventCategory::Deployment);
        let deadline = timestamp(-100);
        due_a.deadline_at = Some(deadline.clone());
        due_b.deadline_at = Some(deadline.clone());
        satisfied.deadline_at = Some(deadline.clone());
        let result = receive(
            service
                .subscribe(
                    wait(
                        Some(0),
                        vec![
                            filter("deployments", EventCategory::Deployment),
                            satisfied,
                            due_a,
                            due_b,
                        ],
                    ),
                    EventVisibility::unrestricted(),
                )
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(result.events.len(), 1);
        assert_eq!(
            result.events[0].filter_ids,
            ["deployments", "deployment-due"]
        );
        assert_eq!(
            result
                .heartbeat_due
                .iter()
                .map(|heartbeat| heartbeat.filter_id.as_str())
                .collect::<Vec<_>>(),
            ["health-a", "health-b"]
        );
    }

    #[tokio::test]
    async fn authorization_filters_results_and_rejects_explicit_forbidden_scopes() {
        let (_temporary, _database, service) = service();
        service
            .publish(other(
                "repository.changed",
                Some(REPOSITORY_A),
                &timestamp(-20),
            ))
            .unwrap();
        service
            .publish(other(
                "repository.changed",
                Some(REPOSITORY_B),
                &timestamp(-10),
            ))
            .unwrap();
        let visibility = EventVisibility {
            unrestricted: false,
            repository_ids: BTreeSet::from([REPOSITORY_A.to_owned()]),
            deployment_ids: BTreeSet::new(),
        };
        let result = receive(
            service
                .subscribe(
                    wait(Some(0), vec![filter("repositories", EventCategory::Other)]),
                    visibility.clone(),
                )
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(result.events.len(), 1);
        assert_eq!(
            repository_id(&result.events[0].event.event),
            Some(REPOSITORY_A)
        );

        let mut forbidden = filter("forbidden", EventCategory::Other);
        forbidden.repository_ids.push(REPOSITORY_B.to_owned());
        let error = service
            .subscribe(wait(None, vec![forbidden]), visibility)
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::PermissionDenied);

        let hidden_bounds = receive(
            service
                .subscribe(
                    wait(Some(100), vec![filter("future", EventCategory::Other)]),
                    EventVisibility {
                        unrestricted: false,
                        repository_ids: BTreeSet::from([REPOSITORY_A.to_owned()]),
                        deployment_ids: BTreeSet::new(),
                    },
                )
                .unwrap(),
        )
        .await
        .unwrap_err();
        assert_eq!(hidden_bounds.code, ErrorCode::CursorStale);
        assert!(hidden_bounds.detail.is_empty());
    }

    #[tokio::test]
    async fn long_waits_refresh_revoked_authority_before_delivery() {
        let (_temporary, _database, service) = service();
        let current = Arc::new(Mutex::new(EventVisibility {
            unrestricted: false,
            repository_ids: BTreeSet::from([REPOSITORY_A.to_owned()]),
            deployment_ids: BTreeSet::new(),
        }));
        let provider: VisibilityProvider = {
            let current = Arc::clone(&current);
            Arc::new(move || Ok(current.lock().unwrap().clone()))
        };
        let mut broad = filter("broad", EventCategory::Other);
        broad.deadline_at = Some(timestamp(50));
        let subscription = service
            .subscribe_with_visibility(wait(None, vec![broad]), Arc::clone(&provider))
            .unwrap();
        *current.lock().unwrap() = EventVisibility::default();
        service
            .publish(other(
                "repository.changed",
                Some(REPOSITORY_A),
                &timestamp(0),
            ))
            .unwrap();
        let result = receive(subscription).await.unwrap();
        assert!(result.events.is_empty());
        assert_eq!(result.heartbeat_due[0].filter_id, "broad");

        *current.lock().unwrap() = EventVisibility {
            unrestricted: false,
            repository_ids: BTreeSet::from([REPOSITORY_A.to_owned()]),
            deployment_ids: BTreeSet::new(),
        };
        let mut explicit = filter("explicit", EventCategory::Other);
        explicit.repository_ids.push(REPOSITORY_A.to_owned());
        let subscription = service
            .subscribe_with_visibility(wait(None, vec![explicit]), provider)
            .unwrap();
        *current.lock().unwrap() = EventVisibility::default();
        service
            .publish(other(
                "repository.changed",
                Some(REPOSITORY_A),
                &timestamp(1),
            ))
            .unwrap();
        assert_eq!(
            receive(subscription).await.unwrap_err().code,
            ErrorCode::PermissionDenied
        );
    }

    #[tokio::test]
    async fn duplicate_and_out_of_order_events_keep_one_monotonic_cursor() {
        let (_temporary, _database, service) = service();
        let mut later = other(
            "fixture.changed",
            Some(REPOSITORY_A),
            "2026-09-04T10:00:00Z",
        );
        later.dedupe_key = Some("fixture:one".to_owned());
        let first = service.publish(later.clone()).unwrap();
        let duplicate = service.publish(later).unwrap();
        let second = service
            .publish(other(
                "fixture.changed",
                Some(REPOSITORY_A),
                "2026-09-04T09:00:00Z",
            ))
            .unwrap();
        assert_eq!(first.cursor, duplicate.cursor);
        assert!(duplicate.duplicate);
        assert!(second.cursor > first.cursor);

        let result = receive(
            service
                .subscribe(
                    wait(Some(0), vec![filter("all", EventCategory::Other)]),
                    EventVisibility::unrestricted(),
                )
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(result.events.len(), 2);
        assert_eq!(result.events[0].event.occurred_at, "2026-09-04T10:00:00Z");
        assert_eq!(result.events[1].event.occurred_at, "2026-09-04T09:00:00Z");
    }

    #[tokio::test]
    async fn restart_replays_retained_history_and_old_cursors_become_stale() {
        let temporary = tempdir().unwrap();
        let path = temporary.path().join("authority.sqlite3");
        let database = Database::open(&path).unwrap();
        let service = EventService::new(database.clone()).unwrap();
        let receipt = service
            .publish(other(
                "fixture.started",
                Some(REPOSITORY_A),
                &timestamp(-100),
            ))
            .unwrap();
        let interrupted = service
            .subscribe(
                wait(None, vec![filter("interrupted", EventCategory::Health)]),
                EventVisibility::unrestricted(),
            )
            .unwrap();
        drop(service);
        assert_eq!(
            receive(interrupted).await.unwrap_err().code,
            ErrorCode::DaemonUnavailable
        );
        let restarted = EventService::new(database).unwrap();
        let result = receive(
            restarted
                .subscribe(
                    wait(Some(0), vec![filter("restart", EventCategory::Other)]),
                    EventVisibility::unrestricted(),
                )
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(result.events[0].event.cursor, receipt.cursor);

        for sequence in 0..=MAX_RETAINED_EVENTS {
            restarted
                .publish(other(
                    "fixture.retained",
                    Some(REPOSITORY_A),
                    &timestamp(i64::try_from(sequence).unwrap()),
                ))
                .unwrap();
        }
        let retained: usize = restarted
            .database
            .call(|connection| {
                let count =
                    connection.query_row("SELECT COUNT(*) FROM owned_events", [], |row| {
                        row.get::<_, i64>(0)
                    })?;
                usize::try_from(count).map_err(|_| {
                    DatabaseError::Domain(ProtocolError::new(
                        ErrorCode::InternalError,
                        "owned event count is invalid",
                    ))
                })
            })
            .unwrap();
        assert_eq!(retained, MAX_RETAINED_EVENTS);
        let error = receive(
            restarted
                .subscribe(
                    wait(Some(0), vec![filter("stale", EventCategory::Other)]),
                    EventVisibility::unrestricted(),
                )
                .unwrap(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::CursorStale);
        let future = receive(
            restarted
                .subscribe(
                    wait(
                        Some(1_000_000),
                        vec![filter("future", EventCategory::Other)],
                    ),
                    EventVisibility::unrestricted(),
                )
                .unwrap(),
        )
        .await
        .unwrap_err();
        assert_eq!(future.code, ErrorCode::CursorStale);
    }

    #[tokio::test]
    async fn many_clients_receive_one_event_and_cancel_without_leaking_capacity() {
        let (_temporary, _database, service) = service();
        let mut subscriptions = Vec::new();
        for sequence in 0..128 {
            subscriptions.push(
                service
                    .subscribe(
                        wait(
                            None,
                            vec![filter(&format!("client-{sequence}"), EventCategory::Health)],
                        ),
                        EventVisibility::unrestricted(),
                    )
                    .unwrap(),
            );
        }
        service
            .publish(NewEvent {
                occurred_at: timestamp(0),
                event: OwnedEvent::Health(HealthOwnedEvent {
                    kind: "health.changed".to_owned(),
                    repository_id: None,
                    deployment_id: None,
                    subject_kind: "host".to_owned(),
                    subject_id: "host".to_owned(),
                    severity: None,
                }),
                dedupe_key: Some("health:one".to_owned()),
            })
            .unwrap();
        for subscription in subscriptions {
            let result = receive(subscription).await.unwrap();
            assert_eq!(result.events.len(), 1);
        }

        let cancelled = service
            .subscribe(
                wait(None, vec![filter("cancelled", EventCategory::Health)]),
                EventVisibility::unrestricted(),
            )
            .unwrap();
        drop(cancelled);
        let deadline = timestamp(-1);
        let mut heartbeat = filter("after-cancel", EventCategory::Health);
        heartbeat.deadline_at = Some(deadline);
        let result = receive(
            service
                .subscribe(wait(None, vec![heartbeat]), EventVisibility::unrestricted())
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(result.heartbeat_due.len(), 1);
    }

    #[test]
    fn bounds_redacted_payloads_and_rejects_unsafe_filters() {
        let (_temporary, _database, service) = service();
        let mut oversized = other("fixture.changed", Some(REPOSITORY_A), &timestamp(0));
        if let OwnedEvent::Other(event) = &mut oversized.event {
            event.subject_id = "x".repeat(MAX_EVENT_BYTES);
        }
        assert_eq!(
            service.publish(oversized).unwrap_err().code,
            ErrorCode::ParamsInvalid
        );

        let mut bad_kind = filter("bad-kind", EventCategory::Other);
        bad_kind.kinds.push("Bad Kind".to_owned());
        assert_eq!(
            service
                .subscribe(wait(None, vec![bad_kind]), EventVisibility::unrestricted())
                .unwrap_err()
                .code,
            ErrorCode::ParamsInvalid
        );
        assert_eq!(
            service
                .subscribe(
                    wait(
                        None,
                        vec![
                            filter("duplicate", EventCategory::Other),
                            filter("duplicate", EventCategory::Health)
                        ],
                    ),
                    EventVisibility::unrestricted(),
                )
                .unwrap_err()
                .code,
            ErrorCode::ParamsInvalid
        );
    }

    #[tokio::test]
    async fn wait_batches_at_one_hundred_and_advances_without_repeating_events() {
        let (_temporary, _database, service) = service();
        for sequence in 0..120 {
            service
                .publish(other(
                    "fixture.batch",
                    Some(REPOSITORY_A),
                    &timestamp(sequence),
                ))
                .unwrap();
        }
        let first = receive(
            service
                .subscribe(
                    wait(Some(0), vec![filter("batch", EventCategory::Other)]),
                    EventVisibility::unrestricted(),
                )
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(first.events.len(), MAX_BATCH);
        let second = receive(
            service
                .subscribe(
                    wait(
                        Some(first.cursor),
                        vec![filter("batch", EventCategory::Other)],
                    ),
                    EventVisibility::unrestricted(),
                )
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(second.events.len(), 20);
        assert!(second.events[0].event.cursor > first.events.last().unwrap().event.cursor);
    }

    #[tokio::test]
    async fn subscription_backpressure_is_bounded_and_cancellation_releases_capacity() {
        let (_temporary, _database, service) = service();
        let mut subscriptions = Vec::new();
        for sequence in 0..MAX_SUBSCRIPTIONS {
            subscriptions.push(
                service
                    .subscribe(
                        wait(
                            None,
                            vec![filter(&format!("wait-{sequence}"), EventCategory::Health)],
                        ),
                        EventVisibility::unrestricted(),
                    )
                    .unwrap(),
            );
        }
        let overflow = service
            .subscribe(
                wait(None, vec![filter("overflow", EventCategory::Health)]),
                EventVisibility::unrestricted(),
            )
            .unwrap();
        let error = receive(overflow).await.unwrap_err();
        assert_eq!(error.code, ErrorCode::Busy);

        drop(subscriptions.pop());
        let mut due = filter("after-release", EventCategory::Health);
        due.deadline_at = Some(timestamp(50));
        let result = receive(
            service
                .subscribe(wait(None, vec![due]), EventVisibility::unrestricted())
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(result.heartbeat_due[0].filter_id, "after-release");
    }

    #[tokio::test]
    async fn aggregate_batch_bytes_remain_below_the_protocol_response_cap() {
        let (_temporary, _database, service) = service();
        for sequence in 0..40 {
            let mut event = other("fixture.large", Some(REPOSITORY_A), &timestamp(sequence));
            if let OwnedEvent::Other(event) = &mut event.event {
                event.subject_id = format!("{sequence}-{}", "x".repeat(7_000));
            }
            service.publish(event).unwrap();
        }
        let result = receive(
            service
                .subscribe(
                    wait(Some(0), vec![filter("large", EventCategory::Other)]),
                    EventVisibility::unrestricted(),
                )
                .unwrap(),
        )
        .await
        .unwrap();
        assert!(result.events.len() < 40);
        assert!(serde_json::to_vec(&result).unwrap().len() < 240 * 1024);
    }
}
