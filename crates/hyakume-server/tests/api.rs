//! Exercises the `/v1` API through the full router and middleware stack,
//! against a real migrated SQLite database.

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use hyakume_core::api::{
    ApiError, ErrorCode, LoginResponse, ProjectSnapshot, Role, WorkspaceListing, WorkspaceSummary,
};
use hyakume_core::model::{BuiltinRelation, Task};
use hyakume_server::config::Config;
use hyakume_server::{AppState, prepare, router};
use serde_json::{Value, json};
use tower::ServiceExt;

const DEV_CONFIG: &str = r#"
database = "server.db"
dev_mode = true

[[dev.identities]]
name = "alice"
[[dev.identities]]
name = "bob"
[[dev.identities]]
name = "gwen"

[[dev.workspaces]]
slug = "acme"
name = "Acme"
owner = "alice"
projects = [{ key = "WEB", name = "Website" }, { key = "API", name = "API" }]
members = [
  { identity = "bob", roles = ["Member"] },
  { identity = "gwen", roles = ["Guest"], guest_projects = ["WEB"] },
]
"#;

struct Server {
    _dir: tempfile::TempDir,
    app: Router,
}

async fn server(config_text: &str) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let mut config: Config = toml::from_str(config_text).unwrap();
    config.database = dir.path().join("server.db");
    let store = prepare(&config).await.unwrap();
    let (state, _shutdown) = AppState::new(store, &config);
    Server {
        _dir: dir,
        app: router(state),
    }
}

impl Server {
    async fn call(
        &self,
        method: Method,
        uri: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder().method(method).uri(uri);
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let request = match body {
            Some(body) => request
                .header("content-type", "application/json")
                .body(Body::from(body.to_string())),
            None => request.body(Body::empty()),
        }
        .unwrap();
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, value)
    }

    async fn login(&self, identity: &str) -> String {
        let (status, body) = self
            .call(
                Method::POST,
                "/v1/dev/login",
                None,
                Some(json!({ "identity": identity })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        serde_json::from_value::<LoginResponse>(body).unwrap().token
    }

    async fn summary(&self, token: &str) -> WorkspaceSummary {
        let (_, list) = self
            .call(Method::GET, "/v1/workspaces", Some(token), None)
            .await;
        let list: Vec<WorkspaceListing> = serde_json::from_value(list).unwrap();
        let id = list[0].workspace.id;
        let (status, body) = self
            .call(
                Method::GET,
                &format!("/v1/workspaces/{id}"),
                Some(token),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        serde_json::from_value(body).unwrap()
    }

    async fn create_task(&self, token: &str, project: &str, title: &str) -> Task {
        let (status, body) = self
            .call(
                Method::POST,
                &format!("/v1/projects/{project}/tasks"),
                Some(token),
                Some(json!({ "title": title })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        serde_json::from_value(body).unwrap()
    }
}

#[tokio::test]
async fn a_custom_role_lets_a_member_link_tasks_across_projects() {
    let server = server(DEV_CONFIG).await;
    let alice = server.login("alice").await;
    let bob = server.login("bob").await;
    let summary = server.summary(&alice).await;
    let ws = summary.workspace.id;
    let project = |key: &str| {
        summary
            .projects
            .iter()
            .find(|p| p.key.as_str() == key)
            .unwrap()
            .id
    };
    let page = server
        .create_task(&alice, &project("WEB").to_string(), "Landing page")
        .await;
    let endpoint = server
        .create_task(&alice, &project("API").to_string(), "Signup endpoint")
        .await;
    let blocks = summary
        .relation_types
        .iter()
        .find(|t| t.builtin == Some(BuiltinRelation::Blocks))
        .unwrap()
        .id;
    let link = json!({ "type_id": blocks, "from": endpoint.id, "to": page.id });
    let relations = format!("/v1/workspaces/{ws}/relations");

    // The Member preset does not include relationship management.
    let (status, body) = server
        .call(Method::POST, &relations, Some(&bob), Some(link.clone()))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    let (status, role) = server
        .call(
            Method::POST,
            &format!("/v1/workspaces/{ws}/roles"),
            Some(&alice),
            Some(json!({ "name": "Linker", "permissions": ["manage_relations"] })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{role}");
    let linker: Role = serde_json::from_value(role).unwrap();
    let member = summary
        .roles
        .iter()
        .find(|r| r.name == "Member")
        .unwrap()
        .id;
    let bob_id = summary
        .members
        .iter()
        .find(|m| m.identity.name == "bob")
        .unwrap()
        .identity
        .id;
    let (status, body) = server
        .call(
            Method::PUT,
            &format!("/v1/workspaces/{ws}/members/{bob_id}/roles"),
            Some(&alice),
            Some(json!({ "roles": [member, linker.id] })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    let (status, body) = server
        .call(Method::POST, &relations, Some(&bob), Some(link))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let (_, snapshot) = server
        .call(
            Method::GET,
            &format!("/v1/projects/{}", project("WEB")),
            Some(&bob),
            None,
        )
        .await;
    let snapshot: ProjectSnapshot = serde_json::from_value(snapshot).unwrap();
    assert_eq!(snapshot.relations.len(), 1);
    assert_eq!(snapshot.linked_tasks[0].reference, "API-1");
}

#[tokio::test]
async fn a_stale_save_gets_409_with_the_latest_task() {
    let server = server(DEV_CONFIG).await;
    let alice = server.login("alice").await;
    let bob = server.login("bob").await;
    let summary = server.summary(&alice).await;
    let web = summary
        .projects
        .iter()
        .find(|p| p.key.as_str() == "WEB")
        .unwrap()
        .id;
    let task = server
        .create_task(&alice, &web.to_string(), "Original")
        .await;
    let uri = format!("/v1/tasks/{}", task.id);
    let edit = |title: &str| {
        let mut body = serde_json::to_value(&task.fields).unwrap();
        body["title"] = json!(title);
        body["expected_version"] = json!(1);
        body
    };

    let (status, _) = server
        .call(Method::PUT, &uri, Some(&alice), Some(edit("Alice")))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = server
        .call(Method::PUT, &uri, Some(&bob), Some(edit("Bob")))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let error: ApiError = serde_json::from_value(body).unwrap();
    assert_eq!(error.code, ErrorCode::Conflict);
    let latest = error.latest.expect("conflict carries the latest task");
    assert_eq!((latest.version, latest.fields.title.as_str()), (2, "Alice"));
}

#[tokio::test]
async fn a_guest_sees_only_invited_projects() {
    let server = server(DEV_CONFIG).await;
    let alice = server.login("alice").await;
    let gwen = server.login("gwen").await;
    let api = server
        .summary(&alice)
        .await
        .projects
        .iter()
        .find(|p| p.key.as_str() == "API")
        .unwrap()
        .id;

    let visible: Vec<String> = server
        .summary(&gwen)
        .await
        .projects
        .iter()
        .map(|p| p.key.to_string())
        .collect();
    assert_eq!(visible, ["WEB"]);
    let (status, _) = server
        .call(
            Method::GET,
            &format!("/v1/projects/{api}"),
            Some(&gwen),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn requests_without_a_valid_token_are_rejected() {
    let server = server(DEV_CONFIG).await;
    let (status, _) = server.call(Method::GET, "/v1/workspaces", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = server
        .call(Method::GET, "/v1/workspaces", Some("hyk_forged"), None)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn dev_login_does_not_exist_outside_dev_mode() {
    let server = server(&DEV_CONFIG.replace("dev_mode = true", "dev_mode = false")).await;
    let (status, _) = server
        .call(
            Method::POST,
            "/v1/dev/login",
            None,
            Some(json!({ "identity": "alice" })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_failed_seed_leaves_nothing_half_created() {
    let dir = tempfile::tempdir().unwrap();
    let load = |text: &str| {
        let mut config: Config = toml::from_str(text).unwrap();
        config.database = dir.path().join("server.db");
        config
    };
    // A misspelled owner fails startup...
    assert!(
        prepare(&load(
            &DEV_CONFIG.replace("owner = \"alice\"", "owner = \"alcie\"")
        ))
        .await
        .is_err()
    );

    // ...and fixing the config gives a workspace that Alice owns.
    let config = load(DEV_CONFIG);
    let store = prepare(&config).await.unwrap();
    let alice = store.identity_by_name("alice").await.unwrap();
    let workspaces = store.workspaces_for(alice.id).await.unwrap();
    assert_eq!(workspaces.len(), 1);
    let access = store
        .access(workspaces[0].id, alice.id)
        .await
        .unwrap()
        .unwrap();
    assert!(access.is_owner);
}
