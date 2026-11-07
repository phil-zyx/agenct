//! Explicit organization partition for globally unique identities. This is not
//! auto-provisioning, a production deployment, or isolation from the DB operator.
use crate::{directory::Directory, Hub};
use rusqlite::{params, OptionalExtension};
use serde::{
    de::{MapAccess, Visitor},
    Deserialize, Deserializer,
};
use std::{collections::HashMap, fmt, sync::Arc};

pub(super) const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS tenant_memberships (
    owner TEXT PRIMARY KEY REFERENCES sessions(owner), tenant TEXT NOT NULL);
CREATE TRIGGER IF NOT EXISTS tenant_memberships_no_replace BEFORE INSERT ON tenant_memberships
WHEN EXISTS (SELECT 1 FROM tenant_memberships WHERE owner=NEW.owner)
BEGIN SELECT RAISE(ABORT,'tenant assignments are immutable'); END;
CREATE TRIGGER IF NOT EXISTS tenant_memberships_no_update BEFORE UPDATE ON tenant_memberships
BEGIN SELECT RAISE(ABORT,'tenant assignments are immutable'); END;
CREATE TRIGGER IF NOT EXISTS tenant_memberships_no_delete BEFORE DELETE ON tenant_memberships
BEGIN SELECT RAISE(ABORT,'tenant assignments are immutable'); END;";

/// JSON object mapping every configured user ID to one organization ID.
/// Duplicate keys are rejected instead of silently choosing one organization.
pub struct Config(pub HashMap<String, String>);
impl<'de> Deserialize<'de> for Config {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Mapping;
        impl<'de> Visitor<'de> for Mapping {
            type Value = Config;
            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("an object of unique user IDs mapped to organization IDs")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut entries = HashMap::new();
                while let Some((user, tenant)) = map.next_entry::<String, String>()? {
                    if entries.insert(user, tenant).is_some() {
                        return Err(serde::de::Error::custom("duplicate user in tenant mapping"));
                    }
                }
                Ok(Config(entries))
            }
        }
        deserializer.deserialize_map(Mapping)
    }
}

impl Hub {
    /// A tenantized database cannot silently fall back to unpartitioned access.
    /// Call after all builders; identity checks also enforce this for library users.
    pub fn validate_startup(&self) -> Result<(), &'static str> {
        let persisted: bool = self
            .db()
            .map_err(|_| "tenant storage unavailable")?
            .query_row("SELECT EXISTS(SELECT 1 FROM tenant_memberships)", [], |r| {
                r.get(0)
            })
            .map_err(|_| "tenant startup audit failed")?;
        if persisted && !self.directory.tenants_enabled() {
            return Err("this database requires its explicit OIDC tenant configuration");
        }
        Ok(())
    }

    /// Enable only with an already-configured OIDC identity mode and a fixed
    /// complete roster. Old cross-organization work fails startup, never leaks.
    pub fn with_tenants(mut self, config: Config) -> Result<Self, &'static str> {
        if self.private_config.is_some() {
            return Err("tenant isolation cannot use live private membership");
        }
        if self.oidc.is_none() && self.browser_auth.is_none() {
            return Err("tenant isolation requires explicit OIDC authentication");
        }
        let directory = Directory::new(self.directory.ids())?.with_tenants(config.0)?;
        if let Some(repositories) = &self.repositories {
            repositories.validate_tenants(&directory)?;
        }
        {
            let mut db = self.db().map_err(|_| "tenant storage unavailable")?;
            let tx = db
                .transaction()
                .map_err(|_| "tenant startup audit failed")?;
            // Bind all current identities, including those with only private chat.
            // Historical assignments remain after a user leaves the active roster.
            {
                let mut query = tx
                    .prepare("SELECT owner FROM sessions")
                    .map_err(|_| "tenant startup audit failed")?;
                let owners = query
                    .query_map([], |r| r.get::<_, String>(0))
                    .map_err(|_| "tenant startup audit failed")?;
                for owner in owners {
                    let owner = owner.map_err(|_| "tenant startup audit failed")?;
                    let persisted: Option<String> = tx
                        .query_row(
                            "SELECT tenant FROM tenant_memberships WHERE owner=?1",
                            [&owner],
                            |r| r.get(0),
                        )
                        .optional()
                        .map_err(|_| "tenant startup audit failed")?;
                    match (persisted, directory.tenant(&owner)) {
                        (Some(old), Some(new)) if old != new => return Err("existing user tenant cannot be reassigned without an explicit data migration"),
                        (None, Some(tenant)) => { tx.execute("INSERT INTO tenant_memberships(owner,tenant) VALUES (?1,?2)",params![owner,tenant]).map_err(|_| "tenant startup audit failed")?; },
                        (None, None) => return Err("historical user data requires an explicit tenant assignment"),
                        _ => {},
                    }
                }
            }
            // Historical work belongs to immutable persisted memberships, even
            // after departure. New actions still require the active directory.
            for table in ["tasks", "proposals"] {
                let sql = format!("SELECT r.requester,r.owner,a.tenant,b.tenant FROM {table} r LEFT JOIN tenant_memberships a ON a.owner=r.requester LEFT JOIN tenant_memberships b ON b.owner=r.owner");
                let mut query = tx
                    .prepare(&sql)
                    .map_err(|_| "tenant startup audit failed")?;
                let rows = query
                    .query_map([], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, Option<String>>(2)?,
                            row.get::<_, Option<String>>(3)?,
                        ))
                    })
                    .map_err(|_| "tenant startup audit failed")?;
                for row in rows {
                    let (requester, owner, requester_tenant, owner_tenant) =
                        row.map_err(|_| "tenant startup audit failed")?;
                    if requester == owner
                        || requester_tenant.is_none()
                        || requester_tenant != owner_tenant
                    {
                        return Err("existing tasks or proposals cross tenant boundaries or reference unmapped users");
                    }
                }
            }
            tx.commit().map_err(|_| "tenant startup audit failed")?;
        }
        self.directory = Arc::new(directory);
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn duplicate_unknown_missing_and_invalid_memberships_are_rejected() {
        assert!(serde_json::from_str::<Config>(r#"{"alice":"one","alice":"two"}"#).is_err());
        for mapping in [
            r#"{}"#,
            r#"{"alice":"one"}"#,
            r#"{"alice":"one","bob":"two","eve":"one"}"#,
            r#"{"alice":"","bob":"two"}"#,
            r#"{"alice":"../other","bob":"two"}"#,
        ] {
            let config: Config = serde_json::from_str(mapping).unwrap();
            assert!(Directory::new(["alice".into(), "bob".into()])
                .unwrap()
                .with_tenants(config.0)
                .is_err());
        }
        let config: Config = serde_json::from_str(r#"{"alice":"one","bob":"two"}"#).unwrap();
        let directory = Directory::new(["alice".into(), "bob".into()])
            .unwrap()
            .with_tenants(config.0)
            .unwrap();
        assert!(!directory.can_delegate("alice", "bob"));
        assert_eq!(directory.tenant("alice"), Some("one"));
    }
}
