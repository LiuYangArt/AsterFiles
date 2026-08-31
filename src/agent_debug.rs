use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

use crate::domain::{
    LoadState, TabId, TabSession,
    file_operations::{
        ConflictCategory, FileOperationKind, FileSnapshot, ItemState, OperationConflict,
        OperationItem, OperationManager, OperationResult, OperationState,
    },
};

const DEFAULT_STATE_DIR: &str = "artifacts/state";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageOperation {
    RequestWindowsAccess,
}

impl PageOperation {
    fn name(self) -> &'static str {
        match self {
            Self::RequestWindowsAccess => "request_windows_access",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentScenario {
    PermissionDenied,
    FileOperationRunning,
    FileOperationConflict,
    FileOperationPartial,
    DragDropFoundation,
}

impl AgentScenario {
    pub fn name(self) -> &'static str {
        match self {
            Self::PermissionDenied => "permission-denied",
            Self::FileOperationRunning => "file-operation-running",
            Self::FileOperationConflict => "file-operation-conflict",
            Self::FileOperationPartial => "file-operation-partial",
            Self::DragDropFoundation => "drag-drop-foundation",
        }
    }

    fn default_path(self) -> PathBuf {
        Path::new(DEFAULT_STATE_DIR).join(format!("{}.json", self.name()))
    }
}

#[derive(Debug, Default)]
pub struct AgentOptions {
    pub scenario: Option<AgentScenario>,
    pub state_output: Option<PathBuf>,
    pub no_ui: bool,
}

impl AgentOptions {
    pub fn from_env() -> Result<Self, String> {
        let mut options = Self::default();
        let mut arguments = env::args_os().skip(1);
        while let Some(argument) = arguments.next() {
            match argument.to_string_lossy().as_ref() {
                "--agent-scenario" => {
                    let value = arguments
                        .next()
                        .ok_or_else(|| "--agent-scenario requires a value".to_owned())?;
                    options.scenario = Some(parse_scenario(&value.to_string_lossy())?);
                }
                "--agent-state-out" => {
                    options.state_output =
                        Some(PathBuf::from(arguments.next().ok_or_else(|| {
                            "--agent-state-out requires a path".to_owned()
                        })?));
                }
                "--no-ui" => options.no_ui = true,
                unknown => return Err(format!("unknown argument: {unknown}")),
            }
        }
        if options.no_ui && options.scenario.is_none() {
            return Err("--no-ui requires --agent-scenario".to_owned());
        }
        if options.state_output.is_some() && options.scenario.is_none() {
            return Err("--agent-state-out requires --agent-scenario".to_owned());
        }
        Ok(options)
    }

    pub fn state_output(&self) -> Option<PathBuf> {
        self.scenario.map(|scenario| {
            self.state_output
                .clone()
                .unwrap_or_else(|| scenario.default_path())
        })
    }
}

fn parse_scenario(value: &str) -> Result<AgentScenario, String> {
    match value {
        "permission-denied" => Ok(AgentScenario::PermissionDenied),
        "file-operation-running" => Ok(AgentScenario::FileOperationRunning),
        "file-operation-conflict" => Ok(AgentScenario::FileOperationConflict),
        "file-operation-partial" => Ok(AgentScenario::FileOperationPartial),
        "drag-drop-foundation" => Ok(AgentScenario::DragDropFoundation),
        _ => Err(format!("unknown agent scenario: {value}")),
    }
}

pub fn apply_scenario(session: &mut TabSession, scenario: AgentScenario) {
    match scenario {
        AgentScenario::PermissionDenied => {
            session.current_path = Some(PathBuf::from(r"C:\AgentScenarios"));
            session.requested_path = Some(PathBuf::from(r"C:\AgentScenarios\PermissionDenied"));
            session.load_state = LoadState::PermissionDenied;
            session.error = Some("permission denied".to_owned());
        }
        AgentScenario::DragDropFoundation => {
            session.current_path = Some(PathBuf::from(r"C:\AgentScenarios\DragDrop"));
            session.load_state = LoadState::Complete;
        }
        AgentScenario::FileOperationRunning
        | AgentScenario::FileOperationConflict
        | AgentScenario::FileOperationPartial => {
            session.current_path = Some(PathBuf::from(r"C:\AgentScenarios\FileOperations"));
            session.load_state = LoadState::Complete;
            session.error = Some(scenario.name().to_owned());
        }
    }
}

pub fn export_state(
    session: &TabSession,
    scenario: AgentScenario,
    output: &Path,
) -> io::Result<()> {
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    let state = AgentState::from_session(session, scenario);
    fs::write(output, state.to_json())
}

#[derive(Debug, PartialEq, Eq)]
struct AgentState {
    scenario: &'static str,
    current_path: String,
    page_state: &'static str,
    visible_page_operations: Vec<&'static str>,
    error_type: Option<&'static str>,
    drag_drop: Option<crate::platform::windows::drag_drop::DragDropState>,
}

impl AgentState {
    fn from_session(session: &TabSession, scenario: AgentScenario) -> Self {
        let projection = page_projection(session.load_state, session.entries.is_empty());
        let operation_state = operation_state_for_scenario(scenario);
        Self {
            scenario: scenario.name(),
            current_path: session
                .visible_path()
                .map(|path| path.as_os_str().to_string_lossy().into_owned())
                .unwrap_or_default(),
            page_state: projection.name,
            visible_page_operations: projection
                .visible_page_operations
                .iter()
                .copied()
                .map(PageOperation::name)
                .collect(),
            error_type: operation_state.or(projection.error_type),
            drag_drop: (scenario == AgentScenario::DragDropFoundation)
                .then(crate::platform::windows::drag_drop::current_state),
        }
    }

    fn to_json(&self) -> String {
        let operations = self
            .visible_page_operations
            .iter()
            .map(|operation| format!("\"{}\"", escape_json(operation)))
            .collect::<Vec<_>>()
            .join(", ");
        let error_type = self
            .error_type
            .map(|value| format!("\"{}\"", escape_json(value)))
            .unwrap_or_else(|| "null".to_owned());
        let drag_drop = self.drag_drop.as_ref().map_or_else(
            || "null".to_owned(),
            |state| {
                let target = state
                    .target
                    .as_ref()
                    .map(|value| format!("\"{}\"", escape_json(value)))
                    .unwrap_or_else(|| "null".to_owned());
                let reason = state
                    .rejection_reason
                    .map(|value| format!("\"{}\"", escape_json(value)))
                    .unwrap_or_else(|| "null".to_owned());
                let last_event = state
                    .last_event
                    .map(|value| format!("\"{}\"", value.name()))
                    .unwrap_or_else(|| "null".to_owned());
                format!(
                    "{{\"lifecycle\":\"{}\",\"registered\":{},\"source_count\":{},\"target\":{},\"negotiated_effect\":\"{}\",\"rejection_reason\":{},\"last_event\":{},\"event_sequence\":{}}}",
                    state.lifecycle.name(),
                    state.registered,
                    state.source_count,
                    target,
                    state.negotiated_effect,
                    reason,
                    last_event,
                    state.event_sequence,
                )
            },
        );
        format!(
            "{{\n  \"schema_version\": 1,\n  \"scenario\": \"{}\",\n  \"current_path\": \"{}\",\n  \"page_state\": \"{}\",\n  \"visible_page_operations\": [{}],\n  \"error_type\": {},\n  \"drag_drop\": {}\n}}\n",
            escape_json(self.scenario),
            escape_json(&self.current_path),
            escape_json(self.page_state),
            operations,
            error_type,
            drag_drop,
        )
    }
}

fn operation_state_for_scenario(scenario: AgentScenario) -> Option<&'static str> {
    if matches!(
        scenario,
        AgentScenario::PermissionDenied | AgentScenario::DragDropFoundation
    ) {
        return None;
    }
    let mut manager = OperationManager::new();
    let id = manager.submit(
        FileOperationKind::Copy,
        Some(TabId(1)),
        vec![OperationItem::pending(
            Some(PathBuf::from(r"C:AgentScenariossource.txt")),
            Some(PathBuf::from(r"C:AgentScenarios	arget.txt")),
        )],
    );
    let _ = manager.start_next();
    let _ = manager.mark_running(id);
    match scenario {
        AgentScenario::FileOperationRunning => {}
        AgentScenario::FileOperationConflict => {
            let task = manager.task_mut(id).expect("scenario task exists");
            let _ = task.set_conflict(OperationConflict {
                category: ConflictCategory::ExistingFile,
                source: FileSnapshot {
                    path: PathBuf::from(r"C:AgentScenariossource.txt"),
                    is_directory: false,
                    size_bytes: Some(64),
                    modified: None,
                },
                destination: FileSnapshot {
                    path: PathBuf::from(r"C:AgentScenarios	arget.txt"),
                    is_directory: false,
                    size_bytes: Some(32),
                    modified: None,
                },
            });
        }
        AgentScenario::FileOperationPartial => {
            manager.task_mut(id).expect("scenario task exists").items[0].state = ItemState::Failed;
            let _ = manager.finish(
                id,
                OperationState::PartiallyCompleted,
                OperationResult {
                    succeeded: vec![PathBuf::from(r"C:AgentScenarioscopied.txt")],
                    skipped: vec![],
                    failed: vec![(
                        PathBuf::from(r"C:AgentScenariossource.txt"),
                        "locked".to_owned(),
                    )],
                    affected_directories: vec![PathBuf::from(r"C:AgentScenarios")],
                },
            );
        }
        AgentScenario::DragDropFoundation => unreachable!(),
        AgentScenario::PermissionDenied => unreachable!(),
    }
    manager.task(id).map(|task| match task.state {
        OperationState::Running => "running",
        OperationState::WaitingConflict => "waiting_conflict",
        OperationState::PartiallyCompleted => "partially_completed",
        _ => "unexpected",
    })
}
pub struct PageProjection {
    pub index: i32,
    name: &'static str,
    pub visible_page_operations: &'static [PageOperation],
    error_type: Option<&'static str>,
}

pub fn page_projection(state: LoadState, entries_empty: bool) -> PageProjection {
    let (index, name, error_type) = match state {
        LoadState::Idle => (0, "idle", None),
        LoadState::Loading => (1, "loading", None),
        LoadState::Partial => (2, "partial", None),
        LoadState::Complete if entries_empty => (3, "empty", None),
        LoadState::Complete => (4, "complete", None),
        LoadState::Cancelled => (5, "cancelled", Some("cancelled")),
        LoadState::NotFound => (6, "not_found", Some("not_found")),
        LoadState::PermissionDenied => (7, "permission_denied", Some("permission_denied")),
        LoadState::Disconnected => (8, "disconnected", Some("disconnected")),
        LoadState::Failed => (9, "failed", Some("io_error")),
    };
    let visible_page_operations = if state == LoadState::PermissionDenied {
        &[PageOperation::RequestWindowsAccess][..]
    } else {
        &[]
    };
    PageProjection {
        index,
        name,
        visible_page_operations,
        error_type,
    }
}

fn escape_json(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            character if character.is_control() => {
                use std::fmt::Write;
                write!(escaped, "\\u{:04x}", character as u32)
                    .expect("writing to String cannot fail");
            }
            character => escaped.push(character),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{TabId, TabSession};

    #[test]
    fn permission_page_only_exposes_windows_access_inside_the_page() {
        let mut session = TabSession::new(TabId(1));
        apply_scenario(&mut session, AgentScenario::PermissionDenied);

        let state = AgentState::from_session(&session, AgentScenario::PermissionDenied);

        assert_eq!(
            state.visible_page_operations,
            vec!["request_windows_access"]
        );
        assert!(!state.visible_page_operations.contains(&"retry"));
        assert!(!state.visible_page_operations.contains(&"back"));
        assert_eq!(state.error_type, Some("permission_denied"));
    }

    #[test]
    fn rendered_permission_page_has_exactly_one_inline_action() {
        use crate::app::AppWindow;
        use i_slint_backend_testing::{AccessibleRole, ElementHandle, ElementRoot};

        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().expect("test UI can be created without a desktop window");
        ui.set_page_state(7);
        ui.set_can_navigate_back(true);
        ui.set_can_refresh(true);
        ui.set_text_request_access("Request access with Windows".into());
        ui.set_show_request_access(
            page_projection(LoadState::PermissionDenied, true)
                .visible_page_operations
                .contains(&PageOperation::RequestWindowsAccess),
        );

        let action_area = ui
            .root_element()
            .query_descendants()
            .match_predicate(|element: &ElementHandle| {
                element.accessible_id().as_deref() == Some("error-page-actions")
            })
            .find_first()
            .expect("error page action area is visible");
        let buttons = action_area
            .query_descendants()
            .match_accessible_role(AccessibleRole::Button)
            .find_all();

        assert_eq!(buttons.len(), 1);
        assert_eq!(
            buttons[0].accessible_label().as_deref(),
            Some("Request access with Windows")
        );
    }

    #[test]
    fn exported_drag_drop_foundation_state_is_agent_readable() {
        let mut session = TabSession::new(TabId(1));
        apply_scenario(&mut session, AgentScenario::DragDropFoundation);

        let json = AgentState::from_session(&session, AgentScenario::DragDropFoundation).to_json();

        assert!(json.contains("\"scenario\": \"drag-drop-foundation\""));
        assert!(json.contains("\"lifecycle\":\"unregistered\""));
        assert!(json.contains("\"registered\":false"));
        assert!(json.contains("\"rejection_reason\":\"not_registered\""));
    }

    #[test]
    fn exported_permission_state_is_valid_json_shape() {
        let mut session = TabSession::new(TabId(1));
        apply_scenario(&mut session, AgentScenario::PermissionDenied);

        let json = AgentState::from_session(&session, AgentScenario::PermissionDenied).to_json();

        assert!(json.contains("\"page_state\": \"permission_denied\""));
        assert!(json.contains("\"visible_page_operations\": [\"request_windows_access\"]"));
        assert!(!json.contains(r#""retry""#));
        assert!(!json.contains(r#""back""#));
    }
}
