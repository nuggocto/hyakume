use std::collections::BTreeSet;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::ids::ProjectId;
use crate::model::ValidationError;

/// Permissions a role can grant. Ownership transfer and workspace deletion
/// aren't here on purpose: only the owner can do those, so no role can
/// hand them out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    View,
    EditTasks,
    DeleteTasks,
    Comment,
    ManageRelations,
    ConfigureProjects,
    ManageInvitations,
    ManageRoles,
}

impl Permission {
    pub const ALL: [Permission; 8] = [
        Self::View,
        Self::EditTasks,
        Self::DeleteTasks,
        Self::Comment,
        Self::ManageRelations,
        Self::ConfigureProjects,
        Self::ManageInvitations,
        Self::ManageRoles,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::View => "view",
            Self::EditTasks => "edit_tasks",
            Self::DeleteTasks => "delete_tasks",
            Self::Comment => "comment",
            Self::ManageRelations => "manage_relations",
            Self::ConfigureProjects => "configure_projects",
            Self::ManageInvitations => "manage_invitations",
            Self::ManageRoles => "manage_roles",
        }
    }

    /// Project-scoped permissions are the only ones a guest's roles can use,
    /// and only inside the projects the guest was invited to.
    pub fn is_project_scoped(self) -> bool {
        matches!(
            self,
            Self::View
                | Self::EditTasks
                | Self::DeleteTasks
                | Self::Comment
                | Self::ManageRelations
        )
    }
}

impl FromStr for Permission {
    type Err = ValidationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|p| p.as_str() == s.trim())
            .ok_or_else(|| ValidationError::Other(format!("unknown permission {s:?}")))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Preset {
    Owner,
    Admin,
    Member,
    Guest,
}

impl Preset {
    pub const ALL: [Preset; 4] = [Self::Owner, Self::Admin, Self::Member, Self::Guest];

    pub fn name(self) -> &'static str {
        match self {
            Self::Owner => "Owner",
            Self::Admin => "Admin",
            Self::Member => "Member",
            Self::Guest => "Guest",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Admin => "admin",
            Self::Member => "member",
            Self::Guest => "guest",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str() == s)
    }

    /// Starting permissions; workspace admins can edit them afterwards.
    pub fn default_permissions(self) -> BTreeSet<Permission> {
        use Permission::*;
        match self {
            Self::Owner | Self::Admin => Permission::ALL.into_iter().collect(),
            Self::Member => [View, EditTasks, Comment].into_iter().collect(),
            Self::Guest => [View].into_iter().collect(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MembershipKind {
    /// Roles apply across every project in the workspace.
    Member,
    /// Roles apply only inside explicitly invited projects.
    Guest,
}

impl MembershipKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Member => "member",
            Self::Guest => "guest",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "member" => Some(Self::Member),
            "guest" => Some(Self::Guest),
            _ => None,
        }
    }
}

/// What one identity may do in one workspace: the union of its roles,
/// bounded by its membership kind.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Access {
    pub kind: MembershipKind,
    pub is_owner: bool,
    pub permissions: BTreeSet<Permission>,
    pub guest_projects: BTreeSet<ProjectId>,
}

impl Access {
    /// Full access, used for local personal work where there is only one person.
    pub fn local_owner() -> Self {
        Self {
            kind: MembershipKind::Member,
            is_owner: true,
            permissions: Permission::ALL.into_iter().collect(),
            guest_projects: BTreeSet::new(),
        }
    }

    /// `project` is the project the action touches, or `None` for
    /// workspace-wide actions such as managing roles.
    pub fn allows(&self, permission: Permission, project: Option<ProjectId>) -> bool {
        if self.is_owner {
            return true;
        }
        if !self.permissions.contains(&permission) {
            return false;
        }
        match self.kind {
            MembershipKind::Member => true,
            MembershipKind::Guest => {
                permission.is_project_scoped()
                    && project.is_some_and(|p| self.guest_projects.contains(&p))
            }
        }
    }

    pub fn can_view_project(&self, project: ProjectId) -> bool {
        self.allows(Permission::View, Some(project))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn access(kind: MembershipKind, permissions: &[Permission], guest_of: &[ProjectId]) -> Access {
        Access {
            kind,
            is_owner: false,
            permissions: permissions.iter().copied().collect(),
            guest_projects: guest_of.iter().copied().collect(),
        }
    }

    #[test]
    fn member_roles_apply_to_every_project_but_need_the_permission() {
        let member = access(
            MembershipKind::Member,
            &Preset::Member
                .default_permissions()
                .into_iter()
                .collect::<Vec<_>>(),
            &[],
        );
        let any_project = ProjectId::new();
        assert!(member.allows(Permission::EditTasks, Some(any_project)));
        assert!(!member.allows(Permission::ManageRelations, Some(any_project)));
        assert!(!member.allows(Permission::DeleteTasks, Some(any_project)));
    }

    #[test]
    fn guest_permissions_stop_at_invited_projects_and_workspace_actions() {
        let invited = ProjectId::new();
        let other = ProjectId::new();
        let guest = access(
            MembershipKind::Guest,
            &[
                Permission::View,
                Permission::EditTasks,
                Permission::ManageRoles,
            ],
            &[invited],
        );
        assert!(guest.allows(Permission::EditTasks, Some(invited)));
        assert!(!guest.allows(Permission::View, Some(other)));
        // A role may list ManageRoles, but a guest never gets workspace-wide powers.
        assert!(!guest.allows(Permission::ManageRoles, None));
    }
}
