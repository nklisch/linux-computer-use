//! Optional, bounded AT-SPI metadata inspection. No text/value interfaces are read.
//!
//! Focus is an explicit mutation and only accepts references from the latest
//! completed inspection. None of this module is required for portal capture/input.
use std::{
    collections::{HashMap, HashSet, VecDeque, hash_map::RandomState},
    hash::BuildHasher,
    time::Duration,
};

use crate::types::{FocusCheck, FocusState};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::time::{Instant, timeout, timeout_at};
use zbus::{
    Connection,
    zvariant::{DynamicType, OwnedObjectPath, OwnedValue, Type},
};

const ACCESSIBLE: &str = "org.a11y.atspi.Accessible";
const COMPONENT: &str = "org.a11y.atspi.Component";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
const ROOT: &str = "/org/a11y/atspi/accessible/root";
const CALL_BUDGET: Duration = Duration::from_millis(350);
const INSPECT_BUDGET: Duration = Duration::from_secs(5);
const MAX_NODES: usize = 500;
const MAX_WARNINGS: usize = 32;
// /usr/include/at-spi-2.0/atspi/atspi-constants.h: AtspiStateType, zero-based.
const ACTIVE: usize = 1;
const FOCUSABLE: usize = 11;
const FOCUSED: usize = 12;

type Reference = (String, OwnedObjectPath);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Inspection {
    pub nodes: Vec<Node>,
    pub truncated: bool,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub name: String,
    pub role: String,
    pub focused: bool,
    pub active: bool,
    pub focusable: bool,
    /// False means the state query failed, not that the widget lacks focus.
    pub state_available: bool,
    /// Applications are depth zero; their immediate children are depth one.
    pub depth: usize,
    pub parent_id: Option<String>,
}

pub struct Accessibility {
    connection: Connection,
    latest: InspectionIds,
}

#[derive(Default)]
struct InspectionIds {
    references: HashMap<String, Reference>,
    windows: HashMap<String, Reference>,
    generation: u64,
    nonce: u64,
}

impl InspectionIds {
    fn begin(&mut self) {
        self.references.clear();
        self.windows.clear();
        self.generation = self.generation.wrapping_add(1);
        self.nonce = RandomState::new().hash_one(self.generation);
    }
    fn id(&self, index: usize) -> String {
        format!("a11y_{:016x}_{}_{}", self.nonce, self.generation, index)
    }
    fn resolve(&self, id: &str) -> Result<&Reference> {
        self.references
            .get(id)
            .context("Unknown or stale accessibility node ID; inspect again before focusing")
    }
}

struct PendingNode {
    reference: Reference,
    depth: usize,
    parent_id: Option<String>,
}

impl Accessibility {
    pub async fn connect() -> Result<Self> {
        let connection = timeout(INSPECT_BUDGET, async {
            let session = Connection::session().await?;
            let reply = session
                .call_method(
                    Some("org.a11y.Bus"),
                    "/org/a11y/bus",
                    Some("org.a11y.Bus"),
                    "GetAddress",
                    &(),
                )
                .await?;
            let address: String = reply.body().deserialize()?;
            zbus::connection::Builder::address(address.as_str())?
                .build()
                .await
        })
        .await
        .context("Accessibility bus connection timed out")?
        .context("Accessibility bus unavailable")?;
        Ok(Self {
            connection,
            latest: InspectionIds::default(),
        })
    }

    /// Without a filter, returns only app/immediate-child metadata. A nonempty
    /// filter matches application names before any descendants are traversed.
    pub async fn inspect(
        &mut self,
        application: Option<&str>,
        max_nodes: usize,
    ) -> Result<Inspection> {
        self.latest.begin();
        let limit = max_nodes.clamp(1, MAX_NODES);
        let filter = application
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_lowercase);
        let depth_limit = if filter.is_some() { 6 } else { 1 };
        let deadline = Instant::now() + INSPECT_BUDGET;
        let mut result = Inspection {
            nodes: vec![],
            truncated: false,
            warnings: vec![],
        };
        if max_nodes != limit {
            warn(
                &mut result,
                format!("max_nodes clamped to {limit} (supported range 1–500)"),
            );
        }
        let root = (
            "org.a11y.atspi.Registry".to_owned(),
            OwnedObjectPath::try_from(ROOT)?,
        );
        let mut queue = VecDeque::new();
        // Root enumeration is bounded independently of the result limit so an
        // application filter can find a later application even with max_nodes=1.
        let roots = children(&self.connection, &root, MAX_NODES, deadline, &mut result).await;
        for reference in roots {
            queue.push_back(PendingNode {
                reference,
                depth: 0,
                parent_id: None,
            });
        }
        let mut seen = HashSet::new();
        let mut references = HashMap::new();
        while let Some(pending) = queue.pop_front() {
            if Instant::now() >= deadline || result.nodes.len() >= limit {
                result.truncated = true;
                break;
            }
            if !seen.insert(pending.reference.clone()) {
                continue;
            }
            let (role_result, state_result) = tokio::join!(
                invoke::<_, String>(
                    &self.connection,
                    &pending.reference,
                    ACCESSIBLE,
                    "GetRoleName",
                    &(),
                    deadline
                ),
                invoke::<_, Vec<u32>>(
                    &self.connection,
                    &pending.reference,
                    ACCESSIBLE,
                    "GetState",
                    &(),
                    deadline
                ),
            );
            let role = match role_result {
                Ok(role) => role,
                Err(_) => {
                    warn(&mut result, "Some node roles were unavailable".into());
                    "unknown".into()
                }
            };
            let private = private_role(&role) || role == "unknown" || role.is_empty();
            // Password widget names are normally labels, but do not rely on every
            // toolkit getting that right. Unknown roles also omit name queries.
            let name = if private || role == "unknown" {
                String::new()
            } else {
                match property::<String>(&self.connection, &pending.reference, "Name", deadline)
                    .await
                {
                    Ok(name) => bounded_name(&name),
                    Err(_) => {
                        warn(&mut result, "Some node names were unavailable".into());
                        String::new()
                    }
                }
            };
            if pending.depth == 0 && !matches_application(&name, filter.as_deref()) {
                continue;
            }
            let state_available = state_result.is_ok();
            if !state_available {
                warn(
                    &mut result,
                    "Some node states were unavailable; false focus flags may mean unknown".into(),
                );
            }
            let states = state_result.unwrap_or_default();
            let id = self.latest.id(result.nodes.len());
            result.nodes.push(Node {
                id: id.clone(),
                name,
                role,
                focused: state_set(&states, FOCUSED),
                active: state_set(&states, ACTIVE),
                focusable: state_set(&states, FOCUSABLE),
                state_available,
                depth: pending.depth,
                parent_id: pending.parent_id,
            });
            references.insert(id.clone(), pending.reference.clone());
            if !private && pending.depth < depth_limit {
                let available = limit
                    .saturating_sub(result.nodes.len())
                    .min(MAX_NODES.saturating_sub(queue.len()));
                for reference in children(
                    &self.connection,
                    &pending.reference,
                    available,
                    deadline,
                    &mut result,
                )
                .await
                {
                    queue.push_back(PendingNode {
                        reference,
                        depth: pending.depth + 1,
                        parent_id: Some(id.clone()),
                    });
                }
            }
        }
        if Instant::now() >= deadline {
            result.truncated = true;
            warn(
                &mut result,
                "Inspection reached its five-second time budget".into(),
            );
        }
        if result.truncated {
            warn(
                &mut result,
                "Results are partial; narrow the application filter or increase max_nodes".into(),
            );
        }
        if filter.is_none() {
            warn(&mut result, "Unfiltered inspection includes only applications and immediate children; provide an application filter for deeper inspection".into());
        } else {
            warn(&mut result, "Filtered inspection stops at depth six and omits password names/descendants; it is not a complete document tree".into());
        }
        // Publish references only after completing the inspection. Cancelling an
        // inspect future leaves the map empty, never targeting an unseen partial tree.
        self.latest.windows = inspected_windows(&result.nodes, &references);
        self.latest.references = references;
        Ok(result)
    }

    /// Check only inspected references, without reading names/text or traversing the desktop.
    /// Application state is advisory and cannot reserve focus after this check.
    pub async fn check_focus(&self, id: &str) -> FocusCheck {
        let Ok(reference) = self.latest.resolve(id) else {
            return FocusCheck::unknown("Unknown or stale node ID; inspect again");
        };
        let Some(window) = self.latest.windows.get(id) else {
            return FocusCheck::unknown("No inspected window ancestry for this node");
        };
        let deadline = Instant::now() + CALL_BUDGET;
        let (node, window) = tokio::join!(
            invoke::<_, Vec<u32>>(
                &self.connection,
                reference,
                ACCESSIBLE,
                "GetState",
                &(),
                deadline
            ),
            invoke::<_, Vec<u32>>(
                &self.connection,
                window,
                ACCESSIBLE,
                "GetState",
                &(),
                deadline
            ),
        );
        evaluate_focus(node.ok().as_deref(), window.ok().as_deref())
    }

    /// Asks the exact inspected component for focus. True is the application's
    /// acknowledgement, not proof of compositor activation or a subsequent paste.
    pub async fn focus(&self, id: &str) -> Result<bool> {
        let reference = self.latest.resolve(id)?;
        invoke(
            &self.connection,
            reference,
            COMPONENT,
            "GrabFocus",
            &(),
            Instant::now() + Duration::from_secs(2),
        )
        .await
    }
}

fn inspected_windows(
    nodes: &[Node],
    references: &HashMap<String, Reference>,
) -> HashMap<String, Reference> {
    let nodes_by_id: HashMap<_, _> = nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    nodes
        .iter()
        .filter_map(|node| {
            let mut current = node;
            // Inspection depth is bounded; this also contains malformed/cyclic metadata.
            for _ in 0..=6 {
                let parent = current
                    .parent_id
                    .as_deref()
                    .and_then(|id| nodes_by_id.get(id));
                // Chooser roles also describe embedded widgets. Only a window at
                // the inspected application's top level supplies ACTIVE state.
                let top_level = current.parent_id.is_none()
                    || parent.is_some_and(|node| node.role == "application");
                if top_level
                    && matches!(
                        current.role.as_str(),
                        "frame"
                            | "dialog"
                            | "window"
                            | "file chooser"
                            | "color chooser"
                            | "font chooser"
                            | "alert"
                    )
                {
                    return references
                        .get(&current.id)
                        .map(|r| (node.id.clone(), r.clone()));
                }
                current = parent?;
            }
            None
        })
        .collect()
}

fn evaluate_focus(node: Option<&[u32]>, window: Option<&[u32]>) -> FocusCheck {
    let (Some(node), Some(window)) = (
        node.filter(|s| !s.is_empty()),
        window.filter(|s| !s.is_empty()),
    ) else {
        return FocusCheck::unknown("Accessibility focus/window state unavailable or timed out");
    };
    if !state_set(node, FOCUSED) || !state_set(window, ACTIVE) {
        FocusCheck {
            state: FocusState::Mismatch,
            reason: "Expected node is not focused in an active window".into(),
        }
    } else {
        FocusCheck {
            state: FocusState::Confirmed,
            reason: "Application reports node focused and window active; focus is not reserved"
                .into(),
        }
    }
}

async fn invoke<B, R>(
    connection: &Connection,
    reference: &Reference,
    interface: &str,
    method: &str,
    body: &B,
    deadline: Instant,
) -> Result<R>
where
    B: Serialize + DynamicType,
    R: DeserializeOwned + Type,
{
    let end = deadline.min(Instant::now() + CALL_BUDGET);
    ensure!(
        Instant::now() < end,
        "Accessibility inspection time budget exhausted"
    );
    let message = timeout_at(
        end,
        connection.call_method(
            Some(reference.0.as_str()),
            reference.1.as_str(),
            Some(interface),
            method,
            body,
        ),
    )
    .await
    .with_context(|| format!("Accessibility {method} timed out; its outcome may be unknown"))??;
    Ok(message.body().deserialize()?)
}

async fn property<R>(
    connection: &Connection,
    reference: &Reference,
    name: &str,
    deadline: Instant,
) -> Result<R>
where
    R: TryFrom<OwnedValue>,
    R::Error: std::error::Error + Send + Sync + 'static,
{
    let value: OwnedValue = invoke(
        connection,
        reference,
        PROPERTIES,
        "Get",
        &(ACCESSIBLE, name),
        deadline,
    )
    .await?;
    Ok(R::try_from(value)?)
}

async fn children(
    connection: &Connection,
    reference: &Reference,
    limit: usize,
    deadline: Instant,
    result: &mut Inspection,
) -> Vec<Reference> {
    let count = match property::<i32>(connection, reference, "ChildCount", deadline).await {
        Ok(count) => count.max(0) as usize,
        Err(_) => {
            result.truncated = true;
            warn(result, "Some child lists were unavailable".into());
            return vec![];
        }
    };
    let take = count.min(limit);
    if take < count {
        result.truncated = true;
    }
    let mut children = Vec::with_capacity(take);
    // GetChildren can materialize an entire huge document even when only a small
    // result was requested. Indexed access bounds both disclosure and allocation.
    for index in 0..take {
        if Instant::now() >= deadline {
            result.truncated = true;
            break;
        }
        match invoke::<_, Reference>(
            connection,
            reference,
            ACCESSIBLE,
            "GetChildAtIndex",
            &(index as i32,),
            deadline,
        )
        .await
        {
            Ok(child) if child.1.as_str() != "/org/a11y/atspi/null" => children.push(child),
            Ok(_) => {}
            Err(_) => {
                result.truncated = true;
                warn(result, "Some child references were unavailable".into());
            }
        }
    }
    children
}

fn warn(result: &mut Inspection, warning: String) {
    if result.warnings.len() < MAX_WARNINGS && !result.warnings.contains(&warning) {
        result.warnings.push(warning);
    }
}
fn state_set(words: &[u32], state: usize) -> bool {
    words
        .get(state / 32)
        .is_some_and(|word| word & (1u32 << (state % 32)) != 0)
}
fn matches_application(name: &str, filter: Option<&str>) -> bool {
    filter.is_none_or(|filter| name.to_lowercase().contains(filter))
}
fn private_role(role: &str) -> bool {
    role.to_ascii_lowercase().contains("password")
}
fn bounded_name(name: &str) -> String {
    name.chars().take(512).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn focus_state_uses_bit_twelve() {
        assert!(!state_set(&[], FOCUSED));
        assert!(state_set(&[1 << 12], FOCUSED));
        assert!(!state_set(&[1 << 11], FOCUSED));
        assert!(state_set(&[0, 1 << 3], 35));
    }
    #[test]
    fn ids_only_resolve_in_latest_inspection() {
        let mut ids = InspectionIds::default();
        ids.begin();
        let id = ids.id(0);
        let reference = (":1.2".to_owned(), OwnedObjectPath::try_from(ROOT).unwrap());
        ids.references.insert(id.clone(), reference.clone());
        assert_eq!(ids.resolve(&id).unwrap(), &reference);
        assert!(ids.resolve(":1.2/org/a11y/atspi/accessible/root").is_err());
        ids.begin();
        assert!(ids.resolve(&id).is_err());
        assert_ne!(ids.id(0), id);
    }
    #[test]
    fn window_ancestry_is_inspection_scoped_and_missing_is_unknown() {
        let reference = (":1.2".into(), OwnedObjectPath::try_from(ROOT).unwrap());
        let node = |id: &str, role: &str, parent: Option<&str>| Node {
            id: id.into(),
            name: String::new(),
            role: role.into(),
            focused: false,
            active: false,
            focusable: false,
            state_available: true,
            depth: 1,
            parent_id: parent.map(str::to_owned),
        };
        let nodes = vec![
            node("window", "frame", None),
            node("field", "text", Some("window")),
            node("orphan", "text", Some("missing")),
            node("app", "application", None),
            node("chooser", "file chooser", Some("app")),
            node("embedded", "file chooser", Some("chooser")),
            node("chooser-field", "text", Some("embedded")),
        ];
        let chooser = (
            ":1.2".into(),
            OwnedObjectPath::try_from("/chooser").unwrap(),
        );
        let embedded = (
            ":1.2".into(),
            OwnedObjectPath::try_from("/embedded").unwrap(),
        );
        let refs = HashMap::from([
            ("window".into(), reference.clone()),
            ("chooser".into(), chooser.clone()),
            ("embedded".into(), embedded),
        ]);
        let windows = inspected_windows(&nodes, &refs);
        assert_eq!(windows.get("field"), Some(&reference));
        assert!(!windows.contains_key("orphan"));
        assert_eq!(windows.get("chooser-field"), Some(&chooser));
        assert_eq!(windows.get("embedded"), Some(&chooser));
        assert_eq!(windows.get("chooser"), Some(&chooser));
        let mut ids = InspectionIds {
            windows,
            ..InspectionIds::default()
        };
        ids.begin();
        assert!(ids.windows.is_empty());
    }

    #[test]
    fn focus_requires_known_node_state_and_active_window() {
        assert_eq!(
            evaluate_focus(Some(&[1 << FOCUSED]), Some(&[1 << ACTIVE])).state,
            FocusState::Confirmed
        );
        assert_eq!(
            evaluate_focus(Some(&[1 << FOCUSED]), Some(&[0])).state,
            FocusState::Mismatch
        );
        assert_eq!(
            evaluate_focus(Some(&[0]), Some(&[1 << ACTIVE])).state,
            FocusState::Mismatch
        );
        for unknown in [None, Some(&[][..])] {
            assert_eq!(
                evaluate_focus(unknown, Some(&[1 << ACTIVE])).state,
                FocusState::Unknown
            );
            assert_eq!(
                evaluate_focus(Some(&[1 << FOCUSED]), unknown).state,
                FocusState::Unknown
            );
        }
    }

    #[test]
    fn filter_and_names_are_bounded_without_breaking_unicode() {
        assert!(matches_application("Godot Engine", Some("godot")));
        assert!(!matches_application("Terminal", Some("godot")));
        assert!(matches_application("Terminal", None));
        assert_eq!(bounded_name(&"界".repeat(600)).chars().count(), 512);
        assert!(private_role("password text"));
    }
    #[test]
    fn node_limits_are_nonzero_and_bounded() {
        assert_eq!(0usize.clamp(1, MAX_NODES), 1);
        assert_eq!(100usize.clamp(1, MAX_NODES), 100);
        assert_eq!(usize::MAX.clamp(1, MAX_NODES), 500);
    }
    #[test]
    fn warnings_are_deduplicated_and_capped() {
        let mut result = Inspection {
            nodes: vec![],
            truncated: false,
            warnings: vec![],
        };
        for _ in 0..50 {
            warn(&mut result, "same".into());
        }
        assert_eq!(result.warnings.len(), 1);
        for i in 0..50 {
            warn(&mut result, i.to_string());
        }
        assert_eq!(result.warnings.len(), MAX_WARNINGS);
    }
}
