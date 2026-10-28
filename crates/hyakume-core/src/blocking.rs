use std::collections::HashMap;

use crate::ids::{RelationTypeId, TaskId};
use crate::model::Relation;

/// Unfinished "blocks" blockers of each task, for the badge only. A blocker
/// we haven't loaded (`is_finished` gives `None`) counts as unfinished. One
/// pass over the links: scanning them once per task cost 75 ms a rebuild at
/// 10,000 tasks.
pub fn unfinished_blockers(
    relations: &[Relation],
    blocks_type: RelationTypeId,
    is_finished: impl Fn(TaskId) -> Option<bool>,
) -> HashMap<TaskId, Vec<TaskId>> {
    let mut blocked: HashMap<TaskId, Vec<TaskId>> = HashMap::new();
    for r in relations.iter().filter(|r| r.type_id == blocks_type) {
        if is_finished(r.from) != Some(true) {
            blocked.entry(r.to).or_default().push(r.from);
        }
    }
    blocked
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{RelationId, WorkspaceId};

    fn relation(type_id: RelationTypeId, from: TaskId, to: TaskId) -> Relation {
        Relation {
            id: RelationId::new(),
            workspace_id: WorkspaceId::new(),
            type_id,
            from,
            to,
        }
    }

    #[test]
    fn only_unfinished_blockers_of_the_blocks_type_count() {
        let blocks = RelationTypeId::new();
        let related = RelationTypeId::new();
        let [
            dependent,
            done_blocker,
            open_blocker,
            merely_related,
            unloaded,
        ] = [(); 5].map(|_| TaskId::new());
        let relations = [
            relation(blocks, done_blocker, dependent),
            relation(blocks, open_blocker, dependent),
            relation(related, merely_related, dependent),
            relation(blocks, unloaded, dependent),
            // Direction matters: the dependent blocks someone else, which doesn't block it.
            relation(blocks, dependent, open_blocker),
        ];
        let finished: HashMap<TaskId, bool> = [
            (done_blocker, true),
            (open_blocker, false),
            (merely_related, false),
        ]
        .into();

        let mut blocked = unfinished_blockers(&relations, blocks, |t| finished.get(&t).copied());

        let mut result = blocked.remove(&dependent).unwrap_or_default();
        result.sort();
        let mut expected = vec![open_blocker, unloaded];
        expected.sort();
        assert_eq!(result, expected);
        assert_eq!(blocked.remove(&open_blocker), Some(vec![dependent]));
        assert!(blocked.is_empty(), "{blocked:?}");
    }
}
