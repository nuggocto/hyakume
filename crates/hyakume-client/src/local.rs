//! Local personal work: the same schema as a server, holding one identity
//! (the person at this computer) who owns one personal workspace.

use hyakume_storage::{Store, StoreError};

use crate::{ClientError, OpenWorkspace};

const LOCAL_IDENTITY: &str = "me";
const LOCAL_WORKSPACE: &str = "personal";

/// Creates the local identity and workspace on first use.
pub async fn open(store: Store) -> Result<OpenWorkspace, ClientError> {
    let me = match store.identity_by_name(LOCAL_IDENTITY).await {
        Ok(me) => me,
        Err(StoreError::NotFound(_)) => store.create_identity(LOCAL_IDENTITY, "Me").await?,
        Err(err) => return Err(err.into()),
    };
    let workspace = match store.workspace_by_slug(LOCAL_WORKSPACE).await {
        Ok(workspace) => workspace,
        Err(StoreError::NotFound(_)) => {
            store
                .create_workspace(LOCAL_WORKSPACE, "Personal", me.id)
                .await?
        }
        Err(err) => return Err(err.into()),
    };
    Ok(OpenWorkspace::local(workspace, store, me))
}
