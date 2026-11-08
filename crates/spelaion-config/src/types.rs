//! Connection types (ADR-029): built-in Development, Test and Production, plus the team's
//! (`.spelaion/connection-types.toml`, only from a trusted repository) and the user's.
//!
//! A connection's `env` names a type by id. The looks of a type (name, colors, description)
//! come from the personal layer, then the team's, then the built-in one. Its safeguards only
//! add up across layers: no file can switch one off, so Production always confirms data
//! changes. Every decision is made here and in the core; the app only shows the result.

use std::collections::BTreeSet;
use std::path::PathBuf;

use crate::error::{ConfigError, ResolveError};
use crate::load::{Config, TypesLayer};
use crate::model::{ConnectionTypeConfig, ConnectionTypesFile, Tone};
use crate::trust::{ItemChange, TrustStore};

/// Production's id: every write there is confirmed.
pub const PRODUCTION: &str = "prod";
/// Another name of Production, as before ADR-029; it cannot be defined as a type of its own.
pub const PRODUCTION_ALIAS: &str = "production";

/// What a connection's type asks for before (or instead of) running things.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Safeguards {
    /// Ask before running any script from the editor.
    pub confirm_execution: bool,
    /// Ask before any statement that is not a read, and before applying table edits.
    pub confirm_data_changes: bool,
    /// Close the session after this many seconds without use (never while a transaction is
    /// open); `None`: never.
    pub close_idle_after: Option<u32>,
    /// Editor sessions start with auto-commit off (ADR-031).
    pub manual_commit: bool,
    /// The first write switches an auto-commit session to manual commit until that
    /// transaction ends (ADR-031).
    pub smart_commit: bool,
}

impl Safeguards {
    /// Both sets together: what either asks for, and the shorter idle time.
    pub fn tighten(self, other: Safeguards) -> Safeguards {
        Safeguards {
            confirm_execution: self.confirm_execution || other.confirm_execution,
            confirm_data_changes: self.confirm_data_changes || other.confirm_data_changes,
            close_idle_after: match (self.close_idle_after, other.close_idle_after) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            },
            manual_commit: self.manual_commit || other.manual_commit,
            smart_commit: self.smart_commit || other.smart_commit,
        }
    }

    fn of(config: &ConnectionTypeConfig) -> Safeguards {
        Safeguards {
            confirm_execution: config.confirm_execution,
            confirm_data_changes: config.confirm_data_changes,
            close_idle_after: config.close_idle_after.filter(|s| *s > 0),
            manual_commit: config.manual_commit,
            smart_commit: config.smart_commit,
        }
    }
}

/// A connection type with every layer applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionType {
    pub id: String,
    pub name: String,
    /// The tone of its badge.
    pub tone: Tone,
    pub description: Option<String>,
    pub built_in: bool,
    /// Defined (or changed) by the team's file.
    pub team: bool,
    /// What the personal layer says about it, if anything (for the Settings page).
    pub personal: Option<ConnectionTypeConfig>,
    /// Every layer together.
    pub safeguards: Safeguards,
    /// The built-in and team layers alone: what the user cannot switch off.
    pub inherited: Safeguards,
}

impl ConnectionType {
    pub fn is_production(&self) -> bool {
        self.id == PRODUCTION
    }
}

struct BuiltIn {
    id: &'static str,
    name: &'static str,
    tone: Tone,
    description: &'static str,
    safeguards: Safeguards,
}

const BUILT_IN: [BuiltIn; 3] = [
    BuiltIn {
        id: "dev",
        name: "Development",
        tone: Tone::Green,
        description: "Development database",
        safeguards: Safeguards {
            confirm_execution: false,
            confirm_data_changes: false,
            close_idle_after: None,
            manual_commit: false,
            smart_commit: false,
        },
    },
    BuiltIn {
        id: "test",
        name: "Test",
        tone: Tone::Amber,
        description: "Test (QA) database",
        safeguards: Safeguards {
            confirm_execution: false,
            confirm_data_changes: false,
            close_idle_after: None,
            manual_commit: false,
            smart_commit: false,
        },
    },
    BuiltIn {
        id: PRODUCTION,
        name: "Production",
        tone: Tone::Red,
        description: "Production database: every change is confirmed, and writes wait for Commit",
        safeguards: Safeguards {
            confirm_execution: false,
            confirm_data_changes: true,
            close_idle_after: None,
            manual_commit: false,
            smart_commit: true,
        },
    },
];

/// The type id a label names: case-insensitive, and `production` is Production.
pub fn type_id(label: &str) -> String {
    let id = label.trim().to_ascii_lowercase();
    if id == PRODUCTION_ALIAS {
        PRODUCTION.to_string()
    } else {
        id
    }
}

/// The connection types visible to a connection, and what stops them from being used.
#[derive(Debug, Clone, Default)]
pub struct ConnectionTypes {
    types: Vec<ConnectionType>,
    /// Layers whose file does not load.
    broken: Vec<ConfigError>,
    /// The team's file is not as the user trusted it: the types this change touches wait for
    /// a review.
    review: Option<(PathBuf, ItemChange, BTreeSet<String>)>,
}

impl ConnectionTypes {
    /// The types the app and the resolver use: built-in, the team's (only from a trusted
    /// repository) and the user's.
    pub fn load(config: &Config, trust: &TrustStore) -> Self {
        let mut out = Self::default();
        let mut layers: Vec<(Layer, &ConnectionTypesFile)> = Vec::new();
        if let Some(root) = config.workspace_root.as_deref()
            && trust.is_trusted(root)
        {
            match &config.team_types {
                TypesLayer::Broken(e) => out.broken.push(e.clone()),
                TypesLayer::Loaded { file, .. } => layers.push((Layer::Team, file)),
                TypesLayer::Absent => {}
            }
            if let Err(change) = trust.check_connection_types(root, config) {
                let touched = touched_types(&change);
                out.review = Some((root.to_path_buf(), change, touched));
            }
        }
        match &config.user_types {
            TypesLayer::Broken(e) => out.broken.push(e.clone()),
            TypesLayer::Loaded { file, .. } => layers.push((Layer::Personal, file)),
            TypesLayer::Absent => {}
        }
        out.types = merge(&layers);
        out
    }

    /// The repository's types alone, without trust or the user's (for `spelaiondb check`).
    pub fn of_repository(config: &Config) -> Self {
        let mut out = Self::default();
        let mut layers = Vec::new();
        match &config.team_types {
            TypesLayer::Broken(e) => out.broken.push(e.clone()),
            TypesLayer::Loaded { file, .. } => layers.push((Layer::Team, file)),
            TypesLayer::Absent => {}
        }
        out.types = merge(&layers);
        out
    }

    /// Every type: the built-in ones first, then the others by id.
    pub fn all(&self) -> &[ConnectionType] {
        &self.types
    }

    /// The type a label names, for showing it (its badge, its name). Never for deciding
    /// safeguards: that is [`ConnectionTypes::resolve`].
    pub fn get(&self, label: &str) -> Option<&ConnectionType> {
        let id = type_id(label);
        self.types.iter().find(|t| t.id == id)
    }

    /// The type of a connection with this label, for connecting: refused while a file it may
    /// come from does not load, or while the team changed it without the user's review.
    /// `None`: no label, or a plain label that names no type.
    pub fn resolve(&self, label: Option<&str>) -> Result<Option<ConnectionType>, ResolveError> {
        let Some(label) = label.filter(|l| !l.trim().is_empty()) else {
            return Ok(None);
        };
        if let Some(e) = self.broken.first() {
            return Err(ConfigError::new(
                &e.file,
                format!(
                    "the connection types could not be read ({}); connections with a type wait until the file is fixed",
                    e.message
                ),
            )
            .into());
        }
        let id = type_id(label);
        if let Some((root, change, touched)) = &self.review
            && touched.contains(&id)
        {
            return Err(ResolveError::Changed {
                root: root.clone(),
                change: change.clone(),
            });
        }
        Ok(self.get(&id).cloned())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Layer {
    Team,
    Personal,
}

fn merge(layers: &[(Layer, &ConnectionTypesFile)]) -> Vec<ConnectionType> {
    let mut ids: Vec<String> = BUILT_IN.iter().map(|b| b.id.to_string()).collect();
    let mut custom: BTreeSet<&String> = BTreeSet::new();
    for (_, file) in layers {
        custom.extend(file.types.keys());
    }
    ids.extend(
        custom
            .into_iter()
            .filter(|id| !BUILT_IN.iter().any(|b| b.id == id.as_str()))
            .cloned(),
    );
    ids.into_iter()
        .map(|id| {
            let built_in = BUILT_IN.iter().find(|b| b.id == id);
            let defined = |layer: Layer| {
                layers
                    .iter()
                    .find(|(l, _)| *l == layer)
                    .and_then(|(_, file)| file.types.get(&id))
            };
            let team = defined(Layer::Team);
            let personal = defined(Layer::Personal);
            // Looks: personal, then team, then built-in.
            let pick = |field: fn(&ConnectionTypeConfig) -> &Option<String>| {
                personal
                    .and_then(|p| field(p).clone())
                    .or_else(|| team.and_then(|t| field(t).clone()))
            };
            let inherited = built_in
                .map(|b| b.safeguards)
                .unwrap_or_default()
                .tighten(team.map(Safeguards::of).unwrap_or_default());
            ConnectionType {
                name: pick(|c| &c.name)
                    .map(|n| n.trim().to_string())
                    .or_else(|| built_in.map(|b| b.name.to_string()))
                    .unwrap_or_else(|| id.clone()),
                tone: personal
                    .and_then(|p| p.tone)
                    .or_else(|| team.and_then(|t| t.tone))
                    .or_else(|| built_in.map(|b| b.tone))
                    .unwrap_or_default(),
                description: pick(|c| &c.description)
                    .or_else(|| built_in.map(|b| b.description.to_string())),
                built_in: built_in.is_some(),
                team: team.is_some(),
                personal: personal.cloned(),
                safeguards: inherited.tighten(personal.map(Safeguards::of).unwrap_or_default()),
                inherited,
                id,
            }
        })
        .collect()
}

/// The type ids a change of the team's file touches (`types.<id>.<setting>` fields).
fn touched_types(change: &ItemChange) -> BTreeSet<String> {
    change
        .fields
        .iter()
        .filter_map(|f| f.field.strip_prefix("types."))
        .filter_map(|rest| rest.split_once('.').map(|(id, _)| id.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(types: &[(&str, ConnectionTypeConfig)]) -> ConnectionTypesFile {
        ConnectionTypesFile {
            schema_version: 1,
            types: types
                .iter()
                .map(|(id, c)| (id.to_string(), c.clone()))
                .collect(),
        }
    }

    #[test]
    fn labels_name_types_case_insensitively_and_production_is_prod() {
        assert_eq!(type_id(" PROD "), "prod");
        assert_eq!(type_id("Production"), "prod");
        assert_eq!(type_id("prodlike"), "prodlike");
        let types = ConnectionTypes {
            types: merge(&[]),
            ..Default::default()
        };
        assert!(types.get("Production").unwrap().is_production());
        assert!(types.get("prodlike").is_none());
        assert_eq!(types.resolve(Some("staging")).unwrap(), None);
        assert_eq!(types.resolve(Some("  ")).unwrap(), None);
        let prod = types.resolve(Some("PROD")).unwrap().unwrap();
        assert!(prod.safeguards.confirm_data_changes);
        assert!(!prod.safeguards.confirm_execution);
        assert!(!types.get("dev").unwrap().safeguards.confirm_data_changes);
    }

    #[test]
    fn no_layer_can_switch_a_safeguard_off() {
        // A team file that redefines Production "without" confirmations, and a personal one
        // that does the same for a team type: both only change looks.
        let team = file(&[
            (
                "prod",
                ConnectionTypeConfig {
                    name: Some("Prod EU".into()),
                    tone: Some(Tone::Green),
                    ..Default::default()
                },
            ),
            (
                "critical",
                ConnectionTypeConfig {
                    confirm_execution: true,
                    close_idle_after: Some(600),
                    ..Default::default()
                },
            ),
        ]);
        let personal = file(&[
            (
                "critical",
                ConnectionTypeConfig {
                    tone: Some(Tone::Blue),
                    close_idle_after: Some(3600),
                    ..Default::default()
                },
            ),
            (
                "prod",
                ConnectionTypeConfig {
                    confirm_execution: true,
                    ..Default::default()
                },
            ),
        ]);
        let types = merge(&[(Layer::Team, &team), (Layer::Personal, &personal)]);
        let get = |id: &str| types.iter().find(|t| t.id == id).unwrap();
        let prod = get("prod");
        assert_eq!(prod.name, "Prod EU");
        assert_eq!(prod.tone, Tone::Green);
        assert!(prod.safeguards.confirm_data_changes);
        assert!(prod.safeguards.confirm_execution);
        assert!(!prod.inherited.confirm_execution);
        let critical = get("critical");
        assert_eq!(critical.name, "critical");
        assert_eq!(critical.tone, Tone::Blue);
        // A type no layer gives a tone is neutral; the built-in ones have theirs.
        assert_eq!(get("dev").tone, Tone::Green);
        assert_eq!(get("test").tone, Tone::Amber);
        assert!(critical.safeguards.confirm_execution);
        assert_eq!(critical.safeguards.close_idle_after, Some(600));
        assert!(critical.team && !critical.built_in);
    }

    #[test]
    fn a_broken_file_refuses_every_label() {
        let types = ConnectionTypes {
            types: merge(&[]),
            broken: vec![ConfigError::new(
                std::path::Path::new("connection-types.toml"),
                "bad",
            )],
            review: None,
        };
        assert!(types.resolve(Some("staging")).is_err());
        assert!(types.resolve(Some("prod")).is_err());
        assert_eq!(types.resolve(None).unwrap(), None);
    }
}
