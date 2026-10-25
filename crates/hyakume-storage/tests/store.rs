use hyakume_core::api::{CreateRelation, CreateRelationType, CreateRole, CreateTask, UpdateTask};
use hyakume_core::model::{BuiltinRelation, ProjectKey, Workspace};
use hyakume_core::permission::{MembershipKind, Permission};
use hyakume_storage::{Store, StoreError};

async fn open() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Store::open(&dir.path().join("work.db"))
        .await
        .expect("open store");
    (dir, store)
}

async fn workspace(store: &Store) -> Workspace {
    let owner = store.create_identity("owner", "Owner").await.unwrap();
    store
        .create_workspace("acme", "Acme", owner.id)
        .await
        .unwrap()
}

fn key(s: &str) -> ProjectKey {
    s.parse().expect("valid key")
}

fn titled(title: &str) -> CreateTask {
    CreateTask {
        title: title.into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn stale_update_is_rejected_with_the_latest_task_and_changes_nothing() {
    let (_dir, store) = open().await;
    let ws = workspace(&store).await;
    let project = store
        .create_project(ws.id, &key("WEB"), "Website")
        .await
        .unwrap();
    let task = store
        .create_task(project.id, titled("Draft"), None)
        .await
        .unwrap();

    // Alice saves first, moving the task from version 1 to 2.
    let mut alice = task.fields.clone();
    alice.title = "Alice's title".into();
    store
        .update_task(
            task.id,
            UpdateTask {
                expected_version: 1,
                fields: alice,
            },
            None,
        )
        .await
        .unwrap();

    // Bob still edits version 1.
    let mut bob = task.fields.clone();
    bob.description = "Bob's description".into();
    let err = store
        .update_task(
            task.id,
            UpdateTask {
                expected_version: 1,
                fields: bob.clone(),
            },
            None,
        )
        .await
        .unwrap_err();
    let StoreError::Conflict { latest, .. } = err else {
        panic!("expected a conflict, got {err:?}");
    };
    assert_eq!(latest.version, 2);
    assert_eq!(latest.fields.title, "Alice's title");
    assert_eq!(store.task(task.id).await.unwrap().fields.description, "");

    // Retrying against the latest version succeeds.
    let saved = store
        .update_task(
            task.id,
            UpdateTask {
                expected_version: 2,
                fields: bob,
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(saved.version, 3);
}

#[tokio::test]
async fn a_task_cannot_move_to_another_projects_stage() {
    let (_dir, store) = open().await;
    let ws = workspace(&store).await;
    let web = store
        .create_project(ws.id, &key("WEB"), "Website")
        .await
        .unwrap();
    let api = store
        .create_project(ws.id, &key("API"), "API")
        .await
        .unwrap();
    let task = store
        .create_task(web.id, titled("Task"), None)
        .await
        .unwrap();
    let foreign_stage = store.stages(api.id).await.unwrap()[0].id;

    let mut fields = task.fields.clone();
    fields.stage_id = foreign_stage;
    let err = store
        .update_task(
            task.id,
            UpdateTask {
                expected_version: 1,
                fields,
            },
            None,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Invalid(_)), "got {err:?}");
}

#[tokio::test]
async fn permissions_from_several_roles_add_up() {
    let (_dir, store) = open().await;
    let ws = workspace(&store).await;
    let bob = store.create_identity("bob", "Bob").await.unwrap();
    let roles = store.roles(ws.id).await.unwrap();
    let member = roles.iter().find(|r| r.name == "Member").unwrap().id;
    let linker = store
        .create_role(
            ws.id,
            CreateRole {
                name: "Linker".into(),
                permissions: vec![Permission::ManageRelations],
            },
        )
        .await
        .unwrap()
        .id;

    store
        .set_membership(ws.id, bob.id, MembershipKind::Member, false, &[member], &[])
        .await
        .unwrap();
    let before = store.access(ws.id, bob.id).await.unwrap().unwrap();
    assert!(!before.allows(Permission::ManageRelations, None));

    store
        .set_member_roles(ws.id, bob.id, &[member, linker])
        .await
        .unwrap();
    let after = store.access(ws.id, bob.id).await.unwrap().unwrap();
    assert!(after.allows(Permission::ManageRelations, None));
    assert!(after.allows(Permission::EditTasks, None));
}

#[tokio::test]
async fn a_role_cannot_share_a_name_with_another_as_typed() {
    let (_dir, store) = open().await;
    let ws = workspace(&store).await;
    let role = |name: &str| CreateRole {
        name: name.into(),
        permissions: vec![Permission::View],
    };

    // Assigning "admin" would otherwise pick the preset Admin.
    for name in ["admin", "MEMBER"] {
        let err = store.create_role(ws.id, role(name)).await.unwrap_err();
        assert!(
            matches!(err, StoreError::Duplicate(_)),
            "{name}: got {err:?}"
        );
    }
    store.create_role(ws.id, role("viewer")).await.unwrap();
}

#[tokio::test]
async fn snapshot_omits_links_into_projects_the_viewer_cannot_see() {
    let (_dir, store) = open().await;
    let ws = workspace(&store).await;
    let web = store
        .create_project(ws.id, &key("WEB"), "Website")
        .await
        .unwrap();
    let secret = store
        .create_project(ws.id, &key("SEC"), "Secret")
        .await
        .unwrap();
    let api = store
        .create_project(ws.id, &key("API"), "API")
        .await
        .unwrap();
    let page = store
        .create_task(web.id, titled("Page"), None)
        .await
        .unwrap();
    let hidden = store
        .create_task(secret.id, titled("Hidden"), None)
        .await
        .unwrap();
    let endpoint = store
        .create_task(api.id, titled("Endpoint"), None)
        .await
        .unwrap();
    let blocks = store
        .relation_types(ws.id)
        .await
        .unwrap()
        .into_iter()
        .find(|t| t.builtin == Some(BuiltinRelation::Blocks))
        .unwrap()
        .id;
    for from in [hidden.id, endpoint.id] {
        store
            .create_relation(
                ws.id,
                CreateRelation {
                    type_id: blocks,
                    from,
                    to: page.id,
                },
                None,
            )
            .await
            .unwrap();
    }

    let snapshot = store
        .project_snapshot(web.id, |p| p != secret.id)
        .await
        .unwrap();
    assert_eq!(snapshot.relations.len(), 1);
    assert_eq!(snapshot.relations[0].from, endpoint.id);
    let linked: Vec<_> = snapshot
        .linked_tasks
        .iter()
        .map(|t| t.reference.as_str())
        .collect();
    assert_eq!(linked, ["API-1"]);
}

#[tokio::test]
async fn relation_type_names_cannot_shadow_each_other_as_typed() {
    let (_dir, store) = open().await;
    let ws = workspace(&store).await;
    let custom = |name: &str, reverse: Option<&str>| CreateRelationType {
        name: name.into(),
        directed: true,
        reverse_name: reverse.map(Into::into),
    };

    for (name, reverse) in [
        ("Blocks", Some("held up by")),   // the built-in "blocks"
        ("holds up", Some("Blocked-By")), // the built-in's reverse name
        ("needs", Some("related_to")),    // the built-in "related to"
    ] {
        let err = store
            .create_relation_type(ws.id, custom(name, reverse))
            .await
            .unwrap_err();
        assert!(
            matches!(err, StoreError::Duplicate(_)),
            "{name}: got {err:?}"
        );
    }
    let err = store
        .create_relation_type(ws.id, custom("depends on", None))
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Invalid(_)), "got {err:?}");
    store
        .create_relation_type(ws.id, custom("depends on", Some("needed by")))
        .await
        .unwrap();
}
