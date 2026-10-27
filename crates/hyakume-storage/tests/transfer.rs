use hyakume_core::api::{CreateRelation, CreateRelationType, CreateTask, LayoutNode, NodeKind};
use hyakume_core::export::LocalExport;
use hyakume_core::ids::{IdentityId, TaskId};
use hyakume_core::model::{BuiltinRelation, ProjectKey, Workspace};
use hyakume_storage::{Store, StoreError};

/// One computer's local work: its own database, person, and workspace.
async fn computer(dir: &tempfile::TempDir, name: &str) -> (Store, IdentityId, Workspace) {
    let store = Store::open(&dir.path().join(format!("{name}.db")))
        .await
        .expect("open store");
    let me = store.create_identity("me", "Me").await.unwrap();
    let ws = store
        .create_workspace("personal", "Personal", me.id)
        .await
        .unwrap();
    (store, me.id, ws)
}

fn key(s: &str) -> ProjectKey {
    s.parse().expect("valid key")
}

/// A laptop with two projects, a custom link type, a blocking link across
/// projects, comments, positions, and a deleted task.
async fn laptop(dir: &tempfile::TempDir) -> (Store, IdentityId, Workspace, LocalExport) {
    let (store, me, ws) = computer(dir, "laptop").await;
    let web = store
        .create_project(ws.id, &key("WEB"), "Website")
        .await
        .unwrap();
    let api = store
        .create_project(ws.id, &key("API"), "API")
        .await
        .unwrap();
    let task = |title: &str| CreateTask {
        title: title.into(),
        assignee: Some(me),
        labels: vec!["design, research".into()],
        ..Default::default()
    };
    let login = store
        .create_task(web.id, task("Login page"), Some(me))
        .await
        .unwrap();
    let gone = store
        .create_task(web.id, task("Scrapped"), Some(me))
        .await
        .unwrap();
    store.delete_task(gone.id).await.unwrap();
    let auth = store
        .create_task(api.id, task("Auth endpoint"), Some(me))
        .await
        .unwrap();
    store
        .add_comment(login.id, Some(me), "Needs the new logo")
        .await
        .unwrap();
    let types = store.relation_types(ws.id).await.unwrap();
    let blocks = types
        .iter()
        .find(|t| t.builtin == Some(BuiltinRelation::Blocks))
        .unwrap();
    store
        .create_relation(
            ws.id,
            CreateRelation {
                type_id: blocks.id,
                from: auth.id,
                to: login.id,
            },
            Some(me),
        )
        .await
        .unwrap();
    let mirrors = store
        .create_relation_type(
            ws.id,
            CreateRelationType {
                name: "mirrors".into(),
                directed: false,
                reverse_name: None,
            },
        )
        .await
        .unwrap();
    store
        .create_relation(
            ws.id,
            CreateRelation {
                type_id: mirrors.id,
                from: login.id,
                to: auth.id,
            },
            Some(me),
        )
        .await
        .unwrap();
    let at = |kind, id, x| LayoutNode {
        kind,
        id,
        x,
        y: 40.0,
        pinned: true,
    };
    store
        .put_layout(
            me,
            ws.id,
            &[
                at(NodeKind::Task, login.id.0, 12.5),
                at(NodeKind::Project, web.id.0, 300.0),
            ],
        )
        .await
        .unwrap();
    let export = store.export_workspace(ws.id, me).await.unwrap();
    (store, me, ws, export)
}

#[tokio::test]
async fn local_work_moves_to_another_computer_with_its_links_comments_and_positions() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (_, _, _, export) = laptop(&dir).await;
    // A file, not a value: what moves between computers is the JSON.
    let file = serde_json::to_string(&export).unwrap();
    let (desk, me, ws) = computer(&dir, "desktop").await;

    desk.import_workspace(ws.id, me, &serde_json::from_str(&file).unwrap(), false)
        .await
        .unwrap();

    let projects = desk.projects(ws.id).await.unwrap();
    let keys: Vec<&str> = projects.iter().map(|p| p.key.as_str()).collect();
    assert_eq!(keys, ["API", "WEB"]);
    let web = projects.iter().find(|p| p.key.as_str() == "WEB").unwrap();
    let snapshot = desk.project_snapshot(web.id, |_| true).await.unwrap();
    let login = &snapshot.tasks[0];
    assert_eq!(login.reference, "WEB-1");
    assert_eq!(login.fields.labels, ["design, research"]);
    // Everything that was the laptop's person is this computer's now.
    assert_eq!(login.fields.assignee, Some(me));
    let comments = desk.comments(login.id).await.unwrap();
    assert_eq!(comments[0].body, "Needs the new logo");
    assert_eq!(comments[0].author, Some(me));

    // Built-in types have their own ids here, and the link still blocks.
    let blocks = desk
        .relation_types(ws.id)
        .await
        .unwrap()
        .into_iter()
        .find(|t| t.builtin == Some(BuiltinRelation::Blocks))
        .unwrap();
    assert_eq!(snapshot.relations.len(), 2);
    assert!(
        snapshot
            .relations
            .iter()
            .any(|r| r.type_id == blocks.id && r.to == login.id)
    );

    let layout = desk.layout(me, ws.id).await.unwrap().nodes;
    assert_eq!(layout.len(), 2);
    assert!(layout.iter().any(|n| n.id == login.id.0 && n.x == 12.5));

    // WEB-2 was deleted on the laptop; its number stays used.
    let next = desk
        .create_task(
            web.id,
            CreateTask {
                title: "New".into(),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(next.reference, "WEB-3");
}

#[tokio::test]
async fn an_import_replaces_local_work_only_when_asked_and_never_half_way() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (_, _, _, export) = laptop(&dir).await;
    let (desk, me, ws) = computer(&dir, "desktop").await;
    let own = desk
        .create_project(ws.id, &key("HOME"), "Home")
        .await
        .unwrap();
    let projects = |store: Store| async move {
        let list = store.projects(ws.id).await.unwrap();
        list.into_iter()
            .map(|p| p.key.to_string())
            .collect::<Vec<_>>()
    };

    let refused = desk.import_workspace(ws.id, me, &export, false).await;
    assert!(
        matches!(refused, Err(StoreError::Duplicate(_))),
        "{refused:?}"
    );
    assert_eq!(projects(desk.clone()).await, ["HOME"]);

    // A link to a task the file doesn't hold: nothing goes in, nothing goes.
    let mut broken = export.clone();
    broken.relations[0].to = TaskId::new();
    assert!(
        desk.import_workspace(ws.id, me, &broken, true)
            .await
            .is_err()
    );
    assert_eq!(projects(desk.clone()).await, ["HOME"]);

    desk.import_workspace(ws.id, me, &export, true)
        .await
        .unwrap();
    assert_eq!(projects(desk.clone()).await, ["API", "WEB"]);
    assert!(desk.project(own.id).await.is_err());
}
