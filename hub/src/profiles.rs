//! Personal role routing. It cannot grant tools, permissions, or credentials.
use super::*;
use std::collections::HashSet;

pub(super) const STALE: &str = "selected agent profile changed; refresh and select it again";
pub(super) const UNAVAILABLE: &str = "unknown or disabled personal agent";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersonalAgent {
    pub id: String,
    pub name: String,
    pub role: String,
    pub model_ref: String,
    pub enabled: bool,
}
#[derive(Serialize, Deserialize)]
pub struct Profile {
    pub revision: u64,
    pub agents: Vec<PersonalAgent>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Update {
    pub expected_revision: u64,
    pub agents: Vec<PersonalAgent>,
}
fn defaults() -> Vec<PersonalAgent> {
    vec![PersonalAgent {
        id: "butler".into(),
        name: "个人管家".into(),
        role: "coordinator".into(),
        model_ref: "server-default".into(),
        enabled: true,
    }]
}
pub(super) fn initialize(db: &Connection) -> rusqlite::Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS personal_profiles (owner TEXT PRIMARY KEY, revision INTEGER NOT NULL, agents_json TEXT NOT NULL);")
}
fn read(db: &Connection, owner: &str) -> Result<Profile> {
    let record: Option<(u64, String)> = db
        .query_row(
            "SELECT revision, agents_json FROM personal_profiles WHERE owner=?1",
            [owner],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match record {
        Some((revision, agents)) => Ok(Profile {
            revision,
            agents: serde_json::from_str(&agents)
                .map_err(|_| Error(StatusCode::INTERNAL_SERVER_ERROR, "invalid stored profile"))?,
        }),
        None => Ok(Profile {
            revision: 0,
            agents: defaults(),
        }),
    }
}
pub(super) async fn catalog(
    State(h): State<Hub>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>> {
    h.identity(&headers)?;
    Ok(Json(serde_json::json!({
        "models": [{"id":"server-default", "runtime": h.runtime.label(), "description":"Operator-configured server model; no personal credentials"}],
        "roles": ["coordinator", "researcher", "writer", "reviewer"],
        "max_agents": 8,
        "execution": "single_role_turn",
        "selection": "Default enabled coordinator; optional chat agent_id selects an enabled personal role",
        "permissions": "Profiles do not grant tools, delegation, or data access"
    })))
}
pub(super) async fn get_profile(State(h): State<Hub>, headers: HeaderMap) -> Result<Json<Profile>> {
    let owner = h.identity(&headers)?;
    let db = h.db()?;
    Ok(Json(read(&db, &owner)?))
}
pub(super) async fn update_profile(
    State(h): State<Hub>,
    headers: HeaderMap,
    Json(input): Json<Update>,
) -> Result<Json<Profile>> {
    let owner = h.identity(&headers)?;
    validate(&input.agents)?;
    let db = h.db()?;
    let current = read(&db, &owner)?;
    if input.expected_revision != current.revision {
        return Err(Error(StatusCode::CONFLICT, "stale profile revision"));
    }
    let revision = current
        .revision
        .checked_add(1)
        .filter(|v| *v <= i64::MAX as u64)
        .ok_or(Error(StatusCode::CONFLICT, "profile revision exhausted"))?;
    let encoded = serde_json::to_string(&input.agents)
        .map_err(|_| Error(StatusCode::INTERNAL_SERVER_ERROR, "profile encoding failed"))?;
    // The shared connection mutex covers both the revision check and write.
    db.execute("INSERT INTO personal_profiles(owner, revision, agents_json) VALUES (?1,?2,?3) ON CONFLICT(owner) DO UPDATE SET revision=excluded.revision, agents_json=excluded.agents_json", params![owner, revision, encoded])?;
    Ok(Json(Profile {
        revision,
        agents: input.agents,
    }))
}

fn validate(agents: &[PersonalAgent]) -> Result<()> {
    if agents.is_empty() || agents.len() > 8 {
        return Err(invalid("profile must contain 1..8 agents"));
    }
    let mut ids = HashSet::new();
    let mut coordinators = 0;
    for agent in agents {
        if agent.id.is_empty()
            || agent.id.len() > 32
            || !agent
                .id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            || !ids.insert(&agent.id)
        {
            return Err(invalid(
                "agent IDs must be unique, 1..32 ASCII letters, digits, hyphens or underscores",
            ));
        }
        if agent.name.trim().is_empty()
            || agent.name.len() > 96
            || agent.name.chars().any(char::is_control)
        {
            return Err(invalid(
                "agent names must contain 1..96 bytes without control characters",
            ));
        }
        if !["coordinator", "researcher", "writer", "reviewer"].contains(&agent.role.as_str()) {
            return Err(invalid("unknown agent role"));
        }
        if agent.model_ref != "server-default" {
            return Err(invalid("unknown server model reference"));
        }
        if agent.role == "coordinator" && agent.enabled {
            coordinators += 1;
        }
    }
    if coordinators != 1 {
        return Err(invalid("exactly one enabled coordinator is required"));
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct RuntimeProfile {
    pub revision: u64,
    pub agent_id: String,
    pub role: String,
    pub model_ref: String,
}

pub(super) fn route(
    db: &Connection,
    owner: &str,
    selected: Option<&str>,
    expected_revision: Option<i64>,
) -> Result<RuntimeProfile> {
    let profile = read(db, owner)?;
    if expected_revision.is_some_and(|revision| revision < 0 || revision as u64 != profile.revision) {
        return Err(Error(StatusCode::CONFLICT, STALE));
    }
    // Validate stored state as well as writes: never fall back on corruption.
    validate(&profile.agents)?;
    let agent = profile
        .agents
        .iter()
        .find(|a| {
            a.enabled
                && match selected {
                    Some(id) => a.id == id,
                    None => a.role == "coordinator",
                }
        })
        .ok_or(invalid(UNAVAILABLE))?;
    Ok(RuntimeProfile {
        revision: profile.revision,
        agent_id: agent.id.clone(),
        role: agent.role.clone(),
        model_ref: agent.model_ref.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn runtime_revalidates_corrupt_stored_profiles() {
        let db = Connection::open_in_memory().unwrap();
        initialize(&db).unwrap();
        let mut agents = defaults();
        agents[0].model_ref = "http://untrusted".into();
        db.execute(
            "INSERT INTO personal_profiles VALUES ('alice',1,?1)",
            [serde_json::to_string(&agents).unwrap()],
        )
        .unwrap();
        assert!(route(&db, "alice", None, None).is_err());
        assert_eq!(
            route(&db, "bob", None, None).unwrap().model_ref,
            "server-default"
        );
    }
}
