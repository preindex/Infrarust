use crate::permissions::Capability;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ProviderKind {
    Ban,
    Permission,
    AdmissionGuard,
}

impl ProviderKind {
    pub const fn capability(self) -> Capability {
        match self {
            Self::Ban => Capability::BanProvider,
            Self::Permission => Capability::PermissionProvider,
            Self::AdmissionGuard => Capability::AdmissionGuard,
        }
    }

    pub const fn section(self) -> &'static str {
        match self {
            Self::Ban => "ban",
            Self::Permission => "permissions",
            Self::AdmissionGuard => "admission-guards",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ProviderRejected {
    #[error("the plugin lacks the {} capability", .kind.capability())]
    MissingCapability { kind: ProviderKind },
    #[error("[{}] provider selects `{selected}`, not this plugin", .kind.section())]
    NotSelected {
        kind: ProviderKind,
        selected: String,
    },
    /// The slot is fixed to one provider, and another one is already registered.
    #[error("[{}] a provider is already registered", .kind.section())]
    AlreadyRegistered { kind: ProviderKind },
}

impl ProviderRejected {
    pub const fn kind(&self) -> ProviderKind {
        match self {
            Self::MissingCapability { kind }
            | Self::NotSelected { kind, .. }
            | Self::AlreadyRegistered { kind } => *kind,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejections_name_the_capability_and_the_config_section() {
        let missing = ProviderRejected::MissingCapability {
            kind: ProviderKind::Ban,
        };
        assert_eq!(
            missing.to_string(),
            "the plugin lacks the ban-provider capability"
        );
        let other = ProviderRejected::NotSelected {
            kind: ProviderKind::Permission,
            selected: "builtin".into(),
        };
        assert_eq!(
            other.to_string(),
            "[permissions] provider selects `builtin`, not this plugin"
        );
        assert_eq!(other.kind(), ProviderKind::Permission);
    }
}
