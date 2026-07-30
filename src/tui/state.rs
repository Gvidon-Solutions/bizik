//! Pure dashboard state transitions.
//!
//! Keyboard input is reduced into state changes plus an [`Intent`]. The reducer
//! never invokes tmux, ssh, the filesystem, or worker threads; `App` executes
//! the returned intent at the infrastructure boundary.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::ListState;
use std::collections::HashSet;
use std::time::Instant;
use uuid::Uuid;

use super::rows::Row;

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Screen {
    Folders,
    Folder {
        host: String,
        folder: Uuid,
        name: String,
    },
    Running,
    Layouts,
    Hosts,
}

impl Screen {
    pub fn title(&self) -> String {
        match self {
            Self::Folders => "folders".into(),
            Self::Folder { name, .. } => name.clone(),
            Self::Running => "running".into(),
            Self::Layouts => "layouts".into(),
            Self::Hosts => "hosts".into(),
        }
    }

    /// Screens reachable with Tab. A folder's detail is not among them: it is
    /// reached by opening a folder and left with Esc.
    pub(crate) const TABS: [Self; 4] = [Self::Folders, Self::Running, Self::Layouts, Self::Hosts];

    pub(crate) fn tab_index(&self) -> Option<usize> {
        Self::TABS.iter().position(|tab| tab == self)
    }
}

pub enum Overlay {
    Help,
    Confirm {
        prompt: String,
        action: Confirm,
    },
    Input {
        prompt: String,
        value: String,
        kind: InputKind,
    },
}

pub enum Confirm {
    CloseViewer,
    Stop { host: String, session: Uuid },
    Forget { host: String, session: Uuid },
    Unmark { host: String, path: String },
    DeleteLayout { id: Uuid },
}

pub enum InputKind {
    SaveLayout,
    Relabel { host: String, path: String },
}

#[derive(Clone, Copy, PartialEq)]
pub enum ToastKind {
    Info,
    Error,
}

pub enum Intent {
    Activate { background: bool },
    AskDelete,
    AskStop,
    BeginRelabel,
    Detach,
    FocusWork,
    InstallHost,
    Refresh,
    ToggleHiddenProjects,
    ToggleProjectHidden,
    ToggleProjectPinned,
    RunConfirmed(Confirm),
    RunInput(InputKind, String),
}

pub struct State {
    pub stack: Vec<Screen>,
    pub overlay: Option<Overlay>,
    pub list: ListState,
    pub rows: Vec<Row>,
    pub filter: String,
    pub filtering: bool,
    pub selected: HashSet<(String, Uuid)>,
    pub toast: Option<(String, ToastKind, Instant)>,
    pub refreshing: bool,
    pub starting: usize,
    pub show_hidden_projects: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            stack: vec![Screen::Folders],
            overlay: None,
            list: ListState::default(),
            rows: Vec::new(),
            filter: String::new(),
            filtering: false,
            selected: HashSet::new(),
            toast: None,
            refreshing: false,
            starting: 0,
            show_hidden_projects: false,
        }
    }
}

impl State {
    pub fn screen(&self) -> &Screen {
        self.stack.last().unwrap_or(&Screen::Folders)
    }

    pub fn current(&self) -> Option<Row> {
        self.list
            .selected()
            .and_then(|index| self.rows.get(index).cloned())
    }

    pub fn info(&mut self, message: impl Into<String>) {
        self.toast = Some((message.into(), ToastKind::Info, Instant::now()));
    }

    pub fn error(&mut self, message: impl Into<String>) {
        self.toast = Some((message.into(), ToastKind::Error, Instant::now()));
    }

    pub fn reduce_key(&mut self, key: KeyEvent) -> Option<Intent> {
        if self.overlay.is_some() {
            return self.reduce_overlay_key(key);
        }
        if self.filtering {
            self.reduce_filter_key(key);
            return None;
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.go_back().then_some(Intent::Detach),
            KeyCode::Char('q') => Some(Intent::Detach),
            KeyCode::Char('c') if ctrl => Some(Intent::Detach),
            KeyCode::Char('Q') => {
                self.overlay = Some(Overlay::Confirm {
                    prompt:
                        "close the panes and this dashboard? sessions keep running on their hosts"
                            .into(),
                    action: Confirm::CloseViewer,
                });
                None
            }
            KeyCode::Char('?') => {
                self.overlay = Some(Overlay::Help);
                None
            }
            KeyCode::Char('r') => Some(Intent::Refresh),
            KeyCode::Char('/') => {
                self.filtering = true;
                None
            }
            KeyCode::Tab => {
                self.cycle_tab(1);
                None
            }
            KeyCode::BackTab => {
                self.cycle_tab(-1);
                None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_cursor(1);
                None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_cursor(-1);
                None
            }
            KeyCode::PageDown => {
                self.move_cursor(10);
                None
            }
            KeyCode::PageUp => {
                self.move_cursor(-10);
                None
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.jump(true);
                None
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.jump(false);
                None
            }
            KeyCode::Char(' ') => {
                self.toggle_selection();
                None
            }
            KeyCode::Enter => Some(Intent::Activate { background: false }),
            KeyCode::Char('b') => Some(Intent::Activate { background: true }),
            KeyCode::Char('w') => Some(Intent::FocusWork),
            KeyCode::Char('S') => {
                self.overlay = Some(Overlay::Input {
                    prompt: "name for this layout".into(),
                    value: String::new(),
                    kind: InputKind::SaveLayout,
                });
                None
            }
            KeyCode::Char('e') => Some(Intent::BeginRelabel),
            KeyCode::Char('x') => Some(Intent::AskStop),
            KeyCode::Char('d') | KeyCode::Delete => Some(Intent::AskDelete),
            KeyCode::Char('p') => Some(Intent::ToggleProjectPinned),
            KeyCode::Char('H') => Some(Intent::ToggleProjectHidden),
            KeyCode::Char('v') => Some(Intent::ToggleHiddenProjects),
            KeyCode::Char('i') => Some(Intent::InstallHost),
            _ => None,
        }
    }

    /// Returns true only when Esc at the root requests the detach effect.
    fn go_back(&mut self) -> bool {
        if self.filtering || !self.filter.is_empty() {
            self.filter.clear();
            self.filtering = false;
            return false;
        }
        if self.stack.len() > 1 {
            self.stack.pop();
            self.list.select(None);
            self.filter.clear();
            return false;
        }
        true
    }

    fn reduce_filter_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.filter.clear();
                self.filtering = false;
            }
            KeyCode::Enter => self.filtering = false,
            KeyCode::Backspace => {
                self.filter.pop();
            }
            KeyCode::Char(character) => self.filter.push(character),
            KeyCode::Down => self.move_cursor(1),
            KeyCode::Up => self.move_cursor(-1),
            _ => {}
        }
    }

    fn reduce_overlay_key(&mut self, key: KeyEvent) -> Option<Intent> {
        match self.overlay.take() {
            Some(Overlay::Help) => None,
            Some(Overlay::Confirm { prompt, action }) => match key.code {
                KeyCode::Char('y') | KeyCode::Enter => Some(Intent::RunConfirmed(action)),
                KeyCode::Esc | KeyCode::Char('n') => None,
                _ => {
                    self.overlay = Some(Overlay::Confirm { prompt, action });
                    None
                }
            },
            Some(Overlay::Input {
                prompt,
                mut value,
                kind,
            }) => match key.code {
                KeyCode::Esc => None,
                KeyCode::Enter => Some(Intent::RunInput(kind, value.trim().to_string())),
                KeyCode::Backspace => {
                    value.pop();
                    self.overlay = Some(Overlay::Input {
                        prompt,
                        value,
                        kind,
                    });
                    None
                }
                KeyCode::Char(character) => {
                    value.push(character);
                    self.overlay = Some(Overlay::Input {
                        prompt,
                        value,
                        kind,
                    });
                    None
                }
                _ => {
                    self.overlay = Some(Overlay::Input {
                        prompt,
                        value,
                        kind,
                    });
                    None
                }
            },
            None => None,
        }
    }

    fn cycle_tab(&mut self, delta: isize) {
        let current = self.screen().tab_index().unwrap_or(0);
        let next = if delta.is_negative() {
            current.checked_sub(1).unwrap_or(Screen::TABS.len() - 1)
        } else {
            (current + 1) % Screen::TABS.len()
        };
        self.stack = vec![Screen::TABS[next].clone()];
        self.list.select(None);
        self.filter.clear();
    }

    fn move_cursor(&mut self, delta: isize) {
        let selectable: Vec<usize> = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.selectable())
            .map(|(index, _)| index)
            .collect();
        if selectable.is_empty() {
            return;
        }
        let at = self
            .list
            .selected()
            .and_then(|selected| selectable.iter().position(|index| *index == selected))
            .unwrap_or(0);
        let next = at.saturating_add_signed(delta).min(selectable.len() - 1);
        self.list.select(Some(selectable[next]));
    }

    fn jump(&mut self, top: bool) {
        let mut selectable = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.selectable());
        let target = if top {
            selectable.next()
        } else {
            selectable.next_back()
        };
        if let Some((index, _)) = target {
            self.list.select(Some(index));
        }
    }

    fn toggle_selection(&mut self) {
        let Some(key) = self.current().and_then(|row| row.select_key()) else {
            self.info("only sessions can be selected");
            return;
        };
        if !self.selected.remove(&key) {
            self.selected.insert(key);
        }
        self.move_cursor(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AgentKind, Session};
    use crate::reconcile::{SessionView, State as SessionState};
    use ratatui::crossterm::event::KeyEvent;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::from(code)
    }

    fn session_row(host: &str) -> Row {
        Row::Session {
            host: host.into(),
            view: SessionView {
                session: Session::new(Uuid::new_v4(), AgentKind::Claude, "work".into()),
                state: SessionState::Working,
                preview: None,
                attention: None,
            },
        }
    }

    #[test]
    fn escape_reduces_one_navigation_layer_at_a_time() {
        let mut state = State {
            stack: vec![
                Screen::Folders,
                Screen::Folder {
                    host: "server".into(),
                    folder: Uuid::new_v4(),
                    name: "repo".into(),
                },
            ],
            filter: "query".into(),
            filtering: true,
            ..State::default()
        };

        assert!(state.reduce_key(key(KeyCode::Esc)).is_none());
        assert!(state.filter.is_empty());
        assert_eq!(state.stack.len(), 2);

        assert!(state.reduce_key(key(KeyCode::Esc)).is_none());
        assert_eq!(state.stack, vec![Screen::Folders]);

        assert!(matches!(
            state.reduce_key(key(KeyCode::Esc)),
            Some(Intent::Detach)
        ));
    }

    #[test]
    fn tab_navigation_wraps_without_effects() {
        let mut state = State::default();
        assert!(state.reduce_key(key(KeyCode::BackTab)).is_none());
        assert_eq!(state.screen(), &Screen::Hosts);
        assert!(state.reduce_key(key(KeyCode::Tab)).is_none());
        assert_eq!(state.screen(), &Screen::Folders);
    }

    #[test]
    fn confirmation_is_state_until_accepted_then_becomes_an_intent() {
        let mut state = State::default();
        state.reduce_key(key(KeyCode::Char('Q')));
        assert!(matches!(state.overlay, Some(Overlay::Confirm { .. })));

        let intent = state.reduce_key(key(KeyCode::Enter));
        assert!(matches!(
            intent,
            Some(Intent::RunConfirmed(Confirm::CloseViewer))
        ));
        assert!(state.overlay.is_none());
    }

    #[test]
    fn filter_editing_never_emits_infrastructure_work() {
        let mut state = State::default();
        state.reduce_key(key(KeyCode::Char('/')));
        state.reduce_key(key(KeyCode::Char('a')));
        state.reduce_key(key(KeyCode::Char('b')));
        assert_eq!(state.filter, "ab");
        assert!(state.filtering);

        assert!(state.reduce_key(key(KeyCode::Enter)).is_none());
        assert!(!state.filtering);
    }

    #[test]
    fn cursor_navigation_skips_notes_and_clamps_at_the_edges() {
        let mut state = State {
            rows: vec![
                Row::Note("top".into()),
                session_row("one"),
                Row::Note("middle".into()),
                session_row("two"),
            ],
            ..State::default()
        };
        state.list.select(Some(1));

        state.reduce_key(key(KeyCode::Down));
        assert_eq!(state.list.selected(), Some(3));
        state.reduce_key(key(KeyCode::Down));
        assert_eq!(state.list.selected(), Some(3));
        state.reduce_key(key(KeyCode::Home));
        assert_eq!(state.list.selected(), Some(1));
        state.reduce_key(key(KeyCode::End));
        assert_eq!(state.list.selected(), Some(3));
    }

    #[test]
    fn session_selection_toggles_and_advances() {
        let mut state = State {
            rows: vec![session_row("one"), session_row("two")],
            ..State::default()
        };
        state.list.select(Some(0));
        let first = state.rows[0]
            .select_key()
            .expect("the test row is a session");

        state.reduce_key(key(KeyCode::Char(' ')));
        assert!(state.selected.contains(&first));
        assert_eq!(state.list.selected(), Some(1));

        state.list.select(Some(0));
        state.reduce_key(key(KeyCode::Char(' ')));
        assert!(!state.selected.contains(&first));
    }

    #[test]
    fn non_session_selection_explains_why_it_did_nothing() {
        let mut state = State {
            rows: vec![Row::NewSession {
                host: "local".into(),
                folder: Uuid::new_v4(),
                agent: AgentKind::Shell,
            }],
            ..State::default()
        };
        state.list.select(Some(0));

        state.reduce_key(key(KeyCode::Char(' ')));
        assert!(state.selected.is_empty());
        assert!(matches!(
            state.toast,
            Some((ref message, ToastKind::Info, _)) if message.contains("only sessions")
        ));
    }

    #[test]
    fn input_overlay_edits_cancels_and_emits_trimmed_value() {
        let mut state = State {
            overlay: Some(Overlay::Input {
                prompt: "name".into(),
                value: " x".into(),
                kind: InputKind::SaveLayout,
            }),
            ..State::default()
        };
        state.reduce_key(key(KeyCode::Char('y')));
        state.reduce_key(key(KeyCode::Backspace));
        let intent = state.reduce_key(key(KeyCode::Enter));
        assert!(matches!(
            intent,
            Some(Intent::RunInput(InputKind::SaveLayout, ref value)) if value == "x"
        ));
        assert!(state.overlay.is_none());

        state.overlay = Some(Overlay::Help);
        assert!(state.reduce_key(key(KeyCode::Char('z'))).is_none());
        assert!(state.overlay.is_none(), "any key closes help");
    }

    #[test]
    fn every_project_management_shortcut_emits_its_intent() {
        let mut state = State::default();
        assert!(matches!(
            state.reduce_key(key(KeyCode::Char('p'))),
            Some(Intent::ToggleProjectPinned)
        ));
        assert!(matches!(
            state.reduce_key(key(KeyCode::Char('H'))),
            Some(Intent::ToggleProjectHidden)
        ));
        assert!(matches!(
            state.reduce_key(key(KeyCode::Char('v'))),
            Some(Intent::ToggleHiddenProjects)
        ));
        assert!(matches!(
            state.reduce_key(key(KeyCode::Delete)),
            Some(Intent::AskDelete)
        ));
    }
}
