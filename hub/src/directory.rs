//! Configured membership and delegation policy, independent of runtime locks.
//!
//! Private staging membership can be refreshed from an owner-only configuration. OIDC/tenant policy belongs here rather
//! than in the presence, absence or current state of a per-user execution gate.
use std::{
    collections::{HashMap, HashSet},
    sync::RwLock,
};

pub(crate) struct Directory {
    members: RwLock<HashSet<String>>,
    tenants: Option<HashMap<String, String>>,
}

impl Directory {
    pub(crate) fn new(ids: impl IntoIterator<Item = String>) -> Result<Self, &'static str> {
        let mut members = HashSet::new();
        for id in ids {
            if id.is_empty()
                || id.len() > 64
                || !id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                || !members.insert(id)
            {
                return Err("invalid or duplicated identity configuration");
            }
        }
        if members.is_empty() {
            return Err("at least one configured user is required");
        }
        Ok(Self {
            members: RwLock::new(members),
            tenants: None,
        })
    }

    pub(crate) fn gitlab_workspace(ids: Vec<String>) -> Self {
        Self {
            members: RwLock::new(ids.into_iter().collect()),
            tenants: None,
        }
    }
    pub(crate) fn add_gitlab_user(&self, id: String) {
        self.members.write().expect("directory lock").insert(id);
    }
    pub(crate) fn contains(&self, id: &str) -> bool {
        self.members.read().expect("directory lock").contains(id)
    }

    pub(crate) fn contacts(&self, requester: &str) -> Vec<String> {
        let mut contacts: Vec<_> = self
            .ids()
            .filter(|id| self.can_delegate(requester, id))
            .collect();
        contacts.sort();
        contacts
    }

    pub(crate) fn can_delegate(&self, requester: &str, owner: &str) -> bool {
        requester != owner
            && self.contains(requester)
            && self.contains(owner)
            && self
                .tenants
                .as_ref()
                .is_none_or(|mapping| mapping.get(requester) == mapping.get(owner))
    }

    pub(crate) fn with_tenants(
        mut self,
        mapping: HashMap<String, String>,
    ) -> Result<Self, &'static str> {
        if mapping.len() != self.members.read().expect("directory lock").len()
            || mapping.iter().any(|(user, tenant)| {
                !self.contains(user)
                    || tenant.is_empty()
                    || tenant.len() > 64
                    || !tenant
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
            })
        {
            return Err(
                "tenant mapping must assign every configured user exactly once to a valid tenant",
            );
        }
        self.tenants = Some(mapping);
        Ok(self)
    }

    pub(crate) fn tenant(&self, user: &str) -> Option<&str> {
        self.tenants.as_ref()?.get(user).map(String::as_str)
    }

    pub(crate) fn tenants_enabled(&self) -> bool {
        self.tenants.is_some()
    }

    pub(crate) fn ids(&self) -> impl Iterator<Item = String> {
        self.members
            .read()
            .expect("directory lock")
            .clone()
            .into_iter()
    }

    pub(crate) fn replace(&self, replacement: Self) {
        *self.members.write().expect("directory lock") =
            replacement.members.into_inner().expect("directory lock");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contacts_exclude_self_unknown_users_and_other_tenants() {
        let directory = Directory::new(["alice", "bob", "carol"].map(String::from))
            .unwrap()
            .with_tenants(
                [("alice", "one"), ("bob", "one"), ("carol", "two")]
                    .into_iter()
                    .map(|(id, tenant)| (id.into(), tenant.into()))
                    .collect(),
            )
            .unwrap();
        assert_eq!(directory.contacts("alice"), vec!["bob"]);
        assert!(directory.contacts("carol").is_empty());
        assert!(directory.contacts("unknown").is_empty());
    }

    #[test]
    fn delegation_requires_two_distinct_configured_members() {
        let directory = Directory::new(["alice".into(), "bob".into()]).unwrap();
        for (requester, owner, allowed) in [
            ("alice", "bob", true),
            ("bob", "alice", true),
            ("alice", "alice", false),
            ("alice", "unknown", false),
            ("unknown", "alice", false),
            ("unknown", "other", false),
        ] {
            assert_eq!(directory.can_delegate(requester, owner), allowed);
        }
        assert_eq!(directory.ids().count(), 2);
    }

    #[test]
    fn configuration_retains_identifier_and_duplicate_constraints() {
        for ids in [
            vec![],
            vec![""],
            vec!["alice", "alice"],
            vec!["white space"],
            vec!["名字"],
        ] {
            assert!(Directory::new(ids.into_iter().map(String::from)).is_err());
        }
        assert!(Directory::new(["x".repeat(65)]).is_err());
        assert!(Directory::new(["x".repeat(64), "Bob_2-a".into()]).is_ok());
    }
    #[test]
    fn gates_serialize_per_user_without_becoming_authorization_state() {
        use crate::{Config, Hub, Runtime, UserConfig};
        let hub = Hub::open(Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            database: ":memory:".into(),
            users: vec![
                UserConfig {
                    id: "alice".into(),
                    token_sha256: "a".repeat(64),
                },
                UserConfig {
                    id: "bob".into(),
                    token_sha256: "b".repeat(64),
                },
            ],
            runtime: Runtime::Mock,
        })
        .unwrap();
        let clone = hub.clone();
        let _alice = hub.gates.get("alice").unwrap().try_lock().unwrap();
        assert!(clone.gates.get("alice").unwrap().try_lock().is_err());
        assert!(clone.gates.get("bob").unwrap().try_lock().is_ok());
        assert!(hub.directory.can_delegate("alice", "bob"));
        // Even a diverging gate map must not change membership/policy decisions.
        let mut changed = hub.clone();
        changed.gates = std::sync::Arc::new(std::collections::HashMap::new());
        assert!(changed.directory.contains("bob"));
        assert!(changed.directory.can_delegate("alice", "bob"));
        assert!(!changed.directory.can_delegate("alice", "unknown"));
    }
}
