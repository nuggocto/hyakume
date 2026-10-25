//! Runs a real server on a loopback port and checks what reaches each
//! client's WebSocket.

use std::time::Duration;

use futures_util::{Stream, StreamExt};
use hyakume_client::config::{ClientConfig, CredentialStore, ServerEntry};
use hyakume_client::remote::Remote;
use hyakume_client::{Client, ClientError, DataDir, credentials};
use hyakume_core::api::{CreateTask, Event, Layout, LayoutNode, NodeKind};
use hyakume_core::ids::ProjectId;
use hyakume_server::config::Config;
use hyakume_server::{AppState, prepare, serve};

const CONFIG: &str = r#"
database = "unused.db"
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

async fn start() -> (tempfile::TempDir, String, tokio::sync::watch::Sender<bool>) {
    start_with(CONFIG).await
}

async fn start_with(text: &str) -> (tempfile::TempDir, String, tokio::sync::watch::Sender<bool>) {
    let dir = tempfile::tempdir().unwrap();
    let mut config: Config = toml::from_str(text).unwrap();
    config.database = dir.path().join("server.db");
    let store = prepare(&config).await.unwrap();
    let (state, shutdown) = AppState::new(store, &config);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let mut stop = shutdown.subscribe();
    tokio::spawn(serve(listener, state, async move {
        let _ = stop.changed().await;
    }));
    (dir, url, shutdown)
}

async fn login(url: &str, identity: &str) -> Remote {
    let token = Remote::new(url, None)
        .unwrap()
        .dev_login(identity)
        .await
        .unwrap()
        .token;
    Remote::new(url, Some(token)).unwrap()
}

/// Subscribes and waits for `Subscribed`, the point after which no change can be missed.
async fn subscribe(remote: &Remote) -> impl Stream<Item = Result<Event, ClientError>> + Unpin {
    let mut events = Box::pin(remote.subscribe().await.unwrap());
    assert_eq!(next(&mut events).await, Event::Subscribed);
    events
}

async fn next(events: &mut (impl Stream<Item = Result<Event, ClientError>> + Unpin)) -> Event {
    tokio::time::timeout(Duration::from_secs(5), events.next())
        .await
        .expect("an event within 5 s")
        .expect("stream still open")
        .expect("valid event")
}

fn changed(project: ProjectId) -> impl Fn(&Event) -> bool {
    move |e| matches!(e, Event::ProjectChanged { project_id, .. } if *project_id == project)
}

#[tokio::test]
async fn events_reach_only_people_allowed_to_see_them() {
    let (_dir, url, _shutdown) = start().await;
    let alice = login(&url, "alice").await;
    let alice_laptop = login(&url, "alice").await;
    let bob = login(&url, "bob").await;
    let gwen = login(&url, "gwen").await;
    let workspace = alice.workspaces().await.unwrap()[0].workspace.id;
    let projects = alice.summary(workspace).await.unwrap().projects;
    let project = |key: &str| projects.iter().find(|p| p.key.as_str() == key).unwrap().id;
    let (web, api) = (project("WEB"), project("API"));

    let mut alice_events = subscribe(&alice_laptop).await;
    let mut bob_events = subscribe(&bob).await;
    let mut gwen_events = subscribe(&gwen).await;

    // A change in API, which the guest cannot see, then one in WEB.
    let task = |title: &str| CreateTask {
        title: title.into(),
        ..Default::default()
    };
    bob.create_task(api, &task("Endpoint")).await.unwrap();
    // Alice moves a node: personal, so only her own devices hear about it.
    let layout = Layout {
        nodes: vec![LayoutNode {
            kind: NodeKind::Project,
            id: web.0,
            x: 10.0,
            y: 20.0,
            pinned: true,
        }],
    };
    alice.put_layout(workspace, &layout).await.unwrap();
    bob.create_task(web, &task("Page")).await.unwrap();

    assert!(changed(api)(&next(&mut alice_events).await));
    assert_eq!(
        next(&mut alice_events).await,
        Event::LayoutChanged {
            workspace_id: workspace
        }
    );
    assert!(changed(web)(&next(&mut alice_events).await));

    // Bob sees both project changes but not Alice's layout.
    assert!(changed(api)(&next(&mut bob_events).await));
    assert!(changed(web)(&next(&mut bob_events).await));

    // The guest's first event is the WEB change; the API change was filtered out.
    assert!(changed(web)(&next(&mut gwen_events).await));

    // The layout itself synced to Alice's other session and stayed hers.
    assert_eq!(
        alice_laptop.layout(workspace).await.unwrap().nodes,
        layout.nodes
    );
    assert!(bob.layout(workspace).await.unwrap().nodes.is_empty());
}

#[tokio::test]
async fn a_dev_file_session_is_refused_once_the_server_leaves_dev_mode() {
    let (_server_dir, url, _shutdown) =
        start_with(&CONFIG.replace("dev_mode = true", "dev_mode = false")).await;
    let profile = tempfile::tempdir().unwrap();
    let client = Client {
        data: DataDir::at(profile.path()),
        config: ClientConfig {
            credential_store: CredentialStore::DevFile,
            servers: vec![ServerEntry {
                name: "a".into(),
                url: url.clone(),
                identity: Some("alice".into()),
            }],
            ..Default::default()
        },
    };
    // A token saved while the server was still in dev mode.
    credentials::save(&client, &url, "alice", "hyk_saved", true).unwrap();

    let err = client.server("a").await.unwrap_err();
    assert!(matches!(err, ClientError::Credentials(_)), "got {err:?}");
}
