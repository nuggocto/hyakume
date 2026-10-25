//! HTTP and WebSocket client for one server's `/v1` API.

use std::time::Duration;

use futures_util::{Stream, StreamExt};
use hyakume_core::api::{
    ApiError, CreateComment, CreateProject, CreateRelation, CreateRelationType, CreateRole,
    CreateTask, DevLoginRequest, Event, Layout, LoginResponse, ProjectSnapshot, Role, ServerInfo,
    SetMemberRoles, TaskDetail, UpdateTask, WorkspaceListing, WorkspaceSummary,
};
use hyakume_core::ids::{IdentityId, ProjectId, RelationId, TaskId, WorkspaceId};
use hyakume_core::model::{Comment, Identity, Project, Relation, RelationType, Task, TaskRef};
use reqwest::{Method, RequestBuilder, Response};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest, http::HeaderValue};

use crate::ClientError;

#[derive(Clone, Debug)]
pub struct Remote {
    base: String,
    token: Option<String>,
    http: reqwest::Client,
}

impl Remote {
    pub fn new(url: &str, token: Option<String>) -> Result<Self, ClientError> {
        let base = url.trim_end_matches('/').to_owned();
        if !(base.starts_with("http://") || base.starts_with("https://")) {
            return Err(ClientError::Invalid(format!(
                "server URL {url:?} must start with http:// or https://"
            )));
        }
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| ClientError::Internal(e.to_string()))?;
        Ok(Self { base, token, http })
    }

    pub fn url(&self) -> &str {
        &self.base
    }

    pub fn has_session(&self) -> bool {
        self.token.is_some()
    }

    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        let builder = self.http.request(method, format!("{}{path}", self.base));
        match &self.token {
            Some(token) => builder.bearer_auth(token),
            None => builder,
        }
    }

    async fn send(&self, builder: RequestBuilder) -> Result<Response, ClientError> {
        let response = builder.send().await.map_err(|e| ClientError::Unreachable {
            server: self.base.clone(),
            reason: error_chain(&e.without_url()),
        })?;
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status();
        match response.json::<ApiError>().await {
            Ok(err) => Err(err.into()),
            Err(_) => Err(ClientError::Internal(format!(
                "{} answered {status} without an error body",
                self.base
            ))),
        }
    }

    async fn json<T: DeserializeOwned>(&self, builder: RequestBuilder) -> Result<T, ClientError> {
        self.send(builder)
            .await?
            .json()
            .await
            .map_err(|e| ClientError::Internal(format!("unexpected response: {e}")))
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, ClientError> {
        self.json(self.request(Method::GET, path)).await
    }

    async fn with_body<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: &impl Serialize,
    ) -> Result<T, ClientError> {
        self.json(self.request(method, path).json(body)).await
    }

    async fn empty(&self, builder: RequestBuilder) -> Result<(), ClientError> {
        self.send(builder).await.map(drop)
    }

    pub async fn info(&self) -> Result<ServerInfo, ClientError> {
        self.get("/v1/info").await
    }

    pub async fn dev_login(&self, identity: &str) -> Result<LoginResponse, ClientError> {
        let body = DevLoginRequest {
            identity: identity.to_owned(),
        };
        self.with_body(Method::POST, "/v1/dev/login", &body).await
    }

    pub async fn me(&self) -> Result<Identity, ClientError> {
        self.get("/v1/me").await
    }

    pub async fn workspaces(&self) -> Result<Vec<WorkspaceListing>, ClientError> {
        if self.token.is_none() {
            return Err(ClientError::Unauthorized(format!(
                "not signed in to {}; run `hyakume auth dev-login`",
                self.base
            )));
        }
        self.get("/v1/workspaces").await
    }

    pub async fn summary(&self, workspace: WorkspaceId) -> Result<WorkspaceSummary, ClientError> {
        self.get(&format!("/v1/workspaces/{workspace}")).await
    }

    pub async fn snapshot(&self, project: ProjectId) -> Result<ProjectSnapshot, ClientError> {
        self.get(&format!("/v1/projects/{project}")).await
    }

    pub async fn task_by_ref(
        &self,
        workspace: WorkspaceId,
        reference: &TaskRef,
    ) -> Result<TaskDetail, ClientError> {
        self.get(&format!("/v1/workspaces/{workspace}/tasks/{reference}"))
            .await
    }

    pub async fn task(&self, task: TaskId) -> Result<TaskDetail, ClientError> {
        self.get(&format!("/v1/tasks/{task}")).await
    }

    pub async fn create_project(
        &self,
        workspace: WorkspaceId,
        body: &CreateProject,
    ) -> Result<Project, ClientError> {
        self.with_body(
            Method::POST,
            &format!("/v1/workspaces/{workspace}/projects"),
            body,
        )
        .await
    }

    pub async fn create_task(
        &self,
        project: ProjectId,
        body: &CreateTask,
    ) -> Result<Task, ClientError> {
        self.with_body(Method::POST, &format!("/v1/projects/{project}/tasks"), body)
            .await
    }

    pub async fn update_task(&self, task: TaskId, body: &UpdateTask) -> Result<Task, ClientError> {
        self.with_body(Method::PUT, &format!("/v1/tasks/{task}"), body)
            .await
    }

    pub async fn delete_task(&self, task: TaskId) -> Result<(), ClientError> {
        self.empty(self.request(Method::DELETE, &format!("/v1/tasks/{task}")))
            .await
    }

    pub async fn add_comment(&self, task: TaskId, body: &str) -> Result<Comment, ClientError> {
        let body = CreateComment {
            body: body.to_owned(),
        };
        self.with_body(Method::POST, &format!("/v1/tasks/{task}/comments"), &body)
            .await
    }

    pub async fn create_relation(
        &self,
        workspace: WorkspaceId,
        body: &CreateRelation,
    ) -> Result<Relation, ClientError> {
        self.with_body(
            Method::POST,
            &format!("/v1/workspaces/{workspace}/relations"),
            body,
        )
        .await
    }

    pub async fn delete_relation(&self, relation: RelationId) -> Result<(), ClientError> {
        self.empty(self.request(Method::DELETE, &format!("/v1/relations/{relation}")))
            .await
    }

    pub async fn create_relation_type(
        &self,
        workspace: WorkspaceId,
        body: &CreateRelationType,
    ) -> Result<RelationType, ClientError> {
        self.with_body(
            Method::POST,
            &format!("/v1/workspaces/{workspace}/relation-types"),
            body,
        )
        .await
    }

    pub async fn create_role(
        &self,
        workspace: WorkspaceId,
        body: &CreateRole,
    ) -> Result<Role, ClientError> {
        self.with_body(
            Method::POST,
            &format!("/v1/workspaces/{workspace}/roles"),
            body,
        )
        .await
    }

    pub async fn set_member_roles(
        &self,
        workspace: WorkspaceId,
        identity: IdentityId,
        body: &SetMemberRoles,
    ) -> Result<(), ClientError> {
        let path = format!("/v1/workspaces/{workspace}/members/{identity}/roles");
        self.empty(self.request(Method::PUT, &path).json(body))
            .await
    }

    pub async fn layout(&self, workspace: WorkspaceId) -> Result<Layout, ClientError> {
        self.get(&format!("/v1/workspaces/{workspace}/layout"))
            .await
    }

    pub async fn put_layout(
        &self,
        workspace: WorkspaceId,
        body: &Layout,
    ) -> Result<(), ClientError> {
        let path = format!("/v1/workspaces/{workspace}/layout");
        self.empty(self.request(Method::PUT, &path).json(body))
            .await
    }

    /// Opens the change stream. The first item is `Event::Subscribed`; fetch
    /// data only after it arrives so no committed change slips between.
    pub async fn subscribe(
        &self,
    ) -> Result<impl Stream<Item = Result<Event, ClientError>> + use<>, ClientError> {
        let url = format!(
            "{}/v1/events",
            self.base
                .replacen("https://", "wss://", 1)
                .replacen("http://", "ws://", 1)
        );
        let mut request = url
            .into_client_request()
            .map_err(|e| ClientError::Invalid(e.to_string()))?;
        if let Some(token) = &self.token {
            let value = HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|_| ClientError::Credentials("saved token is malformed".into()))?;
            request.headers_mut().insert("authorization", value);
        }
        let server = self.base.clone();
        let (socket, _) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|e| match e {
                tungstenite::Error::Http(response) if response.status() == 401 => {
                    ClientError::Unauthorized(format!("session for {server} is not valid"))
                }
                other => ClientError::Unreachable {
                    server: server.clone(),
                    reason: other.to_string(),
                },
            })?;
        let server = self.base.clone();
        Ok(socket.filter_map(move |message| {
            let server = server.clone();
            async move {
                match message {
                    Ok(tungstenite::Message::Text(text)) => Some(
                        serde_json::from_str::<Event>(&text)
                            .map_err(|e| ClientError::Internal(format!("bad event: {e}"))),
                    ),
                    Ok(tungstenite::Message::Close(_)) => Some(Err(ClientError::Unreachable {
                        server,
                        reason: "the server closed the event stream".into(),
                    })),
                    Ok(_) => None,
                    Err(e) => Some(Err(ClientError::Unreachable {
                        server,
                        reason: e.to_string(),
                    })),
                }
            }
        }))
    }
}

/// reqwest's top-level message is just "error sending request"; the cause,
/// such as "Connection refused", is further down the chain.
fn error_chain(err: &dyn std::error::Error) -> String {
    let mut message = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}
