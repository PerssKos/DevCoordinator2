//! The actual source of an explicit candidate generation, independent of the
//! deployment's declared worktree, ownership, data and route identities.
use std::path::PathBuf;

use devcoordinator2_api::{ErrorCode, ProtocolError};

use crate::deployment_state::GenerationSourceOrigin;

pub(crate) fn resolve(
    origin: &GenerationSourceOrigin,
    repository_id: &str,
    uid: u32,
    gid: u32,
) -> Result<PathBuf, ProtocolError> {
    let invalid = || {
        ProtocolError::new(
            ErrorCode::DeploymentApplyFailed,
            "recorded candidate source origin is unavailable or no longer matches its repository/worktree identity",
        )
    };
    let resolved = crate::repository::resolve_worktree(&origin.path, Some((uid, gid)))
        .map_err(|_| invalid())?;
    if resolved.worktree_root != origin.path
        || crate::ids::repository_id(&resolved.repository_root).map_err(|_| invalid())?
            != repository_id
        || crate::ids::worktree_id(&resolved.worktree_root).map_err(|_| invalid())?
            != origin.worktree_id
    {
        return Err(invalid());
    }
    Ok(resolved.worktree_root)
}
