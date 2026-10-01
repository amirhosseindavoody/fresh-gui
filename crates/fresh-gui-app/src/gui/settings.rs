//! Native settings presentation; daemon settings are edited exclusively through ADE.
use super::client_config;
use fresh_gui_protocol::settings::{
    SettingEffect, SettingOwner, SettingScope, SettingValueType, catalog, read_jsonc,
    validate_value,
};
use fresh_gui_protocol::{HelloUi, Message};
use gpui_kit::component::{
    ActiveTheme,
    button::{Button, ButtonVariants},
    h_flex,
    input::{Input, InputEvent, InputState, Textarea, TextareaState},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde_json::Value;

#[derive(Clone)]
pub enum SettingsEvent {
    Wire(Box<Message>),
    OpenJson(String),
    LocalApplied,
    Close,
}
#[derive(Clone, Copy, PartialEq)]
enum Scope {
    Client,
    User,
    Workspace,
}

pub struct SettingsEditor {
    search: Entity<InputState>,
    value: Entity<InputState>,
    language: Entity<InputState>,
    action: Entity<InputState>,
    context: Entity<InputState>,
    focus: FocusHandle,
    capture: bool,
    scope: Scope,
    workspace: Option<String>,
    keybindings: bool,
    selected: Option<Vec<String>>,
    text: String,
    defaults: Value,
    client_defaults: Value,
    path: String,
    daemon_path: String,
    json: Entity<TextareaState>,
    json_mode: bool,
    status: String,
    supported: bool,
    request: u64,
    pending: Option<String>,
    ready: bool,
    _subscriptions: Vec<Subscription>,
}
impl EventEmitter<SettingsEvent> for SettingsEditor {}
impl SettingsEditor {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Search settings, actions, keys or contexts")
        });
        let subscription = cx.subscribe(&search, |_, _, _: &InputEvent, cx| cx.notify());
        let view = cx.weak_entity();
        let capture_subscription = cx.intercept_keystrokes(move |event, window, cx| {
            let _ = view.update(cx, |this: &mut Self, cx| {
                if this.capture && this.focus.is_focused(window) {
                    this.capture = false;
                    cx.stop_propagation();
                    window.prevent_default();
                    if event.keystroke.key != "escape" {
                        this.value.update(cx, |state, cx| {
                            state.set_value(event.keystroke.unparse(), window, cx)
                        });
                        this.status =
                            "Shortcut captured; Apply validates conflicts before saving".into();
                    }
                    cx.notify();
                }
            });
        });
        Self {
            search,
            value: cx.new(|cx| InputState::new(window, cx).placeholder("JSON value or keystroke")),
            language: cx.new(|cx| {
                InputState::new(window, cx).placeholder("Language id (optional, e.g. rust)")
            }),
            action: cx.new(|cx| InputState::new(window, cx).placeholder("Action name (e.g. GoToFile)")),
            context: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("Context: Editor, Terminal, Explorer; empty = global")
            }),
            focus: cx.focus_handle(),
            capture: false,
            scope: Scope::Client,
            workspace: None,
            keybindings: false,
            selected: None,
            text: "{}".into(),
            defaults: Value::Null,
            client_defaults: serde_json::json!({"ui": serde_json::from_value::<HelloUi>(serde_json::json!({})).expect("UI defaults")}),
            path: String::new(),
            daemon_path: String::new(),
            json: cx.new(|cx| TextareaState::new(window, cx)),
            json_mode: false,
            status: String::new(),
            supported: false,
            request: 0,
            pending: None,
            ready: false,
            _subscriptions: vec![subscription, capture_subscription],
        }
    }
    pub fn open(
        &mut self,
        workspace: Option<String>,
        supported: bool,
        daemon_path: String,
        client_defaults: Value,
        cx: &mut Context<Self>,
    ) {
        self.workspace = workspace;
        self.supported = supported;
        self.daemon_path = daemon_path;
        self.client_defaults = client_defaults;
        self.scope = Scope::Client;
        self.keybindings = false;
        self.selected = None;
        self.load(cx);
    }
    pub fn deactivate(&mut self, cx: &mut Context<Self>) {
        self.capture = false;
        self.pending = None;
        self.selected = None;
        cx.notify();
    }

    fn id(&mut self) -> String {
        self.request += 1;
        format!("settings-{}", self.request)
    }
    fn workspace_id(&self) -> Option<String> {
        if self.scope == Scope::Workspace {
            self.workspace.clone()
        } else {
            None
        }
    }
    fn load(&mut self, cx: &mut Context<Self>) {
        self.json_mode = false;
        self.capture = false;
        self.selected = None;
        self.ready = false;
        self.pending = None;
        if self.scope == Scope::Client {
            match client_config::read_document() {
                Ok(text) => {
                    self.text = text;
                    self.path = client_config::client_config_path().display().to_string();
                    self.defaults = self.client_defaults.clone();
                    self.ready = true;
                    self.status = "Local client presentation · changes apply live".into();
                }
                Err(err) => self.status = err.to_string(),
            }
        } else if self.supported && (self.scope != Scope::Workspace || self.workspace.is_some()) {
            let request_id = self.id();
            self.pending = Some(request_id.clone());
            self.status = "Loading daemon settings…".into();
            cx.emit(SettingsEvent::Wire(Box::new(Message::SettingsRead {
                request_id,
                workspace_id: self.workspace_id(),
            })));
        } else {
            self.path = if self.scope == Scope::Workspace {
                String::new()
            } else {
                self.daemon_path.clone()
            };
            self.status = "This daemon does not support typed settings, or no workspace is selected. Use JSON settings.".into();
        }
        cx.notify();
    }
    pub fn snapshot(
        &mut self,
        request_id: &str,
        path: String,
        text: String,
        defaults: Value,
        cx: &mut Context<Self>,
    ) {
        if self.pending.as_deref() != Some(request_id) {
            return;
        }
        self.pending = None;
        self.selected = None;
        self.capture = false;
        self.path = path;
        self.text = text;
        self.defaults = defaults;
        self.ready = true;
        self.status = if self.scope == Scope::Workspace {
            "Workspace Fresh layer saved · restart server to apply"
        } else {
            "Daemon settings loaded · wrapping and shortcuts apply live; other engine settings require restart"
        }
        .into();
        cx.notify();
    }
    pub fn failure(&mut self, request_id: &str, message: &str, cx: &mut Context<Self>) {
        if self.pending.as_deref() == Some(request_id) {
            self.pending = None;
            self.ready = false;
            self.status = format!("{message} · Reload settings before retrying");
            cx.notify();
        }
    }
    fn select(&mut self, path: Vec<String>, window: &mut Window, cx: &mut Context<Self>) {
        let doc = read_jsonc(&self.text).unwrap_or_default();
        let target =
            setting_path(&path, &self.language.read(cx).value()).unwrap_or_else(|_| path.clone());
        let value = at(&doc, &target)
            .or_else(|| at(&self.defaults, &target))
            .filter(|v| !v.is_null())
            .or_else(|| at(&doc, &path))
            .or_else(|| at(&self.defaults, &path))
            .cloned()
            .unwrap_or(Value::Null);
        self.value.update(cx, |state, cx| {
            state.set_value(
                if self.keybindings
                    || catalog().iter().any(|definition| {
                        definition
                            .path
                            .iter()
                            .copied()
                            .eq(path.iter().map(String::as_str))
                            && definition.value_type == SettingValueType::String
                    })
                {
                    value.as_str().unwrap_or("").to_string()
                } else {
                    value.to_string()
                },
                window,
                cx,
            );
        });
        if self.keybindings {
            let parent = &path[..path.len() - 1];
            let binding = at(&doc, parent).unwrap_or(&Value::Null);
            self.action.update(cx, |s, cx| {
                s.set_value(binding["action"].as_str().unwrap_or(""), window, cx)
            });
            self.context.update(cx, |s, cx| {
                s.set_value(binding["when"].as_str().unwrap_or(""), window, cx)
            });
        }
        self.selected = Some(path);
        self.status = "Edit the value, then Apply; Reset removes the override".into();
        cx.notify();
    }
    fn apply(&mut self, reset: bool, cx: &mut Context<Self>) {
        if !self.ready || self.pending.is_some() {
            return;
        }
        let Some(mut path) = self.selected.clone() else {
            return;
        };
        let result = (|| -> anyhow::Result<Option<Value>> {
            if reset {
                if !self.keybindings {
                    path = setting_path(&path, &self.language.read(cx).value())?;
                }
                return Ok(None);
            }
            if self.keybindings {
                let key = normalize_key(&self.value.read(cx).value())?;
                let action = self.action.read(cx).value().to_string();
                anyhow::ensure!(
                    super::actions::known_action(&action),
                    "Unknown native action; available native actions: {}",
                    super::actions::command_ids().into_iter().map(|(id, _)| id).collect::<Vec<_>>().join(", ")
                );
                let context = self.context.read(cx).value().to_string();
                anyhow::ensure!(
                    ["", "Editor", "Explorer", "Terminal"].contains(&context.as_str()),
                    "Unsupported focus context"
                );
                let doc = read_jsonc(&self.text)?;
                let index: usize = path[1].parse()?;
                if let Some(bindings) = doc["shortkeys"].as_array() {
                    for (i, binding) in bindings.iter().enumerate() {
                        if i != index
                            && keys_overlap(binding["shortkey"].as_str().unwrap_or(""), &key)
                            && contexts_overlap(binding["when"].as_str().unwrap_or(""), &context)
                        {
                            anyhow::bail!(
                                "Conflict with {} ({})",
                                binding["action"],
                                binding["when"]
                            );
                        }
                    }
                }
                path.pop();
                let mut binding = at(&doc, &path)
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({}));
                let object = binding
                    .as_object_mut()
                    .ok_or_else(|| anyhow::anyhow!("Binding must be an object"))?;
                object.insert("action".into(), Value::String(action));
                object.insert(
                    "when".into(),
                    if context.is_empty() {
                        Value::Null
                    } else {
                        Value::String(context)
                    },
                );
                object.insert("shortkey".into(), Value::String(key));
                return Ok(Some(binding));
            }
            let value = parse_setting_value(&path, &self.value.read(cx).value())?;
            path = setting_path(&path, &self.language.read(cx).value())?;
            Ok(Some(value))
        })();
        match result {
            Ok(value) => {
                if reset && self.keybindings {
                    path.pop();
                }
                if self.scope == Scope::Client {
                    match client_config::patch_document(&self.text, &path, value.as_ref()) {
                        Ok(text) => {
                            self.text = text;
                            self.selected = None;
                            self.status = "Applied live to this client".into();
                            cx.emit(SettingsEvent::LocalApplied);
                        }
                        Err(err) => self.status = err.to_string(),
                    }
                } else {
                    let request_id = self.id();
                    self.pending = Some(request_id.clone());
                    self.ready = false;
                    cx.emit(SettingsEvent::Wire(Box::new(Message::SettingsPatch {
                        request_id,
                        workspace_id: self.workspace_id(),
                        base_text: self.text.clone(),
                        path,
                        value,
                    })));
                    self.status = "Saving settings…".into();
                }
            }
            Err(err) => self.status = err.to_string(),
        }
        cx.notify();
    }
}
fn at<'a>(value: &'a Value, path: &[String]) -> Option<&'a Value> {
    path.iter().try_fold(value, |v, key| {
        if v.is_array() {
            v.get(key.parse::<usize>().ok()?)
        } else {
            v.get(key)
        }
    })
}
fn normalize_key(key: &str) -> anyhow::Result<String> {
    anyhow::ensure!(!key.trim().is_empty(), "Capture or enter a shortcut");
    key.split_whitespace()
        .map(|part| {
            Keystroke::parse(part)
                .map(|key| key.unparse())
                .map_err(anyhow::Error::from)
        })
        .collect::<anyhow::Result<Vec<_>>>()
        .map(|keys| keys.join(" "))
}
fn parse_setting_value(path: &[String], text: &str) -> anyhow::Result<Value> {
    let definition = catalog().into_iter().find(|definition| {
        definition
            .path
            .iter()
            .copied()
            .eq(path.iter().map(String::as_str))
    });
    let value = if definition
        .as_ref()
        .is_some_and(|definition| definition.value_type == SettingValueType::String)
    {
        Value::String(text.to_owned())
    } else {
        serde_json::from_str(text)?
    };
    if let Some(definition) = definition {
        validate_value(&definition, &value)?;
    }
    Ok(value)
}

fn setting_path(path: &[String], language: &str) -> anyhow::Result<Vec<String>> {
    if language.is_empty() || path.first().map(String::as_str) != Some("editor") {
        return Ok(path.to_vec());
    }
    let key = path
        .get(1)
        .ok_or_else(|| anyhow::anyhow!("Select an editor setting"))?;
    anyhow::ensure!(
        [
            "tab_size",
            "use_tabs",
            "auto_indent",
            "line_wrap",
            "wrap_column"
        ]
        .contains(&key.as_str()),
        "This setting has no Fresh language override"
    );
    Ok(vec!["languages".into(), language.to_owned(), key.clone()])
}
fn keys_overlap(a: &str, b: &str) -> bool {
    let (Ok(a), Ok(b)) = (normalize_key(a), normalize_key(b)) else {
        return false;
    };
    let a: Vec<_> = a.split_whitespace().collect();
    let b: Vec<_> = b.split_whitespace().collect();
    a.starts_with(&b) || b.starts_with(&a)
}
fn contexts_overlap(a: &str, b: &str) -> bool {
    a.is_empty() || b.is_empty() || a == b
}
impl Render for SettingsEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let selected_definition = self.selected.as_ref().and_then(|path| {
            catalog().into_iter().find(|definition| {
                definition
                    .path
                    .iter()
                    .copied()
                    .eq(path.iter().map(String::as_str))
            })
        });
        let mut value_controls = h_flex().gap_2();
        if !self.keybindings
            && let Some(definition) = selected_definition
        {
            let choices = if definition.value_type == SettingValueType::Boolean {
                vec!["true".to_owned(), "false".to_owned()]
            } else {
                definition.choices
            };
            for choice in choices {
                let value = choice.clone();
                value_controls = value_controls.child(
                    Button::new(format!("setting-choice-{choice}"))
                        .label(choice)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.value
                                .update(cx, |state, cx| state.set_value(value.clone(), window, cx));
                            cx.notify();
                        })),
                );
            }
        }
        let query = self.search.read(cx).value().to_lowercase();
        let doc = read_jsonc(&self.text).unwrap_or_default();
        let mut rows = v_flex().gap_1();
        if self.ready && self.keybindings {
            if let Some(bindings) = doc["shortkeys"].as_array() {
                for (i, binding) in bindings.iter().enumerate() {
                    let label = format!(
                        "{} · {} · {}",
                        binding["action"].as_str().unwrap_or("?"),
                        binding["shortkey"].as_str().unwrap_or("?"),
                        binding["when"].as_str().unwrap_or("Global")
                    );
                    if !label.to_lowercase().contains(&query) {
                        continue;
                    }
                    rows = rows.child(
                        Button::new(format!("binding-{i}"))
                            .ghost()
                            .label(label)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.select(
                                    vec!["shortkeys".into(), i.to_string(), "shortkey".into()],
                                    window,
                                    cx,
                                )
                            })),
                    );
                }
            }
            rows = rows.child(
                Button::new("restore-bindings")
                    .label("Restore default bindings")
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(value) = this.defaults.get("shortkeys").cloned() {
                            let request_id = this.id();
                            this.pending = Some(request_id.clone());
                            this.ready = false;
                            cx.emit(SettingsEvent::Wire(Box::new(Message::SettingsPatch {
                                request_id,
                                workspace_id: None,
                                base_text: this.text.clone(),
                                path: vec!["shortkeys".into()],
                                value: Some(value),
                            })));
                            this.status = "Restoring native default bindings…".into();
                            cx.notify();
                        }
                    })),
            );
            rows = rows.child(Button::new("add-binding").label("Add binding").on_click(
                cx.listener(|this, _, window, cx| {
                    let index = read_jsonc(&this.text)
                        .ok()
                        .and_then(|v| v["shortkeys"].as_array().map(Vec::len))
                        .unwrap_or(0);
                    this.select(
                        vec!["shortkeys".into(), index.to_string(), "shortkey".into()],
                        window,
                        cx,
                    );
                }),
            ));
        } else if self.ready {
            for def in catalog() {
                if self.scope == Scope::Workspace && def.scope == SettingScope::Global {
                    continue;
                }
                if (self.scope == Scope::Client) != (def.owner == SettingOwner::LocalClient) {
                    continue;
                }
                let path: Vec<String> = def.path.iter().map(|s| s.to_string()).collect();
                let name = def.path.join(".");
                if !name.to_lowercase().contains(&query) {
                    continue;
                }
                let default = at(&self.defaults, &path)
                    .cloned()
                    .or(def.default)
                    .unwrap_or(Value::Null);

                let value = at(&doc, &path).unwrap_or(&default);
                let effect = if self.scope == Scope::Workspace {
                    "restart server in this workspace"
                } else {
                    match def.effect {
                        SettingEffect::Immediate => "applies live",
                        SettingEffect::NextTerminal => "new terminals",
                        SettingEffect::BackendOnly => {
                            "Fresh view only; native view mapping pending"
                        }
                        _ => "restart server",
                    }
                };
                let label = format!("{name} = {value} · inherited/default {default} · {effect}");
                rows = rows.child(Button::new(name).ghost().label(label).on_click(
                    cx.listener(move |this, _, window, cx| this.select(path.clone(), window, cx)),
                ));
            }
        }
        div()
            .absolute()
            .inset_0()
            .p_4()
            .bg(cx.theme().background.opacity(0.7))
            .flex()
            .justify_center()
            .items_center()
            .child(
                v_flex()
                    .id("settings-editor")
                    .track_focus(&self.focus)
                    .w_full()
                    .max_w(px(850.))
                    .h_full()
                    .max_h(px(680.))
                    .p_4()
                    .gap_3()
                    .bg(cx.theme().background)
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded_lg()
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                        if this.capture {
                            this.capture = false;
                            cx.stop_propagation();
                            window.prevent_default();
                            if event.keystroke.key != "escape" {
                                this.value.update(cx, |s, cx| {
                                    s.set_value(event.keystroke.unparse(), window, cx)
                                });
                            }
                            cx.notify();
                        }
                    }))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().flex_1().child("Settings and keybindings"))
                            .child(Button::new("settings-close").label("Close").on_click(
                                cx.listener(|_, _, _, cx| cx.emit(SettingsEvent::Close)),
                            )),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("client-settings")
                                    .label("Local client")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.scope = Scope::Client;
                                        this.keybindings = false;
                                        this.load(cx);
                                    })),
                            )
                            .child(
                                Button::new("daemon-settings")
                                    .label("Daemon user")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.scope = Scope::User;
                                        this.keybindings = false;
                                        this.load(cx);
                                    })),
                            )
                            .child(
                                Button::new("workspace-settings")
                                    .label("Workspace")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.scope = Scope::Workspace;
                                        this.keybindings = false;
                                        this.load(cx);
                                    })),
                            )
                            .child(Button::new("keybindings").label("Keybindings").on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.scope = Scope::User;
                                    this.keybindings = true;
                                    this.load(cx);
                                }),
                            ))
                            .child(
                                Button::new("settings-reload")
                                    .label("Reload")
                                    .on_click(cx.listener(|this, _, _, cx| this.load(cx))),
                            )
                            .child(Button::new("settings-json").label("Edit JSON").on_click(
                                cx.listener(|this, _, window, cx| {
                                    if this.scope == Scope::Client {
                                        this.json_mode = !this.json_mode;
                                        this.status = "Local JSON is saved with Save local JSON; Close discards unsaved changes".into();
                                        this.json.update(cx, |s, cx| {
                                            s.set_value(this.text.clone(), window, cx)
                                        });
                                        cx.notify();
                                    } else if !this.path.is_empty() {
                                        cx.emit(SettingsEvent::OpenJson(this.path.clone()));
                                    }
                                }),
                            )),
                    )
                    .child(div().text_xs().child(format!(
                        "{} · {}",
                        match self.scope {
                            Scope::Client => "Local client",
                            Scope::User => "Daemon user",
                            Scope::Workspace => "Workspace",
                        },
                        self.path
                    )))
                    .when(self.json_mode, |view| {
                        view.child(Textarea::new(&self.json).h(px(300.))).child(
                            Button::new("save-client-json")
                                .label("Save local JSON")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    match client_config::write_document(
                                        &this.text,
                                        &this.json.read(cx).value(),
                                    ) {
                                        Ok(text) => {
                                            this.text = text;
                                            this.status =
                                                "Local JSON saved and applied live".into();
                                            cx.emit(SettingsEvent::LocalApplied);
                                        }
                                        Err(error) => this.status = error.to_string(),
                                    }
                                    cx.notify();
                                })),
                        )
                    })
                    .when(!self.json_mode, |view| {
                        view.child(Input::new(&self.search)).child(
                            div()
                                .id("settings-list")
                                .flex_1()
                                .min_h_0()
                                .overflow_y_scroll()
                                .child(rows),
                        )
                    })
                    .when(self.selected.is_some() && !self.json_mode, |view| {
                        view.child(
                            v_flex()
                                .gap_2()
                                .child(Input::new(&self.value))
                                .child(value_controls)
                                .when(self.keybindings, |view| {
                                    view.child(Input::new(&self.action))
                                        .child(Input::new(&self.context))
                                        .child(
                                            Button::new("capture-binding")
                                                .label(if self.capture {
                                                    "Press shortcut (Escape cancels)"
                                                } else {
                                                    "Capture shortcut"
                                                })
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.capture = true;
                                                    this.focus.focus(window, cx);
                                                    cx.notify();
                                                })),
                                        )
                                })
                                .when(!self.keybindings && self.scope != Scope::Client, |view| {
                                    view.child(Input::new(&self.language))
                                })
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .child(
                                            Button::new("apply-setting").label("Apply").on_click(
                                                cx.listener(|this, _, _, cx| this.apply(false, cx)),
                                            ),
                                        )
                                        .child(
                                            Button::new("reset-setting")
                                                .label(if self.keybindings {
                                                    "Remove binding"
                                                } else {
                                                    "Reset override"
                                                })
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.apply(true, cx)
                                                })),
                                        ),
                                ),
                        )
                    })
                    .child(div().text_sm().child(self.status.clone())),
            )
    }
}
#[cfg(test)]
mod tests {
    use super::{
        SettingsEditor, contexts_overlap, keys_overlap, normalize_key, parse_setting_value,
        setting_path,
    };
    use crate::gui::actions::SaveBuffer;
    use gpui::{
        Context, Entity, InteractiveElement, ParentElement, Render, Styled, TestAppContext, Window,
    };
    use gpui_kit::{AppContext, KeyBinding, div};
    #[test]
    fn conflicts_respect_focus_and_chords() {
        assert!(contexts_overlap("", "Editor"));
        assert!(!contexts_overlap("Editor", "Terminal"));
        assert_eq!(normalize_key("ctrl-k ctrl-s").unwrap(), "ctrl-k ctrl-s");
        assert!(normalize_key("").is_err());
        assert!(keys_overlap("ctrl-k", "ctrl-k ctrl-s"));
        assert!(!keys_overlap("ctrl-k ctrl-x", "ctrl-k ctrl-s"));
        assert!(!keys_overlap("", "ctrl-k"));
    }

    #[test]
    fn typed_controls_parse_plain_strings_and_validate_numeric_values() {
        let theme = vec!["ui".into(), "theme".into()];
        assert_eq!(
            parse_setting_value(&theme, "dark").unwrap(),
            serde_json::json!("dark")
        );
        assert!(parse_setting_value(&theme, "unknown-theme").is_err());
        let font = vec!["ui".into(), "editorFontSize".into()];
        assert_eq!(
            parse_setting_value(&font, "18").unwrap(),
            serde_json::json!(18)
        );
        assert!(parse_setting_value(&font, "1000").is_err());
        assert!(parse_setting_value(&font, "abc").is_err());
        assert_eq!(
            parse_setting_value(&["editor".into(), "use_tabs".into()], "false").unwrap(),
            serde_json::json!(false)
        );
    }

    #[test]
    fn language_override_and_reset_target_the_same_fresh_key() {
        let path = vec!["editor".to_string(), "tab_size".to_string()];
        assert_eq!(
            setting_path(&path, "rust").unwrap(),
            vec!["languages", "rust", "tab_size"]
        );
        assert_eq!(setting_path(&path, "").unwrap(), path);
        assert!(setting_path(&["editor".into(), "line_numbers".into()], "rust").is_err());
    }

    #[gpui::test]
    fn local_reset_displays_the_inherited_daemon_default(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (editor, cx) = cx.add_window_view(SettingsEditor::new);
        editor.update_in(cx, |editor, window, cx| {
            editor.text = "{}".into();
            editor.defaults = serde_json::json!({"ui":{"theme":"dark","editorFontSize":18}});
            editor.select(vec!["ui".into(), "theme".into()], window, cx);
            assert_eq!(editor.value.read(cx).value(), "dark");
            editor.select(vec!["ui".into(), "editorFontSize".into()], window, cx);
            assert_eq!(editor.value.read(cx).value(), "18");
        });
    }

    #[gpui::test]
    fn settings_ignores_stale_snapshots_and_failures(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (editor, cx) = cx.add_window_view(SettingsEditor::new);
        editor.update_in(cx, |editor, _, cx| {
            editor.pending = Some("settings-2".into());
            editor.selected = Some(vec!["shortkeys".into(), "0".into(), "shortkey".into()]);
            editor.snapshot(
                "settings-1",
                "/old/config.json".into(),
                "{}".into(),
                serde_json::json!({}),
                cx,
            );
            assert_eq!(editor.pending.as_deref(), Some("settings-2"));
            assert!(!editor.ready);
            editor.failure("settings-1", "stale failure", cx);
            assert_eq!(editor.pending.as_deref(), Some("settings-2"));
            editor.snapshot(
                "settings-2",
                "/current/config.json".into(),
                "{}".into(),
                serde_json::json!({}),
                cx,
            );
            assert!(editor.ready);
            assert!(editor.pending.is_none());
            assert!(
                editor.selected.is_none(),
                "A save/removal must not retain an index pointing to the next binding"
            );
        });
    }

    struct CaptureHost {
        editor: Entity<SettingsEditor>,
        save_actions: usize,
    }
    impl Render for CaptureHost {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
            div()
                .size_full()
                .on_action(cx.listener(|this, _: &SaveBuffer, _, _| this.save_actions += 1))
                .child(self.editor.clone())
        }
    }

    #[gpui::test]
    fn shortcut_capture_consumes_existing_action_binding(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (host, cx) = cx.add_window_view(|window, cx| CaptureHost {
            editor: cx.new(|cx| SettingsEditor::new(window, cx)),
            save_actions: 0,
        });
        cx.cx
            .update(|app| app.bind_keys([KeyBinding::new("ctrl-s", SaveBuffer, None)]));
        host.update_in(cx, |host, window, cx| {
            host.editor.update(cx, |editor, cx| {
                editor.capture = true;
                editor.focus.focus(window, cx);
            });
        });
        cx.simulate_keystrokes("ctrl-s");
        host.read_with(cx, |host, _| assert_eq!(host.save_actions, 0));
        host.update_in(cx, |host, _, cx| {
            assert!(!host.editor.read(cx).capture);
            assert_eq!(host.editor.read(cx).value.read(cx).value(), "ctrl-s");
        });
        host.update_in(cx, |host, _, cx| {
            host.editor.update(cx, |editor, cx| {
                editor.capture = true;
                editor.deactivate(cx);
            });
        });
        cx.simulate_keystrokes("ctrl-s");
        host.read_with(cx, |host, _| assert_eq!(host.save_actions, 1));
    }
}
