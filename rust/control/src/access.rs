//! Public identities, grants, and per-operation authorization.
//!
//! The kernel peer remains the authority for local callers. A public identity
//! is accepted only when the configured edge uid asserts it, and public roles
//! are loaded from SQLite for every request so revocation is immediately
//! observable by the next request.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use devcoordinator2_api::params::{
    AcceptInvitation, AccessRole, EmailOnly, HealthHistory,
    InvitationGrant as InvitationGrantParam, InviteUser, RemoveGrant, SetGrant,
};
use devcoordinator2_api::results::{
    AcceptedInvitation, AccessUser, GrantRemoved, GrantSet, Invitation as InvitationResult,
    InvitationGrant as InvitationGrantResult, InvitedUser, RemovedUser, UserGrant, UserList,
    WhoAmI,
};
use devcoordinator2_api::{
    ClientContext, ClientKind, ErrorCode, ProtocolError, Role, Scope, operation,
};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::{Duration, OffsetDateTime, format_description::FormatItem, macros::format_description};

use crate::config::Config;
use crate::database::{Database, DatabaseError};
use crate::events::EventVisibility;

const INVITATION_DAYS: i64 = 14;
const TIMESTAMP_FORMAT: &[FormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");

/// Kernel-authenticated transport caller plus the protocol client context.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Caller {
    pub pid: u32,
    pub uid: u32,
    pub gid: u32,
    pub client_kind: ClientKind,
    pub client_session: Option<String>,
    pub work: Option<devcoordinator2_api::work_context::WorkAttribution>,
    /// Public e-mail asserted by the configured edge, or `None` for a trusted
    /// local caller.
    pub identity: Option<String>,
}

impl Caller {
    pub fn from_client(
        pid: u32,
        uid: u32,
        gid: u32,
        client: ClientContext,
        edge_uid: Option<u32>,
    ) -> Result<Self, ProtocolError> {
        validate_identity_assertion(uid, edge_uid, client.identity.as_deref())?;
        let work = client.work_attribution();
        Ok(Self {
            pid,
            uid,
            gid,
            client_kind: client.kind,
            client_session: client.session,
            work,
            identity: client
                .identity
                .map(|identity| normalize_identity(&identity)),
        })
    }

    pub fn is_local(&self) -> bool {
        self.identity.is_none()
    }

    pub fn actor(&self) -> String {
        self.identity
            .clone()
            .unwrap_or_else(|| format!("uid:{}", self.uid))
    }
}

/// Refuse request-body identity assertions from every peer except the exact
/// configured edge uid. Kept reusable so the socket boundary can reject the
/// request before dispatch; [`Access`] repeats it defensively.
pub fn validate_identity_assertion(
    peer_uid: u32,
    edge_uid: Option<u32>,
    identity: Option<&str>,
) -> Result<(), ProtocolError> {
    if identity.is_some() && edge_uid != Some(peer_uid) {
        return Err(ProtocolError::new(
            ErrorCode::PermissionDenied,
            "only the configured edge may assert a public identity",
        ));
    }
    Ok(())
}

/// Current authorization principal. Deployment grants are a per-request
/// snapshot and are never cached across requests.
#[derive(Clone, Debug, PartialEq)]
pub struct Principal {
    pub local: bool,
    pub identity: Option<String>,
    pub user_id: Option<String>,
    pub administrator: bool,
    pub grants: BTreeMap<String, AccessRole>,
}

impl Principal {
    pub fn role_for(&self, deployment_id: &str) -> Option<AccessRole> {
        if self.local || self.administrator {
            return Some(AccessRole::Administrator);
        }
        self.grants.get(deployment_id).cloned()
    }

    pub fn at_least(&self, deployment_id: &str, required: &AccessRole) -> bool {
        self.role_for(deployment_id)
            .is_some_and(|actual| role_rank(&actual) >= role_rank(required))
    }
}

/// Access portion of the atomic edge route document.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteAccessSection {
    pub owners: Vec<String>,
    pub grants: Vec<RouteGrant>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteGrant {
    pub identity: String,
    pub deployment_id: String,
    pub role: AccessRole,
}

/// Boundary implemented by the route-document owner. Access mutations commit
/// in SQLite before publishing a complete replacement snapshot, matching the
/// existing lost-reply/re-query semantics.
pub trait RoutePublisher: Send + Sync {
    fn publish_access(&self, access: &RouteAccessSection) -> Result<(), ProtocolError>;
}

impl<F> RoutePublisher for F
where
    F: Fn(&RouteAccessSection) -> Result<(), ProtocolError> + Send + Sync,
{
    fn publish_access(&self, access: &RouteAccessSection) -> Result<(), ProtocolError> {
        self(access)
    }
}

/// Required post-dispatch filtering for collection operations available to a
/// non-global public principal. Missing or malformed result shapes fail
/// closed rather than returning an unfiltered response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResultFilter {
    None,
    DeploymentList {
        deployment_ids: BTreeSet<String>,
    },
    HealthRepositories {
        repository_ids: BTreeSet<String>,
        deployment_ids: BTreeSet<String>,
    },
    RepositoryCollection {
        repository_ids: BTreeSet<String>,
    },
}

/// Authorization decision passed to the operation dispatcher. `params` may
/// be narrower than the original request (notably `deployment.list`, where a
/// public path must never cause Git discovery under the edge account).
#[derive(Clone, Debug, PartialEq)]
pub struct Authorization {
    pub principal: Principal,
    pub params: Value,
    pub result_filter: ResultFilter,
}

impl Authorization {
    pub fn apply_result(&self, result: Value) -> Result<Value, ProtocolError> {
        self.result_filter.apply(result)
    }
}

impl ResultFilter {
    pub fn apply(&self, mut result: Value) -> Result<Value, ProtocolError> {
        match self {
            Self::None => Ok(result),
            Self::DeploymentList { deployment_ids } => {
                let object = result_object(&mut result, "deployment.list")?;
                retain_rows(
                    object.get_mut("deployments"),
                    "deployment_id",
                    deployment_ids,
                    "deployment.list",
                )?;
                let declared = object.get_mut("declared").ok_or_else(|| {
                    malformed_filtered_result("deployment.list", "missing declared collection")
                })?;
                let declared = declared.as_array_mut().ok_or_else(|| {
                    malformed_filtered_result("deployment.list", "declared is not an array")
                })?;
                declared.clear();
                Ok(result)
            }
            Self::HealthRepositories {
                repository_ids,
                deployment_ids,
            } => {
                let object = result_object(&mut result, "health.repositories")?;
                retain_rows(
                    object.get_mut("repositories"),
                    "repository_id",
                    repository_ids,
                    "health.repositories",
                )?;
                let repositories = object
                    .get_mut("repositories")
                    .and_then(Value::as_array_mut)
                    .expect("retain_rows validated the collection");
                for repository in repositories {
                    let row = repository.as_object_mut().ok_or_else(|| {
                        malformed_filtered_result(
                            "health.repositories",
                            "repository row is not an object",
                        )
                    })?;
                    // Public repository viewers never receive a host path;
                    // security-assumptions.md keeps filesystem topology on
                    // the trusted local side of the edge boundary.
                    row.insert("root_path".to_owned(), Value::Null);
                    retain_rows(
                        row.get_mut("deployments"),
                        "deployment_id",
                        deployment_ids,
                        "health.repositories",
                    )?;
                }
                let repositories = object.remove("repositories").expect("validated above");
                object.clear();
                object.insert("repositories".to_owned(), repositories);
                Ok(result)
            }
            Self::RepositoryCollection { repository_ids } => {
                let object = result_object(&mut result, "repository collection")?;
                retain_rows(
                    object.get_mut("repositories"),
                    "repository_id",
                    repository_ids,
                    "repository collection",
                )?;
                Ok(result)
            }
        }
    }
}

/// SQLite-backed public access service.
#[derive(Clone)]
pub struct Access {
    database: Database,
    publisher: Arc<dyn RoutePublisher>,
    edge_uid: Option<u32>,
    unit_prefix: Arc<str>,
    admin_emails: Arc<[String]>,
}

impl Access {
    pub fn new(
        config: &Config,
        database: Database,
        publisher: Arc<dyn RoutePublisher>,
    ) -> Result<Self, ProtocolError> {
        Self::from_settings(
            database,
            publisher,
            config.edge_uid,
            config.unit_prefix.clone(),
            config.admin_emails.clone(),
        )
    }

    fn from_settings(
        database: Database,
        publisher: Arc<dyn RoutePublisher>,
        edge_uid: Option<u32>,
        unit_prefix: String,
        admin_emails: Vec<String>,
    ) -> Result<Self, ProtocolError> {
        let access = Self {
            database,
            publisher,
            edge_uid,
            unit_prefix: unit_prefix.into(),
            admin_emails: admin_emails.into(),
        };
        access.bootstrap()?;
        Ok(access)
    }

    pub fn database(&self) -> &Database {
        &self.database
    }

    /// Load the principal and its complete grant snapshot in one transaction.
    pub fn principal(&self, caller: &Caller) -> Result<Principal, ProtocolError> {
        validate_identity_assertion(caller.uid, self.edge_uid, caller.identity.as_deref())?;
        let Some(asserted_identity) = caller.identity.as_deref() else {
            return Ok(Principal {
                local: true,
                identity: None,
                user_id: None,
                administrator: false,
                grants: BTreeMap::new(),
            });
        };
        let identity = normalize_identity(asserted_identity);
        let lookup = identity.clone();
        let now = timestamp()?;
        self.database
            .transaction(move |transaction| {
                let user = transaction
                    .query_row(
                        "SELECT user_id,administrator FROM users WHERE email=?1",
                        [&lookup],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? != 0)),
                    )
                    .optional()?;
                let Some((user_id, administrator)) = user else {
                    return Ok(Principal {
                        local: false,
                        identity: Some(lookup),
                        user_id: None,
                        administrator: false,
                        grants: BTreeMap::new(),
                    });
                };
                let mut statement = transaction.prepare(
                    "SELECT deployment_id,role FROM grants WHERE user_id=?1 \
                     ORDER BY deployment_id",
                )?;
                let rows = statement.query_map([&user_id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?;
                let mut grants = BTreeMap::new();
                for row in rows {
                    let (deployment_id, role) = row?;
                    grants.insert(deployment_id, stored_role(&role)?);
                }
                drop(statement);
                transaction.execute(
                    "UPDATE users SET last_seen_at=?1 WHERE user_id=?2",
                    rusqlite::params![now, user_id],
                )?;
                Ok(Principal {
                    local: false,
                    identity: Some(lookup),
                    user_id: Some(user_id),
                    administrator,
                    grants,
                })
            })
            .map_err(database_or_domain)
    }

    pub fn who_am_i(&self, caller: &Caller) -> Result<WhoAmI, ProtocolError> {
        let principal = self.principal(caller)?;
        Ok(WhoAmI {
            local: principal.local,
            identity: principal.identity,
            user_id: principal.user_id,
            administrator: principal.administrator || principal.local,
            grants: principal.grants,
        })
    }

    pub fn list_users(&self) -> Result<UserList, ProtocolError> {
        self.database
            .call(|connection| {
                let mut users_statement = connection.prepare(
                    "SELECT user_id,email,subject,display_name,administrator,created_at,\
                     created_by,last_seen_at FROM users ORDER BY email",
                )?;
                let user_rows = users_statement.query_map([], |row| {
                    Ok(AccessUser {
                        user_id: row.get(0)?,
                        email: row.get(1)?,
                        subject: row.get(2)?,
                        display_name: row.get(3)?,
                        administrator: row.get::<_, i64>(4)? != 0,
                        created_at: row.get(5)?,
                        created_by: row.get(6)?,
                        last_seen_at: row.get(7)?,
                        grants: Vec::new(),
                    })
                })?;
                let mut users = user_rows.collect::<Result<Vec<_>, _>>()?;
                drop(users_statement);

                let mut grants_by_user: BTreeMap<String, Vec<UserGrant>> = BTreeMap::new();
                let mut grant_statement = connection.prepare(
                    "SELECT user_id,deployment_id,role,granted_at FROM grants \
                     ORDER BY user_id,deployment_id",
                )?;
                let grant_rows = grant_statement.query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })?;
                for row in grant_rows {
                    let (user_id, deployment_id, role, granted_at) = row?;
                    grants_by_user.entry(user_id).or_default().push(UserGrant {
                        deployment_id,
                        role: stored_role(&role)?,
                        granted_at,
                    });
                }
                drop(grant_statement);
                for user in &mut users {
                    user.grants = grants_by_user.remove(&user.user_id).unwrap_or_default();
                }

                let mut invitation_statement = connection.prepare(
                    "SELECT invitation_id,email,administrator,grants_json,created_at,\
                     created_by,expires_at FROM invitations \
                     ORDER BY created_at,invitation_id",
                )?;
                let invitation_rows = invitation_statement.query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)? != 0,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                    ))
                })?;
                let mut invitations = Vec::new();
                for row in invitation_rows {
                    let (
                        invitation_id,
                        email,
                        administrator,
                        grants_json,
                        created_at,
                        created_by,
                        expires_at,
                    ) = row?;
                    let grants = decode_invitation_grants(&grants_json)?
                        .into_iter()
                        .map(|grant| InvitationGrantResult {
                            deployment_id: grant.deployment_id,
                            role: grant.role,
                        })
                        .collect();
                    invitations.push(InvitationResult {
                        invitation_id,
                        email,
                        administrator,
                        created_at,
                        created_by,
                        expires_at,
                        grants,
                    });
                }
                let owners = users
                    .iter()
                    .filter(|user| user.administrator)
                    .map(|user| user.email.clone())
                    .collect();
                Ok(UserList {
                    users,
                    invitations,
                    roles: all_roles(),
                    owners,
                })
            })
            .map_err(database_or_domain)
    }

    pub fn invite(
        &self,
        params: InviteUser,
        caller: &Caller,
    ) -> Result<InvitedUser, ProtocolError> {
        validate_identity_assertion(caller.uid, self.edge_uid, caller.identity.as_deref())?;
        let email = validate_email(&params.email)?;
        for grant in &params.grants {
            validate_grant(grant)?;
        }
        let invitation_id = random_id('i')?;
        let now = OffsetDateTime::now_utc();
        let created_at = format_timestamp(now)?;
        let expires_at = format_timestamp(now + Duration::days(INVITATION_DAYS))?;
        let grants_json = serde_json::to_string(&params.grants).map_err(|error| {
            ProtocolError::new(ErrorCode::InternalError, "cannot encode invitation grants")
                .with_detail(error.to_string())
        })?;
        let created_by = caller.actor();
        let stored_email = email.clone();
        let stored_invitation_id = invitation_id.clone();
        let stored_expiry = expires_at.clone();
        let administrator = params.administrator;
        self.database
            .transaction(move |transaction| {
                if transaction
                    .query_row(
                        "SELECT 1 FROM users WHERE email=?1",
                        [&stored_email],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some()
                {
                    return Err(domain_error(
                        ErrorCode::ParamsInvalid,
                        format!("{stored_email} is already a user"),
                    ));
                }
                transaction.execute("DELETE FROM invitations WHERE email=?1", [&stored_email])?;
                transaction.execute(
                    "INSERT INTO invitations(invitation_id,email,administrator,grants_json,\
                     created_at,created_by,expires_at) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                    rusqlite::params![
                        stored_invitation_id,
                        stored_email,
                        i64::from(administrator),
                        grants_json,
                        created_at,
                        created_by,
                        stored_expiry
                    ],
                )?;
                Ok(())
            })
            .map_err(database_or_domain)?;
        Ok(InvitedUser {
            invitation_id,
            email,
            expires_at,
        })
    }

    pub fn accept_invitation(
        &self,
        params: AcceptInvitation,
    ) -> Result<AcceptedInvitation, ProtocolError> {
        let email = validate_email(&params.email)?;
        let user_id = random_id('u')?;
        let now = timestamp()?;
        let stored_email = email.clone();
        let stored_user_id = user_id.clone();
        let subject = params.subject;
        let display_name = params.display_name;
        let outcome = self
            .database
            .transaction(move |transaction| {
                if let Some((existing_user_id, administrator)) = transaction
                    .query_row(
                        "SELECT user_id,administrator FROM users WHERE email=?1",
                        [&stored_email],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? != 0)),
                    )
                    .optional()?
                {
                    return Ok(AcceptOutcome::Existing(AcceptedInvitation {
                        email: stored_email,
                        user_id: existing_user_id,
                        accepted: false,
                        administrator,
                    }));
                }
                let invitation = transaction
                    .query_row(
                        "SELECT administrator,grants_json,created_by,expires_at \
                         FROM invitations WHERE email=?1",
                        [&stored_email],
                        |row| {
                            Ok(StoredInvitation {
                                administrator: row.get::<_, i64>(0)? != 0,
                                grants_json: row.get(1)?,
                                created_by: row.get(2)?,
                                expires_at: row.get(3)?,
                            })
                        },
                    )
                    .optional()?;
                let Some(invitation) = invitation else {
                    return Ok(AcceptOutcome::Missing);
                };
                if invitation.expires_at.as_str() < now.as_str() {
                    transaction
                        .execute("DELETE FROM invitations WHERE email=?1", [&stored_email])?;
                    return Ok(AcceptOutcome::Expired);
                }
                let grants = decode_invitation_grants(&invitation.grants_json)?;
                transaction.execute(
                    "INSERT INTO users(user_id,email,subject,display_name,administrator,\
                     created_at,created_by,last_seen_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                    rusqlite::params![
                        stored_user_id,
                        stored_email,
                        subject,
                        display_name,
                        i64::from(invitation.administrator),
                        now,
                        invitation.created_by,
                        now
                    ],
                )?;
                for grant in grants {
                    transaction.execute(
                        "INSERT OR REPLACE INTO grants(user_id,deployment_id,role,granted_at,\
                         granted_by) VALUES(?1,?2,?3,?4,?5)",
                        rusqlite::params![
                            stored_user_id,
                            grant.deployment_id,
                            role_name(&grant.role),
                            now,
                            invitation.created_by
                        ],
                    )?;
                }
                transaction.execute("DELETE FROM invitations WHERE email=?1", [&stored_email])?;
                Ok(AcceptOutcome::Accepted(AcceptedInvitation {
                    email: stored_email,
                    user_id: stored_user_id,
                    accepted: true,
                    administrator: invitation.administrator,
                }))
            })
            .map_err(database_or_domain)?;
        match outcome {
            AcceptOutcome::Existing(result) => Ok(result),
            AcceptOutcome::Missing => Err(ProtocolError::new(
                ErrorCode::PermissionDenied,
                "no invitation for this identity",
            )),
            AcceptOutcome::Expired => Err(ProtocolError::new(
                ErrorCode::PermissionDenied,
                "invitation expired",
            )),
            AcceptOutcome::Accepted(result) => {
                self.republish()?;
                Ok(result)
            }
        }
    }

    pub fn remove_user(
        &self,
        params: EmailOnly,
        caller: &Caller,
    ) -> Result<RemovedUser, ProtocolError> {
        validate_identity_assertion(caller.uid, self.edge_uid, caller.identity.as_deref())?;
        let email = validate_email(&params.email)?;
        let stored_email = email.clone();
        let removed = self
            .database
            .transaction(move |transaction| {
                let user_id = transaction
                    .query_row(
                        "SELECT user_id FROM users WHERE email=?1",
                        [&stored_email],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?;
                if let Some(user_id) = &user_id {
                    transaction.execute("DELETE FROM grants WHERE user_id=?1", [user_id])?;
                    transaction.execute("DELETE FROM users WHERE user_id=?1", [user_id])?;
                }
                let invitations = transaction
                    .execute("DELETE FROM invitations WHERE email=?1", [&stored_email])?;
                if user_id.is_none() && invitations == 0 {
                    return Err(domain_error(
                        ErrorCode::UserNotFound,
                        format!("no user or invitation for {stored_email}"),
                    ));
                }
                Ok((user_id.is_some(), invitations != 0))
            })
            .map_err(database_or_domain)?;
        self.republish()?;
        Ok(RemovedUser {
            email,
            removed_user: removed.0,
            removed_invitation: removed.1,
        })
    }

    pub fn set_grant(&self, params: SetGrant, caller: &Caller) -> Result<GrantSet, ProtocolError> {
        validate_identity_assertion(caller.uid, self.edge_uid, caller.identity.as_deref())?;
        let email = validate_email(&params.email)?;
        validate_deployment_id(&params.deployment_id)?;
        let stored_email = email.clone();
        let deployment_id = params.deployment_id.clone();
        let stored_deployment_id = deployment_id.clone();
        let role = params.role.clone();
        let stored_role = role_name(&role).to_owned();
        let granted_at = timestamp()?;
        let granted_by = caller.actor();
        self.database
            .transaction(move |transaction| {
                let user_id = transaction
                    .query_row(
                        "SELECT user_id FROM users WHERE email=?1",
                        [&stored_email],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?
                    .ok_or_else(|| {
                        domain_error(ErrorCode::UserNotFound, format!("no user {stored_email}"))
                    })?;
                let deployment_exists = transaction.query_row(
                    "SELECT EXISTS(SELECT 1 FROM deployments WHERE deployment_id=?1) OR \
                     EXISTS(SELECT 1 FROM observed_deployments \
                            WHERE observed_deployment_id=?1)",
                    [&stored_deployment_id],
                    |row| row.get::<_, i64>(0),
                )? != 0;
                if !deployment_exists {
                    return Err(domain_error(
                        ErrorCode::DeploymentNotFound,
                        format!("no deployment {stored_deployment_id}"),
                    ));
                }
                transaction.execute(
                    "INSERT OR REPLACE INTO grants(user_id,deployment_id,role,granted_at,\
                     granted_by) VALUES(?1,?2,?3,?4,?5)",
                    rusqlite::params![
                        user_id,
                        stored_deployment_id,
                        stored_role,
                        granted_at,
                        granted_by
                    ],
                )?;
                Ok(())
            })
            .map_err(database_or_domain)?;
        self.republish()?;
        Ok(GrantSet {
            email,
            deployment_id,
            role,
        })
    }

    pub fn remove_grant(
        &self,
        params: RemoveGrant,
        caller: &Caller,
    ) -> Result<GrantRemoved, ProtocolError> {
        validate_identity_assertion(caller.uid, self.edge_uid, caller.identity.as_deref())?;
        let email = validate_email(&params.email)?;
        let deployment_id = params.deployment_id;
        let stored_email = email.clone();
        let stored_deployment_id = deployment_id.clone();
        let removed = self
            .database
            .transaction(move |transaction| {
                let user_id = transaction
                    .query_row(
                        "SELECT user_id FROM users WHERE email=?1",
                        [&stored_email],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?
                    .ok_or_else(|| {
                        domain_error(ErrorCode::UserNotFound, format!("no user {stored_email}"))
                    })?;
                Ok(transaction.execute(
                    "DELETE FROM grants WHERE user_id=?1 AND deployment_id=?2",
                    rusqlite::params![user_id, stored_deployment_id],
                )? != 0)
            })
            .map_err(database_or_domain)?;
        self.republish()?;
        Ok(GrantRemoved {
            email,
            deployment_id,
            removed,
        })
    }

    pub fn access_section(&self) -> Result<RouteAccessSection, ProtocolError> {
        self.database
            .call(|connection| {
                let mut owners_statement = connection
                    .prepare("SELECT email FROM users WHERE administrator=1 ORDER BY email")?;
                let owners = owners_statement
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                drop(owners_statement);
                let mut grants_statement = connection.prepare(
                    "SELECT u.email,g.deployment_id,g.role FROM grants g \
                     JOIN users u ON u.user_id=g.user_id \
                     ORDER BY u.email,g.deployment_id",
                )?;
                let rows = grants_statement.query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?;
                let mut grants = Vec::new();
                for row in rows {
                    let (identity, deployment_id, role) = row?;
                    grants.push(RouteGrant {
                        identity,
                        deployment_id,
                        role: stored_role(&role)?,
                    });
                }
                Ok(RouteAccessSection { owners, grants })
            })
            .map_err(database_or_domain)
    }

    pub fn republish(&self) -> Result<(), ProtocolError> {
        self.publisher.publish_access(&self.access_section()?)
    }

    pub fn event_visibility(&self, caller: &Caller) -> Result<EventVisibility, ProtocolError> {
        let principal = self.principal(caller)?;
        if principal.local || principal.administrator {
            return Ok(EventVisibility::unrestricted());
        }
        if principal.user_id.is_none() {
            return Err(ProtocolError::new(
                ErrorCode::PermissionDenied,
                "identity is not an admitted user",
            ));
        }
        Ok(EventVisibility {
            unrestricted: false,
            repository_ids: self.repositories_at_least(&principal, &AccessRole::Viewer)?,
            deployment_ids: deployment_ids_at_least(&principal, &AccessRole::Viewer),
        })
    }

    /// Resolve one operation's public authority from its registry policy and
    /// return any mandatory collection filter. Called for every request; it
    /// intentionally has no authorization cache.
    pub fn authorize(
        &self,
        operation_name: &str,
        params: &Value,
        caller: &Caller,
    ) -> Result<Authorization, ProtocolError> {
        let definition = operation(operation_name).ok_or_else(|| {
            ProtocolError::new(
                ErrorCode::OperationUnknown,
                format!("unknown operation {operation_name:?}"),
            )
        })?;
        (definition.validate_params)(params)?;
        let principal = self.principal(caller)?;
        if principal.local || principal.administrator {
            return Ok(allow(principal, params.clone()));
        }
        if definition.policy.role == Role::Anonymous {
            return Ok(allow(principal, params.clone()));
        }
        if operation_name == "user.accept_invitation" {
            let requested = params
                .get("email")
                .and_then(Value::as_str)
                .map(normalize_identity);
            if caller.client_kind != ClientKind::Edge || requested != principal.identity {
                return Err(ProtocolError::new(
                    ErrorCode::PermissionDenied,
                    "acceptance is performed by the edge for the signed-in identity",
                ));
            }
            return Ok(allow(principal, params.clone()));
        }
        if principal.user_id.is_none() {
            return Err(ProtocolError::new(
                ErrorCode::PermissionDenied,
                "identity is not an admitted user",
            ));
        }
        if definition.policy.role == Role::Self_ && definition.policy.scope == Scope::Self_ {
            return Ok(allow(principal, params.clone()));
        }

        // Preserve the established boundary: an `administrator` deployment
        // grant does not satisfy any operation whose policy requires the
        // global Console administrator role, even when its scope metadata is
        // `deployment`.
        if definition.policy.role == Role::Administrator {
            return Err(requires_administrator(operation_name));
        }

        match (definition.policy.scope, definition.policy.role) {
            (Scope::Deployment, Role::Viewer | Role::Operator) => {
                let required = match definition.policy.role {
                    Role::Viewer => AccessRole::Viewer,
                    Role::Operator => AccessRole::Operator,
                    _ => unreachable!(),
                };
                if operation_name == "deployment.list" {
                    let deployment_ids = principal
                        .grants
                        .iter()
                        .filter(|(_, role)| role_rank(role) >= role_rank(&AccessRole::Viewer))
                        .map(|(deployment_id, _)| deployment_id.clone())
                        .collect();
                    return Ok(Authorization {
                        principal,
                        params: serde_json::json!({}),
                        result_filter: ResultFilter::DeploymentList { deployment_ids },
                    });
                }
                let deployment_id = params.get("deployment_id").and_then(Value::as_str);
                if deployment_id
                    .is_some_and(|deployment_id| principal.at_least(deployment_id, &required))
                {
                    Ok(allow(principal, params.clone()))
                } else {
                    Err(requires_deployment_role(operation_name, &required))
                }
            }
            (Scope::Repository, Role::Viewer | Role::Operator) => {
                let required = match definition.policy.role {
                    Role::Viewer => AccessRole::Viewer,
                    Role::Operator => AccessRole::Operator,
                    _ => unreachable!(),
                };
                let repositories = self.repositories_at_least(&principal, &required)?;
                if operation_name.starts_with("glossary.")
                    && params.get("repository_id").is_none_or(Value::is_null)
                    && params.get("path").is_none_or(Value::is_null)
                {
                    return Ok(allow(principal, params.clone()));
                }
                if operation_name == "health.repositories" {
                    return Ok(Authorization {
                        result_filter: ResultFilter::HealthRepositories {
                            repository_ids: repositories,
                            deployment_ids: deployment_ids_at_least(&principal, &required),
                        },
                        principal,
                        params: params.clone(),
                    });
                }
                let plan_picker = operation_name == "plan.overview"
                    && params.get("repository_id").is_none_or(Value::is_null)
                    && params.get("path").is_none_or(Value::is_null);
                if plan_picker {
                    return Ok(Authorization {
                        principal,
                        params: params.clone(),
                        result_filter: ResultFilter::RepositoryCollection {
                            repository_ids: repositories,
                        },
                    });
                }
                if matches!(
                    operation_name,
                    "usage.repositories" | "progress.repositories"
                ) {
                    if repositories.is_empty() {
                        return Err(requires_repository_collection_role(
                            operation_name,
                            &required,
                        ));
                    }
                    return Ok(Authorization {
                        principal,
                        params: params.clone(),
                        result_filter: ResultFilter::RepositoryCollection {
                            repository_ids: repositories,
                        },
                    });
                }
                let repository_id = self.repository_for(operation_name, params)?;
                if repository_id
                    .as_ref()
                    .is_some_and(|repository_id| repositories.contains(repository_id))
                {
                    Ok(allow(principal, params.clone()))
                } else {
                    Err(requires_repository_role(operation_name, &required))
                }
            }
            _ => Err(ProtocolError::new(
                ErrorCode::PermissionDenied,
                format!("{operation_name} is not available to public users"),
            )),
        }
    }

    fn bootstrap(&self) -> Result<(), ProtocolError> {
        let now = timestamp()?;
        let mut owners = Vec::with_capacity(self.admin_emails.len());
        for configured in self.admin_emails.iter() {
            owners.push((validate_email(configured)?, random_id('u')?));
        }
        self.database
            .transaction(move |transaction| {
                for (email, user_id) in owners {
                    if transaction
                        .query_row("SELECT 1 FROM users WHERE email=?1", [&email], |_| Ok(()))
                        .optional()?
                        .is_none()
                    {
                        transaction.execute(
                            "INSERT INTO users(user_id,email,administrator,created_at,created_by) \
                             VALUES(?1,?2,1,?3,'instance-configuration')",
                            rusqlite::params![user_id, email, now],
                        )?;
                    }
                }
                Ok(())
            })
            .map_err(database_or_domain)
    }

    fn repositories_at_least(
        &self,
        principal: &Principal,
        required: &AccessRole,
    ) -> Result<BTreeSet<String>, ProtocolError> {
        let deployment_ids = deployment_ids_at_least(principal, required);
        if deployment_ids.is_empty() {
            return Ok(BTreeSet::new());
        }
        self.database
            .call(move |connection| {
                let mut repositories = BTreeSet::new();
                let mut managed =
                    connection.prepare("SELECT deployment_id,repository_id FROM deployments")?;
                for row in managed.query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })? {
                    let (deployment_id, repository_id) = row?;
                    if deployment_ids.contains(&deployment_id) {
                        repositories.insert(repository_id);
                    }
                }
                drop(managed);
                let mut observed = connection.prepare(
                    "SELECT observed_deployment_id,repository_id FROM observed_deployments",
                )?;
                for row in observed.query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })? {
                    let (deployment_id, repository_id) = row?;
                    if deployment_ids.contains(&deployment_id) {
                        repositories.insert(repository_id);
                    }
                }
                Ok(repositories)
            })
            .map_err(database_or_domain)
    }

    fn repository_for(
        &self,
        operation_name: &str,
        params: &Value,
    ) -> Result<Option<String>, ProtocolError> {
        match operation_name {
            "task.history" => {
                let task_id = params
                    .get("task_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                self.database
                    .call(move |connection| {
                        connection
                            .query_row(
                                "SELECT repository_id FROM tasks WHERE task_id=?1",
                                [&task_id],
                                |row| row.get(0),
                            )
                            .optional()
                            .map_err(DatabaseError::from)
                    })
                    .map_err(database_or_domain)
            }
            "health.repository" => {
                let path = params
                    .get("path")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                self.repository_for_exact_path(path)
            }
            "health.history" => {
                let history =
                    serde_json::from_value::<HealthHistory>(params.clone()).map_err(|error| {
                        ProtocolError::new(
                            ErrorCode::ParamsInvalid,
                            "health.history parameters are invalid",
                        )
                        .with_detail(error.to_string())
                    })?;
                self.repository_for_health_subject(history)
            }
            // Public planning reads deliberately do not resolve `path`, since
            // that route may perform implicit registration. health.repository
            // is the sole path exception and is constrained to an exact
            // already-registered path above.
            _ => Ok(params
                .get("repository_id")
                .and_then(Value::as_str)
                .map(str::to_owned)),
        }
    }

    fn repository_for_exact_path(&self, path: String) -> Result<Option<String>, ProtocolError> {
        self.database
            .call(move |connection| {
                let repository = connection
                    .query_row(
                        "SELECT repository_id FROM repositories WHERE root_path=?1",
                        [&path],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?;
                if repository.is_some() {
                    return Ok(repository);
                }
                connection
                    .query_row(
                        "SELECT repository_id FROM worktrees WHERE worktree_path=?1",
                        [&path],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(DatabaseError::from)
            })
            .map_err(database_or_domain)
    }

    fn repository_for_health_subject(
        &self,
        history: HealthHistory,
    ) -> Result<Option<String>, ProtocolError> {
        use devcoordinator2_api::params::MetricSubjectKind;

        let subject_id = history.subject_id;
        let unit_prefix = Arc::clone(&self.unit_prefix);
        self.database
            .call(move |connection| {
                let repository = match history.subject_kind {
                    MetricSubjectKind::Repository => connection
                        .query_row(
                            "SELECT repository_id FROM repositories WHERE repository_id=?1",
                            [&subject_id],
                            |row| row.get(0),
                        )
                        .optional()?,
                    MetricSubjectKind::Worktree => connection
                        .query_row(
                            "SELECT repository_id FROM worktrees WHERE worktree_id=?1",
                            [&subject_id],
                            |row| row.get(0),
                        )
                        .optional()?,
                    MetricSubjectKind::Deployment => {
                        repository_for_deployment(connection, &subject_id)?
                    }
                    MetricSubjectKind::Component => subject_id
                        .split_once('/')
                        .map(|(deployment_id, _)| {
                            repository_for_deployment(connection, deployment_id)
                        })
                        .transpose()?
                        .flatten(),
                    MetricSubjectKind::Container => connection
                        .query_row(
                            "SELECT d.repository_id FROM components c JOIN deployments d \
                             ON d.deployment_id=c.deployment_id \
                             WHERE c.binding_kind='container' AND c.binding_identity=?1 \
                             UNION ALL \
                             SELECT repository_id FROM observed_containers \
                             WHERE container_id=?1 LIMIT 1",
                            [&subject_id],
                            |row| row.get(0),
                        )
                        .optional()?,
                    MetricSubjectKind::Test => {
                        let prefix = format!("{unit_prefix}-");
                        let worktree_id = subject_id
                            .strip_prefix(&prefix)
                            .and_then(|rest| rest.split_once('-').map(|(id, _)| id));
                        if let Some(worktree_id) = worktree_id {
                            connection
                                .query_row(
                                    "SELECT repository_id FROM worktrees WHERE worktree_id=?1",
                                    [worktree_id],
                                    |row| row.get(0),
                                )
                                .optional()?
                        } else {
                            None
                        }
                    }
                    MetricSubjectKind::Host
                    | MetricSubjectKind::Daemon
                    | MetricSubjectKind::Other => None,
                };
                Ok(repository)
            })
            .map_err(database_or_domain)
    }
}

#[derive(Debug)]
struct StoredInvitation {
    administrator: bool,
    grants_json: String,
    created_by: String,
    expires_at: String,
}

enum AcceptOutcome {
    Existing(AcceptedInvitation),
    Missing,
    Expired,
    Accepted(AcceptedInvitation),
}

fn allow(principal: Principal, params: Value) -> Authorization {
    Authorization {
        principal,
        params,
        result_filter: ResultFilter::None,
    }
}

fn deployment_ids_at_least(principal: &Principal, required: &AccessRole) -> BTreeSet<String> {
    principal
        .grants
        .iter()
        .filter(|(_, role)| role_rank(role) >= role_rank(required))
        .map(|(deployment_id, _)| deployment_id.clone())
        .collect()
}

fn repository_for_deployment(
    connection: &rusqlite::Connection,
    deployment_id: &str,
) -> Result<Option<String>, DatabaseError> {
    connection
        .query_row(
            "SELECT repository_id FROM deployments WHERE deployment_id=?1 \
             UNION ALL \
             SELECT repository_id FROM observed_deployments \
             WHERE observed_deployment_id=?1 LIMIT 1",
            [deployment_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(DatabaseError::from)
}

fn result_object<'a>(
    result: &'a mut Value,
    operation_name: &str,
) -> Result<&'a mut serde_json::Map<String, Value>, ProtocolError> {
    result
        .as_object_mut()
        .ok_or_else(|| malformed_filtered_result(operation_name, "result is not an object"))
}

fn retain_rows(
    collection: Option<&mut Value>,
    id_field: &str,
    allowed: &BTreeSet<String>,
    operation_name: &str,
) -> Result<(), ProtocolError> {
    let rows = collection
        .ok_or_else(|| malformed_filtered_result(operation_name, "missing collection"))?
        .as_array_mut()
        .ok_or_else(|| malformed_filtered_result(operation_name, "collection is not an array"))?;
    if rows
        .iter()
        .any(|row| row.get(id_field).and_then(Value::as_str).is_none())
    {
        return Err(malformed_filtered_result(
            operation_name,
            "collection row lacks its authorization identity",
        ));
    }
    rows.retain(|row| {
        row.get(id_field)
            .and_then(Value::as_str)
            .is_some_and(|identity| allowed.contains(identity))
    });
    Ok(())
}

fn malformed_filtered_result(operation_name: &str, detail: &str) -> ProtocolError {
    ProtocolError::new(
        ErrorCode::InternalError,
        format!("{operation_name} returned an invalid filterable result"),
    )
    .with_detail(detail)
}

fn requires_administrator(operation_name: &str) -> ProtocolError {
    ProtocolError::new(
        ErrorCode::PermissionDenied,
        format!("{operation_name} requires administrator"),
    )
}

fn requires_deployment_role(operation_name: &str, role: &AccessRole) -> ProtocolError {
    ProtocolError::new(
        ErrorCode::PermissionDenied,
        format!(
            "{operation_name} requires {} on the deployment",
            role_name(role)
        ),
    )
}

fn requires_repository_role(operation_name: &str, role: &AccessRole) -> ProtocolError {
    ProtocolError::new(
        ErrorCode::PermissionDenied,
        format!(
            "{operation_name} requires {} on a deployment of the repository",
            role_name(role)
        ),
    )
}

fn requires_repository_collection_role(operation_name: &str, role: &AccessRole) -> ProtocolError {
    ProtocolError::new(
        ErrorCode::PermissionDenied,
        format!(
            "{operation_name} requires {} on a repository deployment",
            role_name(role)
        ),
    )
}

fn validate_email(value: &str) -> Result<String, ProtocolError> {
    if !value.contains('@') || value.chars().count() > 254 {
        return Err(ProtocolError::new(
            ErrorCode::ParamsInvalid,
            "'email' must be an e-mail address",
        ));
    }
    Ok(normalize_identity(value))
}

fn normalize_identity(value: &str) -> String {
    value.trim().to_lowercase()
}

fn validate_grant(grant: &InvitationGrantParam) -> Result<(), ProtocolError> {
    validate_deployment_id(&grant.deployment_id)
}

fn validate_deployment_id(value: &str) -> Result<(), ProtocolError> {
    if value.len() >= 2
        && value.starts_with('d')
        && value[1..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        Ok(())
    } else {
        Err(ProtocolError::new(
            ErrorCode::ParamsInvalid,
            "grant deployment_id must be a lowercase 'd…' identifier",
        ))
    }
}

fn role_rank(role: &AccessRole) -> u8 {
    match role {
        AccessRole::Access => 0,
        AccessRole::Viewer => 1,
        AccessRole::Operator => 2,
        AccessRole::Administrator => 3,
    }
}

fn role_name(role: &AccessRole) -> &'static str {
    match role {
        AccessRole::Access => "access",
        AccessRole::Viewer => "viewer",
        AccessRole::Operator => "operator",
        AccessRole::Administrator => "administrator",
    }
}

fn all_roles() -> Vec<AccessRole> {
    vec![
        AccessRole::Access,
        AccessRole::Viewer,
        AccessRole::Operator,
        AccessRole::Administrator,
    ]
}

fn stored_role(value: &str) -> Result<AccessRole, DatabaseError> {
    match value {
        "access" => Ok(AccessRole::Access),
        "viewer" => Ok(AccessRole::Viewer),
        "operator" => Ok(AccessRole::Operator),
        "administrator" => Ok(AccessRole::Administrator),
        _ => Err(domain_error(
            ErrorCode::InternalError,
            "stored access role is invalid",
        )),
    }
}

fn decode_invitation_grants(value: &str) -> Result<Vec<InvitationGrantParam>, DatabaseError> {
    let grants = serde_json::from_str::<Vec<InvitationGrantParam>>(value).map_err(|error| {
        DatabaseError::Domain(
            ProtocolError::new(
                ErrorCode::InternalError,
                "stored invitation grants are invalid",
            )
            .with_detail(error.to_string()),
        )
    })?;
    for grant in &grants {
        validate_grant(grant).map_err(DatabaseError::Domain)?;
    }
    Ok(grants)
}

fn random_id(prefix: char) -> Result<String, ProtocolError> {
    let mut bytes = [0_u8; 8];
    getrandom::fill(&mut bytes).map_err(|error| {
        ProtocolError::new(ErrorCode::InternalError, "cannot create an identifier")
            .with_detail(error.to_string())
    })?;
    let mut id = String::with_capacity(17);
    id.push(prefix);
    use std::fmt::Write;
    for byte in bytes {
        write!(id, "{byte:02x}").expect("String writes cannot fail");
    }
    Ok(id)
}

fn timestamp() -> Result<String, ProtocolError> {
    format_timestamp(OffsetDateTime::now_utc())
}

fn format_timestamp(value: OffsetDateTime) -> Result<String, ProtocolError> {
    value.format(TIMESTAMP_FORMAT).map_err(|error| {
        ProtocolError::new(ErrorCode::InternalError, "cannot format access timestamp")
            .with_detail(error.to_string())
    })
}

fn domain_error(code: ErrorCode, message: impl Into<String>) -> DatabaseError {
    DatabaseError::Domain(ProtocolError::new(code, message))
}

fn database_or_domain(error: DatabaseError) -> ProtocolError {
    match error {
        DatabaseError::Domain(error) => error,
        other => ProtocolError::new(ErrorCode::InternalError, "access database operation failed")
            .with_detail(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use tempfile::tempdir;

    #[derive(Default)]
    struct RecordingPublisher {
        sections: Mutex<Vec<RouteAccessSection>>,
    }

    impl RoutePublisher for RecordingPublisher {
        fn publish_access(&self, access: &RouteAccessSection) -> Result<(), ProtocolError> {
            self.sections
                .lock()
                .expect("publisher lock")
                .push(access.clone());
            Ok(())
        }
    }

    struct World {
        _temporary: tempfile::TempDir,
        database: Database,
        access: Access,
        publisher: Arc<RecordingPublisher>,
    }

    impl Drop for World {
        fn drop(&mut self) {
            let _ = self.database.close();
        }
    }

    fn world() -> World {
        let temporary = tempdir().expect("tempdir");
        let database = Database::open(temporary.path().join("authority.sqlite3")).expect("db");
        database
            .transaction(|transaction| {
                transaction.execute_batch(
                    "INSERT INTO repositories(repository_id,root_path,display_name,registered_at,registered_by_uid,last_seen_at)
                     VALUES('r1','/x','one','t',1,'t'),('r2','/y','two','t',1,'t');
                     INSERT INTO worktrees(worktree_id,repository_id,worktree_path,registered_at,last_seen_at)
                     VALUES('w1','r1','/x','t','t'),('w2','r2','/y','t','t');
                     INSERT INTO deployments(deployment_id,repository_id,worktree_id,name,source,domain,
                       spec_fingerprint,spec_json,state,created_at,created_by_uid,client,updated_at,current_generation)
                     VALUES
                       ('d1','r1','w1','one','worktree','one.example.test','f','{}','running','t',1,'other','t',1),
                       ('d2','r2','w2','two','worktree',NULL,'f','{}','running','t',1,'other','t',1);
                     INSERT INTO components(deployment_id,name,type,order_index,spec_fingerprint,
                       desired_state,state,health,generation,binding_kind,binding_identity,restarts,updated_at)
                     VALUES('d1','api','docker',0,'f','running','running','healthy',1,'container','c1',0,'t');
                     INSERT INTO observed_deployments(observed_deployment_id,repository_id,name,native_project,
                       state,health,source,evidence_json,observed_at,imported_at)
                     VALUES('d3','r1','observed','native','running','healthy','compose','{}','t','t');
                     INSERT INTO observed_containers(container_id,observed_deployment_id,repository_id,name,image,
                       compose_service,state,status,health,observed_at)
                     VALUES('c-observed','d3','r1','observed-api','image','api','running','up','healthy','t');
                     INSERT INTO tasks(task_id,repository_id,seq,position,title,outcome,kind,status,created_at,created_by,updated_at)
                     VALUES
                       ('p1','r1',1,1,'one','outcome one','goal','planned','t','t','t'),
                       ('p2','r2',1,1,'two','outcome two','goal','planned','t','t','t');",
                )?;
                Ok(())
            })
            .expect("fixture");
        let publisher = Arc::new(RecordingPublisher::default());
        let access = Access::from_settings(
            database.clone(),
            publisher.clone(),
            Some(999),
            "devcoordinator2-test".to_owned(),
            vec!["owner@example.test".to_owned()],
        )
        .expect("access");
        World {
            _temporary: temporary,
            database,
            access,
            publisher,
        }
    }

    fn local() -> Caller {
        Caller {
            pid: 1,
            uid: 1000,
            gid: 1000,
            client_kind: ClientKind::Other,
            client_session: None,
            work: None,
            identity: None,
        }
    }

    fn public(identity: &str) -> Caller {
        Caller {
            pid: 2,
            uid: 999,
            gid: 999,
            client_kind: ClientKind::Edge,
            client_session: None,
            work: None,
            identity: Some(identity.to_owned()),
        }
    }

    fn invite_and_accept(access: &Access, identity: &str, role: AccessRole, deployment_id: &str) {
        access
            .invite(
                InviteUser {
                    email: identity.to_owned(),
                    administrator: false,
                    grants: vec![InvitationGrantParam {
                        deployment_id: deployment_id.to_owned(),
                        role,
                    }],
                },
                &local(),
            )
            .expect("invite");
        let params = serde_json::json!({"email": identity});
        access
            .authorize("user.accept_invitation", &params, &public(identity))
            .expect("authorize acceptance");
        access
            .accept_invitation(AcceptInvitation {
                email: identity.to_owned(),
                subject: Some("subject".to_owned()),
                display_name: None,
            })
            .expect("accept");
    }

    #[test]
    fn repository_presentation_preserves_administrator_write_authority() {
        let world = world();
        let params = serde_json::json!({"repository_id":"r1111111111111111","display_name":"My project","icon":"code"});
        assert!(
            world
                .access
                .authorize("repository.presentation.update", &params, &local())
                .is_ok()
        );
        for (identity, role) in [
            ("viewer@example.test", AccessRole::Viewer),
            ("operator@example.test", AccessRole::Operator),
            ("deployment-admin@example.test", AccessRole::Administrator),
        ] {
            invite_and_accept(&world.access, identity, role, "d1");
            assert_eq!(
                world
                    .access
                    .authorize("repository.presentation.update", &params, &public(identity))
                    .unwrap_err()
                    .code,
                ErrorCode::PermissionDenied
            );
        }
    }

    #[test]
    fn event_visibility_is_recomputed_from_current_public_grants() {
        let world = world();
        invite_and_accept(
            &world.access,
            "viewer@example.test",
            AccessRole::Viewer,
            "d1",
        );
        let caller = public("viewer@example.test");
        let first = world.access.event_visibility(&caller).unwrap();
        assert!(!first.unrestricted);
        assert_eq!(first.repository_ids, BTreeSet::from(["r1".to_owned()]));
        assert_eq!(first.deployment_ids, BTreeSet::from(["d1".to_owned()]));
        world
            .access
            .remove_grant(
                RemoveGrant {
                    email: "viewer@example.test".into(),
                    deployment_id: "d1".into(),
                },
                &local(),
            )
            .unwrap();
        let revoked = world.access.event_visibility(&caller).unwrap();
        assert!(revoked.repository_ids.is_empty());
        assert!(revoked.deployment_ids.is_empty());
        assert!(
            world
                .access
                .event_visibility(&local())
                .unwrap()
                .unrestricted
        );
    }

    fn assert_permission_denied(result: Result<Authorization, ProtocolError>) {
        assert_eq!(
            result.expect_err("permission denied").code,
            ErrorCode::PermissionDenied
        );
    }

    #[test]
    fn edge_assertion_bootstrap_invitation_and_route_publication_match_contract() {
        let world = world();
        let spoof = Caller::from_client(
            3,
            998,
            998,
            ClientContext {
                kind: ClientKind::Edge,
                session: None,
                identity: Some("owner@example.test".to_owned()),
                work: Some(devcoordinator2_api::work_context::WorkContext::parse(r#"{"version":1,"native_project_id":"claimed-owner-project","thread_id":"thread"}"#).unwrap()),
                work_source: Some(devcoordinator2_api::work_context::WorkSource::Environment),
                ..ClientContext::default()
            },
            Some(999),
        )
        .expect_err("non-edge uid cannot assert identity");
        assert_eq!(spoof.code, ErrorCode::PermissionDenied);

        let local_identity = world.access.who_am_i(&local()).expect("local whoami");
        assert!(local_identity.local && local_identity.administrator);
        let users = world.access.list_users().expect("users");
        assert_eq!(users.owners, vec!["owner@example.test"]);
        assert!(world.publisher.sections.lock().expect("lock").is_empty());

        let invited = world
            .access
            .invite(
                InviteUser {
                    email: "Dev@Example.test".to_owned(),
                    administrator: false,
                    grants: vec![InvitationGrantParam {
                        deployment_id: "d1".to_owned(),
                        role: AccessRole::Operator,
                    }],
                },
                &local(),
            )
            .expect("invite");
        assert_eq!(invited.email, "dev@example.test");
        assert!(world.publisher.sections.lock().expect("lock").is_empty());

        assert_permission_denied(world.access.authorize(
            "user.accept_invitation",
            &serde_json::json!({"email":"dev@example.test"}),
            &public("someone@example.test"),
        ));
        world
            .access
            .authorize(
                "user.accept_invitation",
                &serde_json::json!({"email":"dev@example.test"}),
                &public("dev@example.test"),
            )
            .expect("identity accepts itself");
        let accepted = world
            .access
            .accept_invitation(AcceptInvitation {
                email: "dev@example.test".to_owned(),
                subject: Some("sub-1".to_owned()),
                display_name: Some("Dev".to_owned()),
            })
            .expect("accept");
        assert!(accepted.accepted);
        let who = world
            .access
            .who_am_i(&public("DEV@example.test"))
            .expect("public whoami");
        assert_eq!(who.identity.as_deref(), Some("dev@example.test"));
        assert_eq!(who.grants.get("d1"), Some(&AccessRole::Operator));

        let first = world.publisher.sections.lock().expect("lock")[0].clone();
        assert_eq!(first.owners, vec!["owner@example.test"]);
        assert_eq!(
            first.grants,
            vec![RouteGrant {
                identity: "dev@example.test".to_owned(),
                deployment_id: "d1".to_owned(),
                role: AccessRole::Operator,
            }]
        );

        world
            .access
            .set_grant(
                SetGrant {
                    email: "dev@example.test".to_owned(),
                    deployment_id: "d2".to_owned(),
                    role: AccessRole::Viewer,
                },
                &local(),
            )
            .expect("set grant");
        world
            .access
            .remove_grant(
                RemoveGrant {
                    email: "dev@example.test".to_owned(),
                    deployment_id: "d1".to_owned(),
                },
                &local(),
            )
            .expect("remove grant");
        let current = world.access.access_section().expect("access section");
        assert_eq!(current.grants.len(), 1);
        assert_eq!(current.grants[0].deployment_id, "d2");
        world
            .access
            .remove_user(
                EmailOnly {
                    email: "dev@example.test".to_owned(),
                },
                &local(),
            )
            .expect("remove user");
        assert!(
            world
                .access
                .access_section()
                .expect("section")
                .grants
                .is_empty()
        );
        assert_eq!(
            world
                .access
                .remove_user(
                    EmailOnly {
                        email: "dev@example.test".to_owned(),
                    },
                    &local(),
                )
                .expect_err("missing user")
                .code,
            ErrorCode::UserNotFound
        );
    }

    #[test]
    fn deployment_roles_are_rechecked_and_administrator_policies_remain_global() {
        let world = world();
        assert_permission_denied(world.access.authorize(
            "deployment.list",
            &serde_json::json!({}),
            &public("unknown@example.test"),
        ));
        world
            .access
            .authorize(
                "ping",
                &serde_json::json!({}),
                &public("unknown@example.test"),
            )
            .expect("anonymous public operation");
        assert_permission_denied(world.access.authorize(
            "bug.list",
            &serde_json::json!({}),
            &public("unknown@example.test"),
        ));
        invite_and_accept(
            &world.access,
            "viewer@example.test",
            AccessRole::Viewer,
            "d1",
        );
        let viewer = public("viewer@example.test");
        world
            .access
            .authorize("bug.list", &serde_json::json!({}), &viewer)
            .expect("admitted self operation");

        for (operation, params) in [
            ("config.get", serde_json::json!({})),
            (
                "config.env.set",
                serde_json::json!({"deployment_id":"d1","file":".env","authorized":true,"expected_revision":"revision"}),
            ),
            (
                "config.reload",
                serde_json::json!({"expected_revision":"revision"}),
            ),
            (
                "deployment.preflight",
                serde_json::json!({"deployment_id":"d1"}),
            ),
        ] {
            assert_permission_denied(world.access.authorize(operation, &params, &viewer));
            world
                .access
                .authorize(operation, &params, &local())
                .expect("existing local administrator");
        }

        let authorization = world
            .access
            .authorize(
                "deployment.list",
                &serde_json::json!({"path":"/x"}),
                &viewer,
            )
            .expect("list authorization");
        assert_eq!(authorization.params, serde_json::json!({}));
        let filtered = authorization
            .apply_result(serde_json::json!({
                "deployments":[
                    {"deployment_id":"d1","domain":"one.example.test"},
                    {"deployment_id":"d2","domain":null}
                ],
                "declared":[{"deployment_id":"d9"}]
            }))
            .expect("filtered list");
        assert_eq!(filtered["deployments"].as_array().expect("rows").len(), 1);
        assert!(
            filtered["declared"]
                .as_array()
                .expect("declared")
                .is_empty()
        );

        world
            .access
            .authorize(
                "deployment.status",
                &serde_json::json!({"deployment_id":"d1"}),
                &viewer,
            )
            .expect("viewer status");
        // d2 deliberately has no domain: absence of a public route must not
        // make an ungranted deployment visible through the Console API.
        assert_permission_denied(world.access.authorize(
            "deployment.status",
            &serde_json::json!({"deployment_id":"d2"}),
            &viewer,
        ));
        assert_permission_denied(world.access.authorize(
            "deployment.start",
            &serde_json::json!({"deployment_id":"d1"}),
            &viewer,
        ));

        world
            .access
            .set_grant(
                SetGrant {
                    email: "viewer@example.test".to_owned(),
                    deployment_id: "d1".to_owned(),
                    role: AccessRole::Administrator,
                },
                &local(),
            )
            .expect("promote deployment grant");
        world
            .access
            .authorize(
                "deployment.start",
                &serde_json::json!({"deployment_id":"d1"}),
                &viewer,
            )
            .expect("administrator grant includes operator");
        for (operation, params) in [
            (
                "deployment.apply",
                serde_json::json!({"deployment_id":"d1"}),
            ),
            (
                "grant.set",
                serde_json::json!({
                    "email":"viewer@example.test","deployment_id":"d1","role":"viewer"
                }),
            ),
            ("user.list", serde_json::json!({})),
        ] {
            assert_permission_denied(world.access.authorize(operation, &params, &viewer));
        }
        world
            .access
            .authorize(
                "user.list",
                &serde_json::json!({}),
                &public("owner@example.test"),
            )
            .expect("global administrator");

        world
            .access
            .remove_user(
                EmailOnly {
                    email: "viewer@example.test".to_owned(),
                },
                &local(),
            )
            .expect("revoke user");
        assert_permission_denied(world.access.authorize(
            "deployment.status",
            &serde_json::json!({"deployment_id":"d1"}),
            &viewer,
        ));
    }

    #[test]
    fn repository_and_health_resolution_filters_every_public_collection() {
        let world = world();
        invite_and_accept(
            &world.access,
            "viewer@example.test",
            AccessRole::Viewer,
            "d1",
        );
        let viewer = public("viewer@example.test");

        let plan = world
            .access
            .authorize("plan.overview", &serde_json::json!({}), &viewer)
            .expect("plan picker");
        let plan = plan
            .apply_result(serde_json::json!({
                "repositories":[{"repository_id":"r1"},{"repository_id":"r2"}]
            }))
            .expect("plan filter");
        assert_eq!(
            plan["repositories"],
            serde_json::json!([{"repository_id":"r1"}])
        );
        world
            .access
            .authorize(
                "plan.overview",
                &serde_json::json!({"repository_id":"r1"}),
                &viewer,
            )
            .expect("plan detail");
        assert_permission_denied(world.access.authorize(
            "plan.overview",
            &serde_json::json!({"path":"/x"}),
            &viewer,
        ));
        world
            .access
            .authorize(
                "task.history",
                &serde_json::json!({"task_id":"p1"}),
                &viewer,
            )
            .expect("task history");
        assert_permission_denied(world.access.authorize(
            "task.history",
            &serde_json::json!({"task_id":"p2"}),
            &viewer,
        ));

        world
            .access
            .authorize(
                "health.repository",
                &serde_json::json!({"path":"/x"}),
                &viewer,
            )
            .expect("registered repository health path");
        for denied_path in ["/y", "/x/symlink-or-subdirectory"] {
            assert_permission_denied(world.access.authorize(
                "health.repository",
                &serde_json::json!({"path":denied_path}),
                &viewer,
            ));
        }

        for (kind, subject_id) in [
            ("repository", "r1"),
            ("deployment", "d1"),
            ("component", "d1/api"),
            ("container", "c1"),
            ("deployment", "d3"),
            ("component", "d3/api"),
            ("container", "c-observed"),
            ("worktree", "w1"),
            (
                "test",
                "devcoordinator2-test-w1-t20260903T010203Z-a1b2c3.service",
            ),
        ] {
            world
                .access
                .authorize(
                    "health.history",
                    &serde_json::json!({
                        "subject_kind":kind,"subject_id":subject_id,"metric":"cpu_percent"
                    }),
                    &viewer,
                )
                .unwrap_or_else(|error| panic!("{kind}/{subject_id}: {error}"));
        }
        for (kind, subject_id) in [("repository", "r2"), ("deployment", "d2"), ("host", "host")] {
            assert_permission_denied(world.access.authorize(
                "health.history",
                &serde_json::json!({
                    "subject_kind":kind,"subject_id":subject_id,"metric":"cpu_percent"
                }),
                &viewer,
            ));
        }

        let health = world
            .access
            .authorize("health.repositories", &serde_json::json!({}), &viewer)
            .expect("health repositories")
            .apply_result(serde_json::json!({
                "repositories":[
                    {"repository_id":"r1","deployments":[
                        {"deployment_id":"d1"},{"deployment_id":"d2"}
                    ]},
                    {"repository_id":"r2","deployments":[{"deployment_id":"d2"}]}
                ],
                "host":{"private":true},
                "devcoordinator":{"private":true},
                "shared_unattributed":{"private":true}
            }))
            .expect("health filter");
        assert_eq!(health.as_object().expect("object").len(), 1);
        assert_eq!(health["repositories"].as_array().expect("repos").len(), 1);
        assert!(health["repositories"][0]["root_path"].is_null());
        assert_eq!(
            health["repositories"][0]["deployments"],
            serde_json::json!([
                {"deployment_id":"d1"}
            ])
        );

        assert_permission_denied(world.access.authorize(
            "usage.repositories",
            &serde_json::json!({}),
            &viewer,
        ));
        world
            .access
            .set_grant(
                SetGrant {
                    email: "viewer@example.test".to_owned(),
                    deployment_id: "d1".to_owned(),
                    role: AccessRole::Operator,
                },
                &local(),
            )
            .expect("operator");
        let usage = world
            .access
            .authorize("usage.repositories", &serde_json::json!({}), &viewer)
            .expect("usage collection")
            .apply_result(serde_json::json!({
                "range":"24h","generated_at_ms":1,
                "repositories":[{"repository_id":"r1"},{"repository_id":"r2"}]
            }))
            .expect("usage filter");
        assert_eq!(
            usage["repositories"],
            serde_json::json!([{"repository_id":"r1"}])
        );
        world
            .access
            .authorize(
                "usage.repository",
                &serde_json::json!({"repository_id":"r1"}),
                &viewer,
            )
            .expect("operator repository analytics");
        assert_permission_denied(world.access.authorize(
            "usage.repository",
            &serde_json::json!({"repository_id":"r2"}),
            &viewer,
        ));
        let progress = world
            .access
            .authorize("progress.repositories", &serde_json::json!({}), &viewer)
            .expect("progress collection")
            .apply_result(serde_json::json!({
                "repositories":[{"repository_id":"r1"},{"repository_id":"r2"}]
            }))
            .expect("progress filter");
        assert_eq!(
            progress["repositories"],
            serde_json::json!([{"repository_id":"r1"}])
        );
    }

    #[test]
    fn expired_invitation_is_atomically_consumed_and_filters_fail_closed() {
        let world = world();
        world
            .access
            .invite(
                InviteUser {
                    email: "expired@example.test".to_owned(),
                    administrator: false,
                    grants: Vec::new(),
                },
                &local(),
            )
            .expect("invite");
        world
            .database
            .transaction(|transaction| {
                transaction.execute(
                    "UPDATE invitations SET expires_at='2000-01-01T00:00:00Z' \
                     WHERE email='expired@example.test'",
                    [],
                )?;
                Ok(())
            })
            .expect("expire");
        assert_eq!(
            world
                .access
                .accept_invitation(AcceptInvitation {
                    email: "expired@example.test".to_owned(),
                    subject: None,
                    display_name: None,
                })
                .expect_err("expired")
                .code,
            ErrorCode::PermissionDenied
        );
        let invitations: i64 = world
            .database
            .call(|connection| {
                Ok(connection.query_row(
                    "SELECT count(*) FROM invitations WHERE email='expired@example.test'",
                    [],
                    |row| row.get(0),
                )?)
            })
            .expect("count");
        assert_eq!(invitations, 0);

        let filter = ResultFilter::RepositoryCollection {
            repository_ids: BTreeSet::from(["r1".to_owned()]),
        };
        assert_eq!(
            filter
                .apply(serde_json::json!({"repositories":[{"display_name":"missing id"}]}))
                .expect_err("must fail closed")
                .code,
            ErrorCode::InternalError
        );
    }
}
