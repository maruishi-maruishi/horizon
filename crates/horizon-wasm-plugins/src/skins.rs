//! Skins and capes on an offline-mode server.
//!
//! An online-mode server gets every player's `textures` (skin, model and equipped cape, signed by
//! Mojang) from the session service when they log in. An offline-mode server gets none, so every
//! player looks like a default skin without a cape. When a player arrives without `textures`, this
//! looks their name up at Mojang (`api.mojang.com` for the account id, then the session server's
//! profile with `unsigned=false`, the same signed property online mode receives) and adds it, so
//! every client draws their real skin, outer layers and cape. Names that have no Minecraft account
//! keep the default skin. Turned off with `enabled = false` in `plugins/skins.toml`.
use horizon_protocol::packets::login::Property;
use serde_json::Value;
use std::{
    collections::HashMap,
    path::Path,
    sync::Mutex,
    time::{Duration, Instant},
};

/// How long a found skin is kept before it is looked up again.
const KEEP_FOUND: Duration = Duration::from_secs(6 * 60 * 60);
/// How long "no such account" is kept.
const KEEP_MISSING: Duration = Duration::from_secs(10 * 60);
const PROFILE_BY_NAME: &str = "https://api.mojang.com/users/profiles/minecraft/";
const SESSION_PROFILE: &str = "https://sessionserver.mojang.com/session/minecraft/profile/";

const DEFAULT_FILE: &str = "# Skins and capes for offline-mode servers (online mode gets them anyway).\n\
# When a player joins without a skin, their name is looked up at Mojang and the\n\
# account's skin and cape are used. false keeps everyone on the default skins.\n\
enabled = true\n";

#[derive(serde::Deserialize)]
struct File {
    #[serde(default = "yes")]
    enabled: bool,
}

fn yes() -> bool {
    true
}

pub struct Skins {
    enabled: bool,
    cache: Mutex<HashMap<String, (Instant, Option<Vec<Property>>)>>,
}

impl Skins {
    /// From `path` (`plugins/skins.toml`), written with the defaults when missing.
    pub fn load(path: &Path) -> Self {
        if !path.exists() {
            let _ = std::fs::write(path, DEFAULT_FILE);
        }
        let enabled = std::fs::read_to_string(path)
            .ok()
            .and_then(|t| toml::from_str::<File>(&t).ok())
            .is_none_or(|f| f.enabled);
        Self {
            enabled,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Adds the account's `textures` to a player who has none.
    pub fn fill(&self, name: &str, properties: &mut Vec<Property>) {
        if !self.enabled || properties.iter().any(|p| p.name == "textures") {
            return;
        }
        let key = name.to_ascii_lowercase();
        let cached = self.cache.lock().unwrap().get(&key).cloned();
        let found = match cached {
            Some((at, found))
                if at.elapsed() < if found.is_some() { KEEP_FOUND } else { KEEP_MISSING } =>
            {
                found
            }
            _ => match lookup(name) {
                Ok(found) => {
                    self.cache
                        .lock()
                        .unwrap()
                        .insert(key, (Instant::now(), found.clone()));
                    found
                }
                Err(e) => {
                    log::warn!("skins: looking up {name}: {e}");
                    None
                }
            },
        };
        if let Some(found) = found {
            log::info!("skins: {name} gets their Mojang skin");
            properties.extend(found);
        }
    }
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(5))
        .build()
}

/// The account's signed properties; `Ok(None)` when no account has this name.
fn lookup(name: &str) -> Result<Option<Vec<Property>>, String> {
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return Ok(None);
    }
    let agent = agent();
    let Some(body) = get(&agent, &format!("{PROFILE_BY_NAME}{name}"))? else {
        return Ok(None);
    };
    let Some(id) = account_id(&body) else {
        return Ok(None);
    };
    let Some(body) = get(&agent, &format!("{SESSION_PROFILE}{id}?unsigned=false"))? else {
        return Ok(None);
    };
    Ok(textures(&body))
}

/// A 200's body; None for 204 or 404 (no such account).
fn get(agent: &ureq::Agent, url: &str) -> Result<Option<String>, String> {
    match agent.get(url).call() {
        Ok(r) if r.status() == 200 => r.into_string().map(Some).map_err(|e| e.to_string()),
        Ok(_) | Err(ureq::Error::Status(404, _)) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// `{"id": "<32 hex>", "name": ...}`.
fn account_id(body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(body).ok()?;
    let id = v.get("id")?.as_str()?;
    (id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())).then(|| id.to_owned())
}

/// The signed `textures` property of a session-server profile.
fn textures(body: &str) -> Option<Vec<Property>> {
    let v: Value = serde_json::from_str(body).ok()?;
    let found: Vec<Property> = v
        .get("properties")?
        .as_array()?
        .iter()
        .filter_map(|p| {
            let name = p.get("name")?.as_str()?;
            let signature = p.get("signature")?.as_str()?;
            let value = p.get("value")?.as_str()?;
            (name == "textures").then(|| Property {
                name: name.to_owned(),
                value: value.to_owned(),
                signature: Some(signature.to_owned()),
            })
        })
        .collect();
    (!found.is_empty()).then_some(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_account_id_and_the_signed_textures() {
        assert_eq!(
            account_id(r#"{"id":"069a79f444e94726a5befca90e38aaf5","name":"Notch"}"#).as_deref(),
            Some("069a79f444e94726a5befca90e38aaf5")
        );
        assert_eq!(account_id(r#"{"errorMessage":"no"}"#), None);
        let found = textures(
            r#"{"id":"x","name":"N","properties":[{"name":"textures","value":"e30=","signature":"c2ln"}]}"#,
        )
        .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].signature.as_deref(), Some("c2ln"));
        // Unsigned textures are no use: clients refuse them.
        assert!(
            textures(r#"{"properties":[{"name":"textures","value":"e30="}]}"#).is_none()
        );
    }

    #[test]
    fn players_with_textures_or_odd_names_are_left_alone() {
        let skins = Skins {
            enabled: true,
            cache: Mutex::new(HashMap::new()),
        };
        let mut props = vec![Property {
            name: "textures".into(),
            value: "v".into(),
            signature: None,
        }];
        skins.fill("Notch", &mut props);
        assert_eq!(props.len(), 1);
        assert_eq!(lookup("bad name!").unwrap(), None);
    }
}
