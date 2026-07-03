use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

pub const AGENT_RUN_ID_ENV: &str = "AGENT_RUN_ID";
pub const AGENT_RUN_PARENT_ENV: &str = "AGENT_RUN_PARENT";

const RUN_ID_PREFIX: &str = "ar-";
const ULID_LEN: usize = 26;
const RUN_ID_LEN: usize = RUN_ID_PREFIX.len() + ULID_LEN;
const CROCKFORD_LOWER: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RunId(String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunIdParseError {
    Empty,
    InvalidLength { actual: usize },
    InvalidPrefix,
    InvalidCharacter { index: usize, ch: char },
    TimestampOverflow,
}

impl fmt::Display for RunIdParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "run id is empty"),
            Self::InvalidLength { actual } => {
                write!(f, "run id must be {RUN_ID_LEN} characters, got {actual}")
            }
            Self::InvalidPrefix => write!(f, "run id must start with {RUN_ID_PREFIX}"),
            Self::InvalidCharacter { index, ch } => {
                write!(f, "invalid run id character {ch:?} at byte {index}")
            }
            Self::TimestampOverflow => write!(f, "run id ULID timestamp prefix is out of range"),
        }
    }
}

impl std::error::Error for RunIdParseError {}

impl RunId {
    pub fn new() -> Self {
        let millis = chrono::Utc::now().timestamp_millis().max(0) as u64;
        let mut bytes = [0_u8; 16];
        let timestamp = millis.to_be_bytes();
        bytes[..6].copy_from_slice(&timestamp[2..]);
        bytes[6..].copy_from_slice(&uuid::Uuid::new_v4().as_bytes()[..10]);
        Self(format!("{RUN_ID_PREFIX}{}", encode_ulid_lower(bytes)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn validate(value: &str) -> Result<(), RunIdParseError> {
        parse_run_id(value).map(|_| ())
    }
}

impl Default for RunId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<RunId> for String {
    fn from(value: RunId) -> Self {
        value.0
    }
}

impl TryFrom<String> for RunId {
    type Error = RunIdParseError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        parse_run_id(&value)?;
        Ok(Self(value))
    }
}

impl FromStr for RunId {
    type Err = RunIdParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_run_id(value)?;
        Ok(Self(value.to_string()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunIdentity {
    pub run_id: RunId,
    pub parent: Option<RunId>,
}

impl RunIdentity {
    pub fn mint() -> Self {
        Self {
            run_id: RunId::new(),
            parent: None,
        }
    }

    pub fn from_env() -> Result<Option<Self>, RunIdParseError> {
        let run_id = std::env::var(AGENT_RUN_ID_ENV).ok();
        let parent = std::env::var(AGENT_RUN_PARENT_ENV).ok();
        Self::from_env_values(run_id.as_deref(), parent.as_deref())
    }

    pub fn from_env_vars(
        env_vars: &HashMap<String, String>,
    ) -> Result<Option<Self>, RunIdParseError> {
        let env_run_id = std::env::var(AGENT_RUN_ID_ENV).ok();
        let env_parent = std::env::var(AGENT_RUN_PARENT_ENV).ok();
        let run_id = env_vars
            .get(AGENT_RUN_ID_ENV)
            .map(String::as_str)
            .filter(|value| !value.is_empty())
            .or_else(|| env_run_id.as_deref().filter(|value| !value.is_empty()));
        let parent = env_vars
            .get(AGENT_RUN_PARENT_ENV)
            .map(String::as_str)
            .filter(|value| !value.is_empty())
            .or_else(|| env_parent.as_deref().filter(|value| !value.is_empty()));
        Self::from_env_values(run_id, parent)
    }

    pub fn from_env_values(
        run_id: Option<&str>,
        parent: Option<&str>,
    ) -> Result<Option<Self>, RunIdParseError> {
        let Some(run_id) = run_id.filter(|value| !value.is_empty()) else {
            return Ok(None);
        };
        let run_id = RunId::from_str(run_id)?;
        let parent = parent
            .filter(|value| !value.is_empty())
            .map(RunId::from_str)
            .transpose()?;
        Ok(Some(Self { run_id, parent }))
    }

    pub fn mint_if_absent() -> Result<Self, RunIdParseError> {
        Ok(Self::from_env()?.unwrap_or_else(Self::mint))
    }

    pub fn ensure_env_vars(
        env_vars: &mut HashMap<String, String>,
    ) -> Result<Self, RunIdParseError> {
        let identity = Self::from_env_vars(env_vars)?.unwrap_or_else(Self::mint);
        identity.write_env_vars(env_vars);
        Ok(identity)
    }

    pub fn mint_nested() -> Result<Self, RunIdParseError> {
        let parent = Self::from_env()?.map(|identity| identity.run_id);
        Ok(Self {
            run_id: RunId::new(),
            parent,
        })
    }

    pub fn env_pairs(&self) -> Vec<(&'static str, String)> {
        let mut pairs = vec![(AGENT_RUN_ID_ENV, self.run_id.to_string())];
        if let Some(parent) = &self.parent {
            pairs.push((AGENT_RUN_PARENT_ENV, parent.to_string()));
        }
        pairs
    }

    pub fn write_env_vars(&self, env_vars: &mut HashMap<String, String>) {
        env_vars.insert(AGENT_RUN_ID_ENV.to_string(), self.run_id.to_string());
        if let Some(parent) = &self.parent {
            env_vars.insert(AGENT_RUN_PARENT_ENV.to_string(), parent.to_string());
        } else {
            env_vars.remove(AGENT_RUN_PARENT_ENV);
        }
    }
}

fn parse_run_id(value: &str) -> Result<(), RunIdParseError> {
    if value.is_empty() {
        return Err(RunIdParseError::Empty);
    }
    if value.len() != RUN_ID_LEN {
        return Err(RunIdParseError::InvalidLength {
            actual: value.len(),
        });
    }
    let Some(ulid) = value.strip_prefix(RUN_ID_PREFIX) else {
        return Err(RunIdParseError::InvalidPrefix);
    };
    for (offset, ch) in ulid.char_indices() {
        let index = RUN_ID_PREFIX.len() + offset;
        if !CROCKFORD_LOWER.contains(&(ch as u8)) {
            return Err(RunIdParseError::InvalidCharacter { index, ch });
        }
    }
    if ulid.as_bytes()[0] > b'7' {
        return Err(RunIdParseError::TimestampOverflow);
    }
    Ok(())
}

fn encode_ulid_lower(bytes: [u8; 16]) -> String {
    let mut value = u128::from_be_bytes(bytes);
    let mut encoded = [b'0'; ULID_LEN];
    for slot in encoded.iter_mut().rev() {
        *slot = CROCKFORD_LOWER[(value & 0b11111) as usize];
        value >>= 5;
    }
    String::from_utf8(encoded.to_vec()).expect("ULID alphabet is valid UTF-8")
}

#[cfg(test)]
mod run_id_tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn run_id_mints_lowercase_prefixed_ulid() {
        let run_id = RunId::new();
        assert!(run_id.as_str().starts_with("ar-"));
        assert_eq!(run_id.as_str().len(), 29);
        assert!(RunId::validate(run_id.as_str()).is_ok());
        assert_eq!(run_id.as_str(), run_id.as_str().to_ascii_lowercase());
    }

    #[test]
    fn run_id_rejects_invalid_values() {
        assert!(RunId::from_str("").is_err());
        assert!(RunId::from_str("01j1qb3c9r7v5m2k8x4tznq6wd").is_err());
        assert!(RunId::from_str("ar-01J1QB3C9R7V5M2K8X4TZNQ6WD").is_err());
        assert!(RunId::from_str("ar-81j1qb3c9r7v5m2k8x4tznq6wd").is_err());
        assert!(RunId::from_str("ar-01j1qb3c9r7v5m2k8x4tznq6wu").is_err());
    }

    #[test]
    fn run_id_env_resolution_honors_existing_value() {
        let _guard = env_lock().lock().unwrap();
        let original = std::env::var(AGENT_RUN_ID_ENV).ok();
        let expected = "ar-01j1qb3c9r7v5m2k8x4tznq6wd";
        unsafe { std::env::set_var(AGENT_RUN_ID_ENV, expected) };

        let identity = RunIdentity::mint_if_absent().expect("valid env");
        assert_eq!(identity.run_id.as_str(), expected);
        assert!(identity.parent.is_none());

        match original {
            Some(value) => unsafe { std::env::set_var(AGENT_RUN_ID_ENV, value) },
            None => unsafe { std::env::remove_var(AGENT_RUN_ID_ENV) },
        }
    }

    #[test]
    fn run_id_nested_mint_hands_off_parent() {
        let _guard = env_lock().lock().unwrap();
        let original_id = std::env::var(AGENT_RUN_ID_ENV).ok();
        let original_parent = std::env::var(AGENT_RUN_PARENT_ENV).ok();
        let parent = "ar-01j1qb3c9r7v5m2k8x4tznq6wd";
        unsafe { std::env::set_var(AGENT_RUN_ID_ENV, parent) };
        unsafe { std::env::remove_var(AGENT_RUN_PARENT_ENV) };

        let identity = RunIdentity::mint_nested().expect("nested mint");
        assert_ne!(identity.run_id.as_str(), parent);
        assert_eq!(identity.parent.as_ref().map(RunId::as_str), Some(parent));
        let env_pairs = identity.env_pairs();
        assert!(env_pairs.iter().any(|(key, _)| *key == AGENT_RUN_ID_ENV));
        assert!(env_pairs
            .iter()
            .any(|(key, value)| *key == AGENT_RUN_PARENT_ENV && value == parent));

        match original_id {
            Some(value) => unsafe { std::env::set_var(AGENT_RUN_ID_ENV, value) },
            None => unsafe { std::env::remove_var(AGENT_RUN_ID_ENV) },
        }
        match original_parent {
            Some(value) => unsafe { std::env::set_var(AGENT_RUN_PARENT_ENV, value) },
            None => unsafe { std::env::remove_var(AGENT_RUN_PARENT_ENV) },
        }
    }

    #[test]
    fn run_id_from_env_vars_prefers_request_values() {
        let _guard = env_lock().lock().unwrap();
        let original_id = std::env::var(AGENT_RUN_ID_ENV).ok();
        let original_parent = std::env::var(AGENT_RUN_PARENT_ENV).ok();
        unsafe { std::env::set_var(AGENT_RUN_ID_ENV, "ar-01j1qb3c9r7v5m2k8x4tznq6wf") };
        unsafe { std::env::remove_var(AGENT_RUN_PARENT_ENV) };
        let mut env_vars = HashMap::new();
        env_vars.insert(
            AGENT_RUN_ID_ENV.to_string(),
            "ar-01j1qb3c9r7v5m2k8x4tznq6wd".to_string(),
        );
        env_vars.insert(
            AGENT_RUN_PARENT_ENV.to_string(),
            "ar-01j1qb3c9r7v5m2k8x4tznq6we".to_string(),
        );

        let identity = RunIdentity::from_env_vars(&env_vars)
            .expect("valid identity")
            .expect("identity");

        assert_eq!(identity.run_id.as_str(), "ar-01j1qb3c9r7v5m2k8x4tznq6wd");
        assert_eq!(
            identity.parent.as_ref().map(RunId::as_str),
            Some("ar-01j1qb3c9r7v5m2k8x4tznq6we")
        );

        match original_id {
            Some(value) => unsafe { std::env::set_var(AGENT_RUN_ID_ENV, value) },
            None => unsafe { std::env::remove_var(AGENT_RUN_ID_ENV) },
        }
        match original_parent {
            Some(value) => unsafe { std::env::set_var(AGENT_RUN_PARENT_ENV, value) },
            None => unsafe { std::env::remove_var(AGENT_RUN_PARENT_ENV) },
        }
    }

    #[test]
    fn run_id_ensure_env_vars_mints_and_writes_request_env() {
        let _guard = env_lock().lock().unwrap();
        let original_id = std::env::var(AGENT_RUN_ID_ENV).ok();
        let original_parent = std::env::var(AGENT_RUN_PARENT_ENV).ok();
        unsafe { std::env::remove_var(AGENT_RUN_ID_ENV) };
        unsafe { std::env::remove_var(AGENT_RUN_PARENT_ENV) };
        let mut env_vars = HashMap::new();

        let identity = RunIdentity::ensure_env_vars(&mut env_vars).expect("identity");

        assert_eq!(
            env_vars.get(AGENT_RUN_ID_ENV).map(String::as_str),
            Some(identity.run_id.as_str())
        );
        assert!(!env_vars.contains_key(AGENT_RUN_PARENT_ENV));

        match original_id {
            Some(value) => unsafe { std::env::set_var(AGENT_RUN_ID_ENV, value) },
            None => unsafe { std::env::remove_var(AGENT_RUN_ID_ENV) },
        }
        match original_parent {
            Some(value) => unsafe { std::env::set_var(AGENT_RUN_PARENT_ENV, value) },
            None => unsafe { std::env::remove_var(AGENT_RUN_PARENT_ENV) },
        }
    }
}
