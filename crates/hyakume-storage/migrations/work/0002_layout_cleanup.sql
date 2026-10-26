-- A layout row names a task or a project, but `node_id` can't be a foreign
-- key to both tables, so nothing removed a position when its node went.
-- Old rows piled up and every layout read sent them back to the client.

DELETE FROM layouts
WHERE (node_kind = 'task' AND node_id NOT IN (SELECT id FROM tasks))
   OR (node_kind = 'project' AND node_id NOT IN (SELECT id FROM projects));

CREATE TRIGGER layouts_forget_task AFTER DELETE ON tasks
BEGIN
    DELETE FROM layouts WHERE node_kind = 'task' AND node_id = OLD.id;
END;

-- A project's tasks go with it through ON DELETE CASCADE, which fires the
-- trigger above for each of them.
CREATE TRIGGER layouts_forget_project AFTER DELETE ON projects
BEGIN
    DELETE FROM layouts WHERE node_kind = 'project' AND node_id = OLD.id;
END;
