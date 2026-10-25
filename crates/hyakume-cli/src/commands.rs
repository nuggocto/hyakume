use std::io::{BufRead, IsTerminal, Write};

use hyakume_client::config::ServerEntry;
use hyakume_client::remote::Remote;
use hyakume_client::{Client, ClientError, DataDir, OpenWorkspace, credentials};
use hyakume_core::api::{
    CreateProject, CreateRelation, CreateRelationType, CreateRole, CreateTask, ProjectSnapshot,
    Role, SetMemberRoles, UpdateTask, WorkspaceSummary,
};
use hyakume_core::ids::IdentityId;
use hyakume_core::model::{
    Priority, Project, RelationType, Stage, TaskFields, TaskRef, name_key, relation_endpoints,
};
use hyakume_core::permission::Permission;
use serde_json::json;
use time::Date;
use time::macros::format_description;

use crate::cli::*;
use crate::output::{
    Output, ProjectContext, TaskDetailView, TaskView, task_detail_text, task_line,
};

pub async fn run(cli: Cli) -> Result<(), ClientError> {
    let mut client = Client::load(DataDir::resolve()?)?;
    let out = Output { json: cli.json };
    match cli.command {
        Command::Server(command) => server(&mut client, command, &out),
        Command::Auth(command) => auth(&mut client, command, &out).await,
        Command::Workspace(command) => workspace(&mut client, command, &out).await,
        command => {
            let ws = client.open(cli.workspace.as_deref()).await?;
            let summary = ws.summary().await?;
            let ctx = Ctx { ws, summary, out };
            work(&ctx, command).await
        }
    }
}

fn server(client: &mut Client, command: ServerCommand, out: &Output) -> Result<(), ClientError> {
    match command {
        ServerCommand::Add { name, url } => {
            if name == hyakume_client::LOCAL || name.contains('/') {
                return Err(ClientError::Invalid(format!(
                    "server name {name:?} must not be `local` or contain `/`"
                )));
            }
            let remote = Remote::new(&url, None)?;
            if client.config.servers.iter().any(|s| s.name == name) {
                return Err(ClientError::Duplicate(format!(
                    "server {name:?} already exists"
                )));
            }
            client.config.servers.push(ServerEntry {
                name: name.clone(),
                url: remote.url().to_owned(),
                identity: None,
            });
            client.save_config()?;
            out.show(&json!({ "name": name, "url": remote.url() }), || {
                format!("Added server {name} at {}", remote.url())
            });
        }
        ServerCommand::List => {
            let servers = &client.config.servers;
            out.show(servers, || {
                servers
                    .iter()
                    .map(|s| {
                        let who = s.identity.as_deref().unwrap_or("not signed in");
                        format!("{:<12} {}  ({who})\n", s.name, s.url)
                    })
                    .collect()
            });
        }
        ServerCommand::Remove { name } => {
            let entry = client.config.server(&name)?.clone();
            if let Some(identity) = &entry.identity {
                credentials::delete(client, &entry.url, identity)?;
            }
            client.config.servers.retain(|s| s.name != name);
            if client
                .config
                .default_workspace
                .as_deref()
                .is_some_and(|d| d.starts_with(&format!("{name}/")))
            {
                client.config.default_workspace = None;
            }
            client.save_config()?;
            out.show(&json!({ "removed": name }), || {
                format!("Removed server {name}")
            });
        }
    }
    Ok(())
}

async fn auth(client: &mut Client, command: AuthCommand, out: &Output) -> Result<(), ClientError> {
    match command {
        AuthCommand::DevLogin { identity, server } => {
            let entry = client.config.server(&server)?.clone();
            let remote = Remote::new(&entry.url, None)?;
            let info = remote.info().await?;
            if !info.dev_mode {
                return Err(ClientError::Invalid(format!(
                    "{server} is not running in dev mode, so development login is disabled there"
                )));
            }
            let login = remote.dev_login(&identity).await?;
            credentials::save(client, &entry.url, &identity, &login.token, info.dev_mode)?;
            client.config.server_mut(&server)?.identity = Some(identity.clone());
            client.save_config()?;
            out.show(
                &json!({ "server": server, "identity": login.identity }),
                || {
                    format!(
                        "Signed in to {server} as {} (development login)",
                        login.identity.name
                    )
                },
            );
        }
        AuthCommand::Logout { server } => {
            let entry = client.config.server(&server)?.clone();
            if let Some(identity) = &entry.identity {
                credentials::delete(client, &entry.url, identity)?;
            }
            client.config.server_mut(&server)?.identity = None;
            client.save_config()?;
            out.show(&json!({ "server": server }), || {
                format!("Signed out of {server}")
            });
        }
        AuthCommand::Status => {
            let mut rows = Vec::new();
            for entry in client.config.servers.clone() {
                let status = match client.server(&entry.name).await {
                    Ok(remote) if remote.has_session() => match remote.me().await {
                        Ok(me) => format!("signed in as {}", me.name),
                        Err(ClientError::Unauthorized(_)) => "session expired".into(),
                        Err(err) => format!("unknown ({err})"),
                    },
                    Ok(_) => "not signed in".into(),
                    Err(err) => format!("unknown ({err})"),
                };
                rows.push(json!({ "server": entry.name, "url": entry.url, "status": status }));
            }
            out.show(&rows, || {
                rows.iter()
                    .map(|r| {
                        format!(
                            "{:<12} {}\n",
                            r["server"].as_str().unwrap_or(""),
                            r["status"].as_str().unwrap_or("")
                        )
                    })
                    .collect()
            });
        }
    }
    Ok(())
}

async fn workspace(
    client: &mut Client,
    command: WorkspaceCommand,
    out: &Output,
) -> Result<(), ClientError> {
    match command {
        WorkspaceCommand::List => {
            let (listings, failures) = client.list().await;
            for (source, err) in &failures {
                eprintln!("warning: {source}: {err}");
            }
            let default = client
                .config
                .default_workspace
                .clone()
                .unwrap_or_else(|| hyakume_client::LOCAL.into());
            let rows: Vec<_> = listings
                .iter()
                .map(|l| {
                    json!({
                        "selector": l.selector,
                        "name": l.workspace.name,
                        "id": l.workspace.id,
                        "default": l.selector == default,
                    })
                })
                .collect();
            out.show(&rows, || {
                listings
                    .iter()
                    .map(|l| {
                        let mark = if l.selector == default { "*" } else { " " };
                        format!("{mark} {:<20} {}\n", l.selector, l.workspace.name)
                    })
                    .collect()
            });
        }
        WorkspaceCommand::Use { selector } => {
            let ws = client.open(Some(&selector)).await?;
            client.config.default_workspace = Some(ws.selector.clone());
            client.save_config()?;
            out.show(&json!({ "default": ws.selector }), || {
                format!(
                    "Default workspace is now {} ({})",
                    ws.selector, ws.workspace.name
                )
            });
        }
    }
    Ok(())
}

struct Ctx {
    ws: OpenWorkspace,
    summary: WorkspaceSummary,
    out: Output,
}

impl Ctx {
    fn project(&self, key: &str) -> Result<&Project, ClientError> {
        self.summary
            .projects
            .iter()
            .find(|p| p.key.as_str().eq_ignore_ascii_case(key))
            .ok_or_else(|| ClientError::NotFound(format!("project {key}")))
    }

    /// `me` means the signed-in identity.
    async fn member(&self, name: &str) -> Result<IdentityId, ClientError> {
        if name == "me" {
            return Ok(self.ws.me().await?.id);
        }
        self.summary
            .members
            .iter()
            .find(|m| m.identity.name == name)
            .map(|m| m.identity.id)
            .ok_or_else(|| ClientError::NotFound(format!("member {name}")))
    }

    async fn detail(
        &self,
        reference: &str,
    ) -> Result<(TaskRef, hyakume_core::api::TaskDetail), ClientError> {
        let reference: TaskRef =
            reference
                .parse()
                .map_err(|e: hyakume_core::model::ValidationError| {
                    ClientError::Invalid(e.to_string())
                })?;
        let detail = self.ws.task_by_ref(&reference).await?;
        Ok((reference, detail))
    }

    async fn snapshot_of(&self, project: &Project) -> Result<ProjectSnapshot, ClientError> {
        self.ws.snapshot(project.id).await
    }

    /// Resolves a relation type by name, or by its reverse name, in which
    /// case the caller must swap the ends.
    fn relation_type(&self, name: &str) -> Result<(&RelationType, bool), ClientError> {
        let wanted = name_key(name);
        let mut matches = Vec::new();
        for kind in &self.summary.relation_types {
            if name_key(&kind.name) == wanted {
                matches.push((kind, false));
            }
            if kind.reverse_name.as_deref().map(name_key).as_deref() == Some(wanted.as_str()) {
                matches.push((kind, true));
            }
        }
        // The server prevents collisions, but never guess if older data has one.
        match matches.as_slice() {
            [single] => Ok(*single),
            [] => Err(ClientError::NotFound(format!("relation type {name:?}"))),
            _ => Err(ClientError::Invalid(format!(
                "{name:?} matches more than one relation type; rename one of them"
            ))),
        }
    }

    fn role(&self, name: &str) -> Result<&Role, ClientError> {
        let wanted = name_key(name);
        let matches: Vec<_> = self
            .summary
            .roles
            .iter()
            .filter(|r| name_key(&r.name) == wanted)
            .collect();
        // Same as relation types: never guess between two roles.
        match matches.as_slice() {
            [single] => Ok(single),
            [] => Err(ClientError::NotFound(format!("role {name:?}"))),
            _ => Err(ClientError::Invalid(format!(
                "{name:?} matches more than one role; rename one of them"
            ))),
        }
    }

    async fn save(
        &self,
        task_id: hyakume_core::ids::TaskId,
        expected_version: i64,
        fields: TaskFields,
    ) -> Result<TaskView, ClientError> {
        let task = self
            .ws
            .update_task(
                task_id,
                UpdateTask {
                    expected_version,
                    fields,
                },
            )
            .await?;
        let project = self
            .summary
            .projects
            .iter()
            .find(|p| p.id == task.project_id)
            .ok_or_else(|| ClientError::NotFound("project".into()))?;
        let snapshot = self.snapshot_of(project).await?;
        Ok(ProjectContext::new(&self.summary, &snapshot).task_view(&task))
    }
}

fn stage<'a>(stages: &'a [Stage], name: &str) -> Result<&'a Stage, ClientError> {
    let wanted = name_key(name);
    stages
        .iter()
        .find(|s| name_key(&s.name) == wanted)
        .ok_or_else(|| {
            let names: Vec<_> = stages.iter().map(|s| s.name.as_str()).collect();
            ClientError::NotFound(format!("stage {name:?} (stages: {})", names.join(", ")))
        })
}

fn parse_priority(s: &str) -> Result<Priority, ClientError> {
    s.parse()
        .map_err(|e: hyakume_core::model::ValidationError| ClientError::Invalid(e.to_string()))
}

fn parse_date(s: &str) -> Result<Date, ClientError> {
    Date::parse(s, format_description!("[year]-[month]-[day]"))
        .map_err(|_| ClientError::Invalid(format!("{s:?} is not a date like 2026-11-01")))
}

/// Destructive commands ask on a terminal and require `--yes` otherwise.
fn confirm(question: &str, yes: bool) -> Result<(), ClientError> {
    if yes {
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        return Err(ClientError::Invalid(
            "refusing to delete without --yes when not run from a terminal".into(),
        ));
    }
    eprint!("{question} [y/N] ");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut answer)
        .map_err(|e| ClientError::Internal(e.to_string()))?;
    if matches!(answer.trim(), "y" | "Y" | "yes") {
        Ok(())
    } else {
        Err(ClientError::Invalid("cancelled".into()))
    }
}

async fn work(ctx: &Ctx, command: Command) -> Result<(), ClientError> {
    let out = &ctx.out;
    match command {
        Command::Project(ProjectCommand::List) => {
            let projects = &ctx.summary.projects;
            out.show(projects, || {
                projects
                    .iter()
                    .map(|p| format!("{:<8} {}\n", p.key, p.name))
                    .collect()
            });
        }
        Command::Project(ProjectCommand::Create { key, name }) => {
            let key = key
                .parse()
                .map_err(|e: hyakume_core::model::ValidationError| {
                    ClientError::Invalid(e.to_string())
                })?;
            let project = ctx.ws.create_project(CreateProject { key, name }).await?;
            out.show(&project, || {
                format!("Created project {} {}", project.key, project.name)
            });
        }
        Command::Stage(StageCommand::List { project }) => {
            let snapshot = ctx.snapshot_of(ctx.project(&project)?).await?;
            out.show(&snapshot.stages, || {
                snapshot
                    .stages
                    .iter()
                    .map(|s| {
                        format!(
                            "{}{}\n",
                            s.name,
                            if s.finished {
                                "  (counts as finished)"
                            } else {
                                ""
                            }
                        )
                    })
                    .collect()
            });
        }
        Command::Task(command) => task(ctx, command).await?,
        Command::Relation(command) => relation(ctx, command).await?,
        Command::RelationType(RelationTypeCommand::List) => {
            let kinds = &ctx.summary.relation_types;
            out.show(kinds, || {
                kinds
                    .iter()
                    .map(|k| {
                        let shape = match (&k.reverse_name, k.directed) {
                            (Some(reverse), _) => format!("directed, reverse \"{reverse}\""),
                            (None, true) => "directed".into(),
                            (None, false) => "undirected".into(),
                        };
                        let builtin = if k.builtin.is_some() {
                            ", built in"
                        } else {
                            ""
                        };
                        format!("{:<16} {shape}{builtin}\n", k.name)
                    })
                    .collect()
            });
        }
        Command::RelationType(RelationTypeCommand::Create {
            name,
            directed,
            reverse,
        }) => {
            let kind = ctx
                .ws
                .create_relation_type(CreateRelationType {
                    name,
                    directed,
                    reverse_name: reverse,
                })
                .await?;
            out.show(&kind, || format!("Created relation type \"{}\"", kind.name));
        }
        Command::Role(RoleCommand::List) => {
            let roles = &ctx.summary.roles;
            out.show(roles, || {
                roles
                    .iter()
                    .map(|r| {
                        let permissions: Vec<_> =
                            r.permissions.iter().map(|p| p.as_str()).collect();
                        format!("{:<10} {}\n", r.name, permissions.join(", "))
                    })
                    .collect()
            });
        }
        Command::Role(RoleCommand::Create { name, permissions }) => {
            let permissions = permissions
                .iter()
                .map(|p| p.parse::<Permission>())
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| {
                    let known: Vec<_> = Permission::ALL.iter().map(|p| p.as_str()).collect();
                    ClientError::Invalid(format!("{e} (known: {})", known.join(", ")))
                })?;
            let role = ctx.ws.create_role(CreateRole { name, permissions }).await?;
            out.show(&role, || format!("Created role {}", role.name));
        }
        Command::Member(MemberCommand::List) => {
            let members = &ctx.summary.members;
            out.show(members, || {
                members
                    .iter()
                    .map(|m| {
                        let roles: Vec<_> = m
                            .roles
                            .iter()
                            .filter_map(|id| ctx.summary.roles.iter().find(|r| r.id == *id))
                            .map(|r| r.name.as_str())
                            .collect();
                        let owner = if m.is_owner { " (owner)" } else { "" };
                        format!(
                            "{:<12} {:<7} {}{owner}\n",
                            m.identity.name,
                            m.kind.as_str(),
                            roles.join(", ")
                        )
                    })
                    .collect()
            });
        }
        Command::Member(MemberCommand::Roles { identity, roles }) => {
            let member = ctx.member(&identity).await?;
            let roles = roles
                .iter()
                .map(|name| ctx.role(name))
                .collect::<Result<Vec<_>, _>>()?;
            let ids = roles.iter().map(|r| r.id).collect();
            ctx.ws
                .set_member_roles(member, SetMemberRoles { roles: ids })
                .await?;
            // Print the stored names, so a typed "admin" shows up as "Admin".
            let names: Vec<_> = roles.iter().map(|r| r.name.as_str()).collect();
            out.show(&json!({ "identity": identity, "roles": names }), || {
                format!("{identity} now has roles: {}", names.join(", "))
            });
        }
        Command::Server(_) | Command::Auth(_) | Command::Workspace(_) => {
            unreachable!("handled before opening a workspace")
        }
    }
    Ok(())
}

async fn task(ctx: &Ctx, command: TaskCommand) -> Result<(), ClientError> {
    let out = &ctx.out;
    match command {
        TaskCommand::List(filter) => {
            let projects: Vec<&Project> = match &filter.project {
                Some(key) => vec![ctx.project(key)?],
                None => ctx.summary.projects.iter().collect(),
            };
            let assignee = match (&filter.assignee, filter.mine) {
                (Some(name), _) => Some(ctx.member(name).await?),
                (None, true) => Some(ctx.ws.me().await?.id),
                (None, false) => None,
            };
            let mut views = Vec::new();
            for project in projects {
                let snapshot = ctx.snapshot_of(project).await?;
                let stage_filter = filter
                    .stage
                    .as_deref()
                    .map(|s| stage(&snapshot.stages, s))
                    .transpose()?;
                let pc = ProjectContext::new(&ctx.summary, &snapshot);
                views.extend(
                    snapshot
                        .tasks
                        .iter()
                        .filter(|t| stage_filter.is_none_or(|s| t.fields.stage_id == s.id))
                        .filter(|t| assignee.is_none_or(|a| t.fields.assignee == Some(a)))
                        .map(|t| pc.task_view(t)),
                );
            }
            out.show(&views, || {
                views.iter().map(|v| task_line(v) + "\n").collect()
            });
        }
        TaskCommand::Show { reference } => {
            let (_, detail) = ctx.detail(&reference).await?;
            let project = ctx
                .summary
                .projects
                .iter()
                .find(|p| p.id == detail.task.project_id)
                .ok_or_else(|| ClientError::NotFound("project".into()))?;
            let snapshot = ctx.snapshot_of(project).await?;
            let pc = ProjectContext::new(&ctx.summary, &snapshot);
            let view = TaskDetailView {
                relations: pc.relations_of(detail.task.id),
                comments: detail.comments.iter().map(|c| pc.comment_view(c)).collect(),
                view: pc.task_view(&detail.task),
            };
            out.show(&view, || task_detail_text(&view));
        }
        TaskCommand::Create(args) => {
            let project = ctx.project(&args.project)?;
            let stage_id = match &args.stage {
                Some(name) => Some(stage(&ctx.snapshot_of(project).await?.stages, name)?.id),
                None => None,
            };
            let assignee = match &args.assignee {
                Some(name) => Some(ctx.member(name).await?),
                None => None,
            };
            let body = CreateTask {
                title: args.title,
                description: args.description,
                stage_id,
                assignee,
                labels: args.labels,
                priority: args.priority.as_deref().map(parse_priority).transpose()?,
                due_date: args.due.as_deref().map(parse_date).transpose()?,
            };
            let task = ctx.ws.create_task(project.id, body).await?;
            let snapshot = ctx.snapshot_of(project).await?;
            let view = ProjectContext::new(&ctx.summary, &snapshot).task_view(&task);
            out.show(&view, || format!("Created {}", task_line(&view)));
        }
        TaskCommand::Edit(args) => {
            let (_, detail) = ctx.detail(&args.reference).await?;
            let mut fields = detail.task.fields.clone();
            let mut changed = false;
            if let Some(title) = args.title {
                fields.title = title;
                changed = true;
            }
            if let Some(description) = args.description {
                fields.description = description;
                changed = true;
            }
            if let Some(name) = &args.assignee {
                fields.assignee = Some(ctx.member(name).await?);
                changed = true;
            }
            if args.unassign {
                fields.assignee = None;
                changed = true;
            }
            if !args.labels.is_empty() {
                fields.labels = args.labels;
                changed = true;
            }
            if args.clear_labels {
                fields.labels.clear();
                changed = true;
            }
            if let Some(priority) = &args.priority {
                fields.priority = Some(parse_priority(priority)?);
                changed = true;
            }
            if args.clear_priority {
                fields.priority = None;
                changed = true;
            }
            if let Some(due) = &args.due {
                fields.due_date = Some(parse_date(due)?);
                changed = true;
            }
            if args.clear_due {
                fields.due_date = None;
                changed = true;
            }
            if !changed {
                return Err(ClientError::Invalid(
                    "nothing to change; pass at least one field flag".into(),
                ));
            }
            let version = args.expected_version.unwrap_or(detail.task.version);
            let view = ctx.save(detail.task.id, version, fields).await?;
            out.show(&view, || {
                format!(
                    "Saved {} (version {})",
                    view.task.reference, view.task.version
                )
            });
        }
        TaskCommand::Move {
            reference,
            stage: stage_name,
            expected_version,
        } => {
            let (_, detail) = ctx.detail(&reference).await?;
            let project = ctx
                .summary
                .projects
                .iter()
                .find(|p| p.id == detail.task.project_id)
                .ok_or_else(|| ClientError::NotFound("project".into()))?;
            let snapshot = ctx.snapshot_of(project).await?;
            let mut fields = detail.task.fields.clone();
            fields.stage_id = stage(&snapshot.stages, &stage_name)?.id;
            let version = expected_version.unwrap_or(detail.task.version);
            let view = ctx.save(detail.task.id, version, fields).await?;
            out.show(&view, || format!("Moved {}", task_line(&view)));
        }
        TaskCommand::Delete { reference, yes } => {
            let (_, detail) = ctx.detail(&reference).await?;
            confirm(
                &format!(
                    "Delete {} \"{}\" with its comments and relations?",
                    detail.task.reference, detail.task.fields.title
                ),
                yes,
            )?;
            ctx.ws.delete_task(detail.task.id).await?;
            out.show(&json!({ "deleted": detail.task.reference }), || {
                format!("Deleted {}", detail.task.reference)
            });
        }
        TaskCommand::Comment { reference, text } => {
            let (_, detail) = ctx.detail(&reference).await?;
            let comment = ctx.ws.add_comment(detail.task.id, &text).await?;
            out.show(&comment, || {
                format!("Commented on {}", detail.task.reference)
            });
        }
    }
    Ok(())
}

async fn relation(ctx: &Ctx, command: RelationCommand) -> Result<(), ClientError> {
    let out = &ctx.out;
    match command {
        RelationCommand::List { reference } => {
            let (_, detail) = ctx.detail(&reference).await?;
            let project = ctx
                .summary
                .projects
                .iter()
                .find(|p| p.id == detail.task.project_id)
                .ok_or_else(|| ClientError::NotFound("project".into()))?;
            let snapshot = ctx.snapshot_of(project).await?;
            let views = ProjectContext::new(&ctx.summary, &snapshot).relations_of(detail.task.id);
            out.show(&views, || {
                views
                    .iter()
                    .map(|r| {
                        format!(
                            "{} {} {}  {}\n",
                            detail.task.reference, r.relation, r.task, r.title
                        )
                    })
                    .collect()
            });
        }
        RelationCommand::Create {
            from,
            relation_type,
            to,
        } => {
            let (kind, reversed) = ctx.relation_type(&relation_type)?;
            let (_, a) = ctx.detail(&from).await?;
            let (_, b) = ctx.detail(&to).await?;
            let (from_task, to_task) = if reversed {
                (&b.task, &a.task)
            } else {
                (&a.task, &b.task)
            };
            let relation = ctx
                .ws
                .create_relation(CreateRelation {
                    type_id: kind.id,
                    from: from_task.id,
                    to: to_task.id,
                })
                .await?;
            out.show(&relation, || {
                format!(
                    "Linked {} {} {}",
                    a.task.reference, relation_type, b.task.reference
                )
            });
        }
        RelationCommand::Delete {
            from,
            relation_type,
            to,
            yes,
        } => {
            let (kind, reversed) = ctx.relation_type(&relation_type)?;
            let (_, a) = ctx.detail(&from).await?;
            let (_, b) = ctx.detail(&to).await?;
            let (x, y) = if reversed {
                (b.task.id, a.task.id)
            } else {
                (a.task.id, b.task.id)
            };
            let (x, y) = relation_endpoints(kind.directed, x, y);
            let project = ctx
                .summary
                .projects
                .iter()
                .find(|p| p.id == a.task.project_id)
                .ok_or_else(|| ClientError::NotFound("project".into()))?;
            let snapshot = ctx.snapshot_of(project).await?;
            let existing = snapshot
                .relations
                .iter()
                .find(|r| r.type_id == kind.id && r.from == x && r.to == y)
                .ok_or_else(|| {
                    ClientError::NotFound(format!("relation {from} {relation_type} {to}"))
                })?;
            confirm(
                &format!("Remove the link {from} {relation_type} {to}?"),
                yes,
            )?;
            ctx.ws.delete_relation(existing.id).await?;
            out.show(&json!({ "deleted": existing.id }), || {
                format!("Removed {from} {relation_type} {to}")
            });
        }
    }
    Ok(())
}
