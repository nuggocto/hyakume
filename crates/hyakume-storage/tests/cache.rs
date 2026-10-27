use hyakume_storage::Cache;

fn pair(id: &str, body: &str) -> (String, String) {
    (id.to_owned(), body.to_owned())
}

#[tokio::test]
async fn a_new_copy_replaces_the_old_one_and_never_another_sources() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cache = Cache::open(&dir.path().join("cache.db")).await.unwrap();
    let alice = "http://a alice";
    let bob = "http://a bob";
    cache
        .replace(alice, &[pair("w1", "acme v1"), pair("w2", "studio")], &[])
        .await
        .unwrap();
    cache
        .replace(bob, &[pair("w1", "acme as bob")], &[])
        .await
        .unwrap();

    // Alice lost access to w2, so her next copy doesn't list it.
    cache
        .replace(alice, &[pair("w1", "acme v2")], &[])
        .await
        .unwrap();

    assert_eq!(cache.load(alice).await.unwrap(), ["acme v2"]);
    assert_eq!(cache.load(bob).await.unwrap(), ["acme as bob"]);
    assert!(cache.load("http://b alice").await.unwrap().is_empty());
}

#[tokio::test]
async fn comments_go_once_their_task_leaves_the_copy() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cache = Cache::open(&dir.path().join("cache.db")).await.unwrap();
    let alice = "http://a alice";
    let bob = "http://a bob";
    let tasks = |ids: &[&str]| ids.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    for (source, task) in [(alice, "t1"), (alice, "t2"), (bob, "t2")] {
        cache
            .save_comments(source, task, &format!("{source} {task}"))
            .await
            .unwrap();
    }

    // t2 was deleted, or moved somewhere Alice can't see.
    cache
        .replace(alice, &[pair("w1", "acme")], &tasks(&["t1"]))
        .await
        .unwrap();

    let body = |source, task| {
        let cache = cache.clone();
        async move { cache.comments(source, task).await.unwrap().map(|(b, _)| b) }
    };
    assert_eq!(
        body(alice, "t1").await.as_deref(),
        Some("http://a alice t1")
    );
    assert_eq!(body(alice, "t2").await, None);
    assert_eq!(body(bob, "t2").await.as_deref(), Some("http://a bob t2"));
}
