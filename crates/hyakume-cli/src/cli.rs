use clap::{Args, Parser, Subcommand};

const EXIT_CODES: &str = "\
Exit codes:
  0  success                 5  task changed since you read it (version conflict)
  1  internal or config      6  already exists
  2  invalid input/usage     7  server unreachable
  3  not found or hidden     8  not signed in or session expired
  4  not permitted         127  desktop app not installed (no arguments)

With --json, results go to stdout and errors to stderr as
{\"error\": {\"code\", \"message\", \"latest\"?}}.";

/// Hyakume work tracker. Run without arguments to open the desktop app.
#[derive(Parser)]
#[command(name = "hyakume", version, after_help = EXIT_CODES)]
pub struct Cli {
    /// Workspace to act on: `local` or SERVER/WORKSPACE. Defaults to the
    /// saved default, then `local`.
    #[arg(short, long, global = true)]
    pub workspace: Option<String>,
    /// Print stable JSON instead of text. Errors go to stderr as JSON too.
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Manage the servers this profile knows about.
    #[command(subcommand)]
    Server(ServerCommand),
    /// Sign in to servers.
    #[command(subcommand)]
    Auth(AuthCommand),
    /// List workspaces or choose the default one.
    #[command(subcommand)]
    Workspace(WorkspaceCommand),
    #[command(subcommand)]
    Project(ProjectCommand),
    #[command(subcommand)]
    Stage(StageCommand),
    #[command(subcommand)]
    Task(TaskCommand),
    /// Links between tasks, across projects of one workspace.
    #[command(subcommand)]
    Relation(RelationCommand),
    #[command(subcommand, name = "relation-type")]
    RelationType(RelationTypeCommand),
    #[command(subcommand)]
    Role(RoleCommand),
    #[command(subcommand)]
    Member(MemberCommand),
    /// Carry local work to another computer as one file.
    #[command(subcommand)]
    Local(LocalCommand),
}

#[derive(Subcommand)]
pub enum ServerCommand {
    /// Remember a server under a short name.
    Add {
        name: String,
        url: String,
    },
    List,
    /// Forget a server and its saved session.
    Remove {
        name: String,
    },
}

#[derive(Subcommand)]
pub enum AuthCommand {
    /// Development only: sign in as a test identity listed in the server config.
    DevLogin {
        identity: String,
        #[arg(long)]
        server: String,
    },
    /// Delete the saved session for a server.
    Logout {
        #[arg(long)]
        server: String,
    },
    /// Show who you are signed in as on each server.
    Status,
}

#[derive(Subcommand)]
pub enum WorkspaceCommand {
    List,
    /// Save the workspace used when --workspace is not given.
    Use {
        selector: String,
    },
}

#[derive(Subcommand)]
pub enum ProjectCommand {
    List,
    /// Create a project with the default workflow stages.
    Create {
        key: String,
        name: String,
    },
}

#[derive(Subcommand)]
pub enum StageCommand {
    List {
        #[arg(long)]
        project: String,
    },
}

#[derive(Subcommand)]
pub enum TaskCommand {
    List(TaskFilter),
    Show {
        reference: String,
    },
    Create(TaskCreate),
    /// Change fields. Saves the whole task and fails with exit code 5 if
    /// someone else changed it first.
    Edit(TaskEdit),
    /// Move a task to another workflow stage.
    Move {
        reference: String,
        stage: String,
        #[arg(long)]
        expected_version: Option<i64>,
    },
    Delete {
        reference: String,
        /// Skip the confirmation prompt. Required when not run from a terminal.
        #[arg(long)]
        yes: bool,
    },
    Comment {
        reference: String,
        text: String,
    },
}

#[derive(Args)]
pub struct TaskFilter {
    #[arg(long)]
    pub project: Option<String>,
    #[arg(long)]
    pub stage: Option<String>,
    /// Only tasks assigned to you.
    #[arg(long, conflicts_with = "assignee")]
    pub mine: bool,
    #[arg(long)]
    pub assignee: Option<String>,
}

#[derive(Args)]
pub struct TaskCreate {
    #[arg(long)]
    pub project: String,
    pub title: String,
    #[arg(long, default_value = "")]
    pub description: String,
    #[arg(long)]
    pub stage: Option<String>,
    /// Identity name, or `me`.
    #[arg(long)]
    pub assignee: Option<String>,
    #[arg(long = "label")]
    pub labels: Vec<String>,
    #[arg(long)]
    pub priority: Option<String>,
    /// YYYY-MM-DD
    #[arg(long)]
    pub due: Option<String>,
}

#[derive(Args)]
pub struct TaskEdit {
    pub reference: String,
    #[arg(long)]
    pub title: Option<String>,
    #[arg(long)]
    pub description: Option<String>,
    #[arg(long, conflicts_with = "unassign")]
    pub assignee: Option<String>,
    #[arg(long)]
    pub unassign: bool,
    /// Replaces all labels. Repeat for several.
    #[arg(long = "label", conflicts_with = "clear_labels")]
    pub labels: Vec<String>,
    #[arg(long)]
    pub clear_labels: bool,
    #[arg(long, conflicts_with = "clear_priority")]
    pub priority: Option<String>,
    #[arg(long)]
    pub clear_priority: bool,
    #[arg(long, conflicts_with = "clear_due")]
    pub due: Option<String>,
    #[arg(long)]
    pub clear_due: bool,
    /// Fail unless the task is still at this version. Without it, the version
    /// read just before saving is used.
    #[arg(long)]
    pub expected_version: Option<i64>,
}

#[derive(Subcommand)]
pub enum RelationCommand {
    /// Relations of one task.
    List { reference: String },
    /// For example: `relation create API-1 blocks WEB-2`.
    Create {
        from: String,
        relation_type: String,
        to: String,
    },
    Delete {
        from: String,
        relation_type: String,
        to: String,
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
pub enum RelationTypeCommand {
    List,
    Create {
        name: String,
        /// Directional types read differently from each end.
        #[arg(long)]
        directed: bool,
        /// Label seen from the target, such as "needed by". Required with --directed.
        #[arg(long, requires = "directed", required_if_eq("directed", "true"))]
        reverse: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum RoleCommand {
    List,
    /// For example: `role create Linker --permission manage_relations`.
    Create {
        name: String,
        #[arg(long = "permission", required = true)]
        permissions: Vec<String>,
    },
}

#[derive(Subcommand)]
pub enum MemberCommand {
    List,
    /// Replace a member's roles.
    Roles {
        identity: String,
        #[arg(required = true)]
        roles: Vec<String>,
    },
}

#[derive(Subcommand)]
pub enum LocalCommand {
    /// Write all local work, with its comments, links, and your canvas
    /// positions, to FILE. It holds your own work: keep it private.
    Export { file: std::path::PathBuf },
    /// Put FILE's work in place of local work. Nothing is merged: with
    /// --replace, local projects that aren't in FILE are deleted.
    Import {
        file: std::path::PathBuf,
        /// Allowed even when local work already has projects.
        #[arg(long)]
        replace: bool,
        /// Skip the confirmation for --replace. Required when not run from a terminal.
        #[arg(long, requires = "replace")]
        yes: bool,
    },
}
