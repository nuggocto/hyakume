use crate::ids::{RelationTypeId, TaskId};
use crate::model::Relation;

/// Unfinished tasks that block `task` through the built-in "blocks" type.
/// This only drives the blocked badge; it never stops a move.
/// A blocker we haven't loaded (`is_finished` gives `None`) counts as
/// unfinished, so missing data can't hide one.
pub fn unfinished_blockers(
    task: TaskId,
    relations: &[Relation],
    blocks_type: RelationTypeId,
    is_finished: impl Fn(TaskId) -> Option<bool>,
) -> Vec<TaskId> {
    relations
        .iter()
        .filter(|r| r.type_id == blocks_type && r.to == task)
        .map(|r| r.from)
        .filter(|blocker| is_finished(*blocker) != Some(true))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

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

        let mut result =
            unfinished_blockers(dependent, &relations, blocks, |t| finished.get(&t).copied());
        result.sort();
        let mut expected = vec![open_blocker, unloaded];
        expected.sort();
        assert_eq!(result, expected);
    }
}
