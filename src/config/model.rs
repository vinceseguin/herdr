use std::{collections::BTreeSet, num::NonZeroUsize};

use crossterm::event::KeyModifiers;
use serde::{de, Deserialize, Deserializer, Serialize};

use super::{
    ActionKeybinds, BindingConfig, CommandKeybindConfig, IndexedKeybind, Keybinds, SidebarConfig,
    SoundConfig, TabBarRightEntryConfig, ThemeConfig, DEFAULT_MOBILE_WIDTH_THRESHOLD,
    DEFAULT_MOUSE_SCROLL_LINES, DEFAULT_SCROLLBACK_LIMIT_BYTES,
};

pub const MAX_TOAST_DELAY_SECONDS: u64 = 3600;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum UpdateChannelConfig {
    #[default]
    Stable,
    Preview,
}

impl UpdateChannelConfig {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Preview => "preview",
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct UpdateConfig {
    pub channel: UpdateChannelConfig,
    pub version_check: bool,
    pub manifest_check: bool,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            channel: default_update_channel(),
            version_check: true,
            manifest_check: true,
        }
    }
}

fn default_update_channel() -> UpdateChannelConfig {
    default_update_channel_for_build(cfg!(windows), crate::build_info::is_preview())
}

fn default_update_channel_for_build(is_windows: bool, is_preview: bool) -> UpdateChannelConfig {
    if is_windows && is_preview {
        UpdateChannelConfig::Preview
    } else {
        UpdateChannelConfig::Stable
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ToastDelivery {
    #[default]
    Off,
    Herdr,
    Terminal,
    System,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema, Default,
)]
#[serde(rename_all = "kebab-case")]
pub enum ToastHerdrPosition {
    TopLeft,
    TopRight,
    BottomLeft,
    #[default]
    BottomRight,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ToastClipboardPosition {
    TopLeft,
    TopCenter,
    TopRight,
    BottomLeft,
    #[default]
    BottomCenter,
    BottomRight,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AgentPanelSortConfig {
    #[default]
    #[serde(alias = "workspaces")]
    Spaces,
    Priority,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum LegacyAgentPanelScopeConfig {
    Current,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum StatusIndicatorStyle {
    #[default]
    Dots,
    Symbols,
}

impl StatusIndicatorStyle {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dots => "dots",
            Self::Symbols => "symbols",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HostCursorModeConfig {
    #[default]
    Auto,
    Native,
    Drawn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SidebarCollapsedModeConfig {
    #[default]
    Compact,
    Hidden,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RightClickPassthroughModifierConfig(Option<KeyModifiers>);

impl RightClickPassthroughModifierConfig {
    pub fn modifiers(self) -> Option<KeyModifiers> {
        self.0
    }
}

impl<'de> Deserialize<'de> for RightClickPassthroughModifierConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        parse_right_click_passthrough_modifier(&value)
            .map(Self)
            .ok_or_else(|| {
                de::Error::custom(
                    "right_click_passthrough_modifier must be empty, off, none, disabled, ctrl/control, alt/option, cmd/command/super, meta, hyper, or a + separated combination without shift",
                )
            })
    }
}

fn parse_right_click_passthrough_modifier(value: &str) -> Option<Option<KeyModifiers>> {
    let trimmed = value.trim();
    if trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case("off")
        || trimmed.eq_ignore_ascii_case("none")
        || trimmed.eq_ignore_ascii_case("disabled")
    {
        return Some(None);
    }

    let mut modifiers = KeyModifiers::empty();
    for token in trimmed.split('+') {
        let token = token.trim().to_ascii_lowercase();
        let modifier = match token.as_str() {
            "ctrl" | "control" => KeyModifiers::CONTROL,
            "alt" | "option" => KeyModifiers::ALT,
            "cmd" | "command" | "super" => KeyModifiers::SUPER,
            "meta" => KeyModifiers::META,
            "hyper" => KeyModifiers::HYPER,
            "shift" => return None,
            _ => return None,
        };
        modifiers |= modifier;
    }

    (!modifiers.is_empty()).then_some(Some(modifiers))
}

#[derive(Debug, Clone)]
pub struct ToastConfig {
    pub delivery: ToastDelivery,
    pub delay_seconds: u64,
    pub herdr: HerdrToastConfig,
    pub clipboard: ClipboardToastConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct HerdrToastConfig {
    pub position: ToastHerdrPosition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct ClipboardToastConfig {
    pub enabled: bool,
    pub position: ToastClipboardPosition,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum NewTerminalCwdConfig {
    #[default]
    Follow,
    Home,
    Current,
    Path(String),
}

impl<'de> Deserialize<'de> for NewTerminalCwdConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        match value.trim() {
            "" | "follow" => Ok(Self::Follow),
            "home" => Ok(Self::Home),
            "current" => Ok(Self::Current),
            _ => Ok(Self::Path(value)),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellModeConfig {
    #[default]
    Auto,
    Login,
    NonLogin,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct TerminalConfig {
    /// Executable used for new interactive panes. Empty means SHELL, then /bin/sh.
    pub default_shell: String,
    /// Startup mode for new interactive pane shells.
    pub shell_mode: ShellModeConfig,
    /// CWD policy for new interactive panes, tabs, and workspaces.
    pub new_cwd: NewTerminalCwdConfig,
    /// Render Kitty graphics in compatible outer terminals. Default: true.
    pub kitty_graphics: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct SessionConfig {
    /// Resume supported AI-agent panes into their native conversation sessions
    /// when restoring a Herdr session. Default: true.
    pub resume_agents_on_restore: bool,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            resume_agents_on_restore: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConfigReloadStatus {
    Applied,
    Partial,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ConfigReloadReport {
    pub status: ConfigReloadStatus,
    pub diagnostics: Vec<String>,
}

/// Validate `[ui]` sidebar bound configuration.
///
/// Returns `Some((min, max))` when `min <= max`, `None` otherwise. The two
/// values are funneled through this helper before they reach any
/// `u16::clamp(min, max)` call site (`u16::clamp` panics when `min > max`).
pub fn validated_sidebar_bounds(min: u16, max: u16) -> Option<(u16, u16)> {
    if min <= max {
        Some((min, max))
    } else {
        None
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub onboarding: Option<bool>,
    pub theme: ThemeConfig,
    pub terminal: TerminalConfig,
    pub session: SessionConfig,
    pub server: ServerConfig,
    pub update: UpdateConfig,
    pub keys: KeysConfig,
    pub ui: UiConfig,
    pub worktrees: WorktreesConfig,
    pub advanced: AdvancedConfig,
    pub experimental: ExperimentalConfig,
    pub remote: RemoteConfig,
    pub fleet: FleetConfig,
    pub gateway: GatewayConfig,
}

#[derive(Debug)]
pub struct LoadedConfig {
    pub config: Config,
    pub diagnostics: Vec<String>,
    pub invalid_sections: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct KeysConfig {
    /// Prefix key to enter prefix mode (e.g. "ctrl+b", "f12", "esc").
    pub prefix: String,
    /// Open keybinding help. Default: "prefix+?"
    pub help: BindingConfig,
    /// Open settings. Default: "prefix+s"
    pub settings: BindingConfig,
    /// Create a new workspace. Default: "prefix+shift+n"
    pub new_workspace: BindingConfig,
    /// Create a Git worktree from the selected workspace. Default: "prefix+shift+g"
    pub new_worktree: BindingConfig,
    /// Open an existing Git worktree from the selected workspace. Unset by default.
    pub open_worktree: BindingConfig,
    /// Delete the selected managed worktree checkout after confirmation. Unset by default.
    pub remove_worktree: BindingConfig,
    /// Rename the selected workspace. Default: "prefix+shift+w"
    pub rename_workspace: BindingConfig,
    /// Close the selected workspace. Default: "prefix+shift+d"
    pub close_workspace: BindingConfig,
    /// Open the workspace navigation surface. Default: "prefix+w"
    pub workspace_picker: BindingConfig,
    /// Open the session navigator. Default: "prefix+g"
    pub goto: BindingConfig,
    /// Move workspace selection up in navigate mode. Default: "up".
    pub navigate_workspace_up: BindingConfig,
    /// Move workspace selection down in navigate mode. Default: "down".
    pub navigate_workspace_down: BindingConfig,
    /// Focus the pane to the left in navigate mode. Default: "h". Left arrow is always an alias.
    pub navigate_pane_left: BindingConfig,
    /// Focus the pane below in navigate mode. Default: "j".
    pub navigate_pane_down: BindingConfig,
    /// Focus the pane above in navigate mode. Default: "k".
    pub navigate_pane_up: BindingConfig,
    /// Focus the pane to the right in navigate mode. Default: "l". Right arrow is always an alias.
    pub navigate_pane_right: BindingConfig,
    /// Detach the current client from its Herdr server. Default: "prefix+q".
    pub detach: BindingConfig,
    /// Reload config.toml in the running app/server. Default: "prefix+shift+r".
    pub reload_config: BindingConfig,
    /// Focus the currently visible notification target. Default: "prefix+o".
    pub open_notification_target: BindingConfig,
    /// Select the previous workspace. Unset by default.
    pub previous_workspace: BindingConfig,
    /// Select the next workspace. Unset by default.
    pub next_workspace: BindingConfig,
    /// Focus the previous agent shown in the agent panel. Unset by default.
    pub previous_agent: BindingConfig,
    /// Focus the next agent shown in the agent panel. Unset by default.
    pub next_agent: BindingConfig,
    /// Focus an agent by index 1-9. Unset by default.
    pub focus_agent: BindingConfig,
    /// Local-client shortcut that sends a clipboard image to a remote Herdr session. Default: "ctrl+v".
    pub remote_image_paste: String,
    /// Create a new tab in the active workspace. Default: "prefix+c"
    pub new_tab: BindingConfig,
    /// Rename the active tab. Default: "prefix+shift+t".
    pub rename_tab: BindingConfig,
    /// Select the previous tab. Default: "prefix+p".
    pub previous_tab: BindingConfig,
    /// Select the next tab. Default: "prefix+n".
    pub next_tab: BindingConfig,
    /// Move the active tab one position toward the front. Unset by default.
    pub move_tab_previous: BindingConfig,
    /// Move the active tab one position toward the back. Unset by default.
    pub move_tab_next: BindingConfig,
    /// Switch to tab 1-9. Default: "prefix+1..9".
    pub switch_tab: BindingConfig,
    /// Switch to workspace 1-9 from prefix mode. Unset by default.
    pub switch_workspace: BindingConfig,
    /// Close the active tab. Default: "prefix+shift+x".
    pub close_tab: BindingConfig,
    /// Rename the focused pane. Default: "prefix+shift+p".
    pub rename_pane: BindingConfig,
    /// Open the focused pane scrollback in $EDITOR. Default: "prefix+e".
    pub edit_scrollback: BindingConfig,
    /// Enter keyboard copy mode for the focused pane. Default: "prefix+[".
    pub copy_mode: BindingConfig,
    /// Focus the pane to the left. Default: "prefix+h".
    pub focus_pane_left: BindingConfig,
    /// Focus the pane below. Default: "prefix+j".
    pub focus_pane_down: BindingConfig,
    /// Focus the pane above. Default: "prefix+k".
    pub focus_pane_up: BindingConfig,
    /// Focus the pane to the right. Default: "prefix+l".
    pub focus_pane_right: BindingConfig,
    /// Swap the focused pane with the pane to the left. Default: "prefix+shift+h".
    pub swap_pane_left: BindingConfig,
    /// Swap the focused pane with the pane below. Default: "prefix+shift+j".
    pub swap_pane_down: BindingConfig,
    /// Swap the focused pane with the pane above. Default: "prefix+shift+k".
    pub swap_pane_up: BindingConfig,
    /// Swap the focused pane with the pane to the right. Default: "prefix+shift+l".
    pub swap_pane_right: BindingConfig,
    /// Cycle to the next pane. Default: "prefix+tab".
    pub cycle_pane_next: BindingConfig,
    /// Cycle to the previous pane. Default: "prefix+shift+tab".
    pub cycle_pane_previous: BindingConfig,
    /// Focus the last focused pane across workspaces and tabs. Unset by default.
    pub last_pane: BindingConfig,
    /// Split pane vertically (side by side). Default: "prefix+v"
    pub split_vertical: BindingConfig,
    /// Split pane horizontally (stacked). Default: "prefix+minus"
    pub split_horizontal: BindingConfig,
    /// Close the focused pane. Default: "prefix+x"
    pub close_pane: BindingConfig,
    /// Toggle zoom for the focused pane. Default: "prefix+z"
    #[serde(alias = "fullscreen")]
    pub zoom: BindingConfig,
    /// Enter resize mode. Default: "prefix+r"
    pub resize_mode: BindingConfig,
    /// Resize the focused pane toward the left. Unset by default.
    pub resize_pane_left: BindingConfig,
    /// Resize the focused pane downward. Unset by default.
    pub resize_pane_down: BindingConfig,
    /// Resize the focused pane upward. Unset by default.
    pub resize_pane_up: BindingConfig,
    /// Resize the focused pane toward the right. Unset by default.
    pub resize_pane_right: BindingConfig,
    /// Toggle sidebar collapse. Default: "prefix+b"
    pub toggle_sidebar: BindingConfig,
    /// Optional indexed shortcuts expanded over number keys 1-9.
    pub indexed: IndexedKeysConfig,
    /// Prefix-mode custom command bindings.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<CommandKeybindConfig>,
    #[serde(skip_serializing)]
    pub(crate) user_fields: BTreeSet<&'static str>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct KeysConfigOverlay {
    #[serde(skip_serializing_if = "Option::is_none")]
    prefix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    help: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    settings: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    new_workspace: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    new_worktree: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    open_worktree: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    remove_worktree: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rename_workspace: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    close_workspace: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace_picker: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    goto: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    navigate_workspace_up: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    navigate_workspace_down: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    navigate_pane_left: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    navigate_pane_down: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    navigate_pane_up: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    navigate_pane_right: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detach: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reload_config: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    open_notification_target: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_workspace: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_workspace: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_agent: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_agent: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    focus_agent: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    remote_image_paste: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    new_tab: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rename_tab: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_tab: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_tab: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    move_tab_previous: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    move_tab_next: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    switch_tab: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    switch_workspace: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    close_tab: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rename_pane: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    edit_scrollback: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    copy_mode: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    focus_pane_left: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    focus_pane_down: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    focus_pane_up: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    focus_pane_right: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    swap_pane_left: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    swap_pane_down: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    swap_pane_up: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    swap_pane_right: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cycle_pane_next: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cycle_pane_previous: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_pane: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    split_vertical: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    split_horizontal: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    close_pane: Option<BindingConfig>,
    #[serde(alias = "fullscreen", skip_serializing_if = "Option::is_none")]
    zoom: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resize_mode: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resize_pane_left: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resize_pane_down: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resize_pane_up: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resize_pane_right: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    toggle_sidebar: Option<BindingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    indexed: Option<IndexedKeysConfig>,
    #[serde(skip_serializing)]
    command: Option<Vec<CommandKeybindConfig>>,
}

impl KeysConfigOverlay {
    pub(crate) fn set_prefix(&mut self, prefix: String) {
        self.prefix = Some(prefix);
    }
}

impl<'de> Deserialize<'de> for KeysConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let input = KeysConfigOverlay::deserialize(deserializer)?;
        let mut keys = KeysConfig::default();

        macro_rules! apply_field {
            ($field:ident) => {
                if let Some(value) = input.$field {
                    keys.$field = value;
                    keys.user_fields.insert(stringify!($field));
                }
            };
        }

        apply_field!(prefix);
        apply_field!(help);
        apply_field!(settings);
        apply_field!(new_workspace);
        apply_field!(new_worktree);
        apply_field!(open_worktree);
        apply_field!(remove_worktree);
        apply_field!(rename_workspace);
        apply_field!(close_workspace);
        apply_field!(workspace_picker);
        apply_field!(goto);
        apply_field!(navigate_workspace_up);
        apply_field!(navigate_workspace_down);
        apply_field!(navigate_pane_left);
        apply_field!(navigate_pane_down);
        apply_field!(navigate_pane_up);
        apply_field!(navigate_pane_right);
        apply_field!(detach);
        apply_field!(reload_config);
        apply_field!(open_notification_target);
        apply_field!(previous_workspace);
        apply_field!(next_workspace);
        apply_field!(previous_agent);
        apply_field!(next_agent);
        apply_field!(focus_agent);
        apply_field!(remote_image_paste);
        apply_field!(new_tab);
        apply_field!(rename_tab);
        apply_field!(previous_tab);
        apply_field!(next_tab);
        apply_field!(move_tab_previous);
        apply_field!(move_tab_next);
        apply_field!(switch_tab);
        apply_field!(switch_workspace);
        apply_field!(close_tab);
        apply_field!(rename_pane);
        apply_field!(edit_scrollback);
        apply_field!(copy_mode);
        apply_field!(focus_pane_left);
        apply_field!(focus_pane_down);
        apply_field!(focus_pane_up);
        apply_field!(focus_pane_right);
        apply_field!(swap_pane_left);
        apply_field!(swap_pane_down);
        apply_field!(swap_pane_up);
        apply_field!(swap_pane_right);
        apply_field!(cycle_pane_next);
        apply_field!(cycle_pane_previous);
        apply_field!(last_pane);
        apply_field!(split_vertical);
        apply_field!(split_horizontal);
        apply_field!(close_pane);
        apply_field!(zoom);
        apply_field!(resize_mode);
        apply_field!(resize_pane_left);
        apply_field!(resize_pane_down);
        apply_field!(resize_pane_up);
        apply_field!(resize_pane_right);
        apply_field!(toggle_sidebar);
        apply_field!(indexed);
        apply_field!(command);

        Ok(keys)
    }
}

impl KeysConfig {
    pub(crate) fn key_field_is_user_configured(&self, field: &str) -> bool {
        self.user_fields.contains(field)
    }

    pub(crate) fn local_profile(&self, keybinds: &Keybinds) -> KeysConfigOverlay {
        let mut profile = KeysConfigOverlay::default();

        macro_rules! copy_user_field {
            ($field:ident) => {
                if self.user_fields.contains(stringify!($field)) {
                    profile.$field = Some(self.$field.clone());
                }
            };
        }
        macro_rules! copy_effective_action_field {
            ($field:ident, $target:expr) => {
                if self.user_fields.contains(stringify!($field)) {
                    profile.$field = Some(self.$field.clone());
                } else if binding_config_is_effective(&self.$field, &$target) {
                    profile.$field = Some(self.$field.clone());
                } else if binding_config_has_values(&self.$field) {
                    profile.$field = Some(BindingConfig::empty());
                }
            };
        }
        macro_rules! copy_effective_indexed_field {
            ($field:ident, $target:expr) => {
                if self.user_fields.contains(stringify!($field)) {
                    profile.$field = Some(self.$field.clone());
                } else if let Some(effective) = effective_indexed_config(&self.$field, &$target) {
                    profile.$field = Some(effective);
                } else if binding_config_has_values(&self.$field) {
                    profile.$field = Some(BindingConfig::empty());
                }
            };
        }

        profile.prefix = Some(self.prefix.clone());
        copy_effective_action_field!(help, keybinds.help);
        copy_effective_action_field!(settings, keybinds.settings);
        copy_effective_action_field!(new_workspace, keybinds.new_workspace);
        copy_effective_action_field!(new_worktree, keybinds.new_worktree);
        copy_effective_action_field!(open_worktree, keybinds.open_worktree);
        copy_effective_action_field!(remove_worktree, keybinds.remove_worktree);
        copy_effective_action_field!(rename_workspace, keybinds.rename_workspace);
        copy_effective_action_field!(close_workspace, keybinds.close_workspace);
        copy_effective_action_field!(workspace_picker, keybinds.workspace_picker);
        copy_effective_action_field!(goto, keybinds.goto);
        copy_effective_action_field!(navigate_workspace_up, keybinds.navigate.workspace_up);
        copy_effective_action_field!(navigate_workspace_down, keybinds.navigate.workspace_down);
        copy_effective_action_field!(navigate_pane_left, keybinds.navigate.pane_left);
        copy_effective_action_field!(navigate_pane_down, keybinds.navigate.pane_down);
        copy_effective_action_field!(navigate_pane_up, keybinds.navigate.pane_up);
        copy_effective_action_field!(navigate_pane_right, keybinds.navigate.pane_right);
        copy_effective_action_field!(detach, keybinds.detach);
        copy_effective_action_field!(reload_config, keybinds.reload_config);
        copy_effective_action_field!(open_notification_target, keybinds.open_notification_target);
        copy_effective_action_field!(previous_workspace, keybinds.previous_workspace);
        copy_effective_action_field!(next_workspace, keybinds.next_workspace);
        copy_effective_action_field!(previous_agent, keybinds.previous_agent);
        copy_effective_action_field!(next_agent, keybinds.next_agent);
        copy_effective_indexed_field!(focus_agent, keybinds.focus_agent);
        copy_user_field!(remote_image_paste);
        copy_effective_action_field!(new_tab, keybinds.new_tab);
        copy_effective_action_field!(rename_tab, keybinds.rename_tab);
        copy_effective_action_field!(previous_tab, keybinds.previous_tab);
        copy_effective_action_field!(next_tab, keybinds.next_tab);
        copy_effective_action_field!(move_tab_previous, keybinds.move_tab_previous);
        copy_effective_action_field!(move_tab_next, keybinds.move_tab_next);
        copy_effective_indexed_field!(switch_tab, keybinds.switch_tab);
        copy_effective_indexed_field!(switch_workspace, keybinds.switch_workspace);
        copy_effective_action_field!(close_tab, keybinds.close_tab);
        copy_effective_action_field!(rename_pane, keybinds.rename_pane);
        copy_effective_action_field!(edit_scrollback, keybinds.edit_scrollback);
        copy_effective_action_field!(copy_mode, keybinds.copy_mode);
        copy_effective_action_field!(focus_pane_left, keybinds.focus_pane_left);
        copy_effective_action_field!(focus_pane_down, keybinds.focus_pane_down);
        copy_effective_action_field!(focus_pane_up, keybinds.focus_pane_up);
        copy_effective_action_field!(focus_pane_right, keybinds.focus_pane_right);
        copy_effective_action_field!(swap_pane_left, keybinds.swap_pane_left);
        copy_effective_action_field!(swap_pane_down, keybinds.swap_pane_down);
        copy_effective_action_field!(swap_pane_up, keybinds.swap_pane_up);
        copy_effective_action_field!(swap_pane_right, keybinds.swap_pane_right);
        copy_effective_action_field!(cycle_pane_next, keybinds.cycle_pane_next);
        copy_effective_action_field!(cycle_pane_previous, keybinds.cycle_pane_previous);
        copy_effective_action_field!(last_pane, keybinds.last_pane);
        copy_effective_action_field!(split_vertical, keybinds.split_vertical);
        copy_effective_action_field!(split_horizontal, keybinds.split_horizontal);
        copy_effective_action_field!(close_pane, keybinds.close_pane);
        copy_effective_action_field!(zoom, keybinds.zoom);
        copy_effective_action_field!(resize_mode, keybinds.resize_mode);
        copy_effective_action_field!(resize_pane_left, keybinds.resize_pane_left);
        copy_effective_action_field!(resize_pane_down, keybinds.resize_pane_down);
        copy_effective_action_field!(resize_pane_up, keybinds.resize_pane_up);
        copy_effective_action_field!(resize_pane_right, keybinds.resize_pane_right);
        copy_effective_action_field!(toggle_sidebar, keybinds.toggle_sidebar);
        copy_user_field!(indexed);

        profile
    }
}

fn binding_config_has_values(config: &BindingConfig) -> bool {
    config.has_values()
}

fn binding_config_is_effective(config: &BindingConfig, keybinds: &ActionKeybinds) -> bool {
    !binding_config_has_values(config) || !keybinds.bindings.is_empty()
}

fn effective_indexed_config(
    config: &BindingConfig,
    keybinds: &[IndexedKeybind],
) -> Option<BindingConfig> {
    if !binding_config_has_values(config) {
        return Some(config.clone());
    }

    let expected_labels = config.indexed_labels();
    if expected_labels.is_empty() {
        return None;
    }

    let effective_labels: Vec<String> = expected_labels
        .iter()
        .filter(|expected| {
            keybinds
                .iter()
                .any(|binding| binding.label.as_str() == expected.as_str())
        })
        .cloned()
        .collect();

    if effective_labels.is_empty() {
        None
    } else if effective_labels.len() == expected_labels.len() {
        Some(config.clone())
    } else {
        Some(BindingConfig::Many(effective_labels))
    }
}

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct IndexedKeysConfig {
    /// Modifier combo for tab shortcuts 1-9. Unset by default.
    pub tabs: String,
    /// Modifier combo for workspace shortcuts 1-9. Unset by default.
    pub workspaces: String,
    /// Modifier combo for agent shortcuts 1-9. Unset by default.
    pub agents: String,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct WorktreesConfig {
    /// Root directory under which Herdr creates <repo>/<branch-slug> checkouts.
    pub directory: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TabBarPositionConfig {
    #[default]
    Top,
    Bottom,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub sidebar_width: u16,
    /// Minimum sidebar width (columns) when expanded. Default: 18.
    pub sidebar_min_width: u16,
    /// Maximum sidebar width (columns) when expanded. Default: 36.
    pub sidebar_max_width: u16,
    /// Start with the sidebar collapsed. Default: false.
    pub sidebar_start_collapsed: bool,
    /// Collapsed sidebar presentation. Default: compact.
    pub sidebar_collapsed_mode: SidebarCollapsedModeConfig,
    /// Terminal width at or below which Herdr uses the mobile single-column layout. Default: 64.
    pub mobile_width_threshold: u16,
    /// Capture mouse input for Herdr's mouse UI. Default: true.
    pub mouse_capture: bool,
    /// Copy text selected with the mouse. Default: true.
    pub copy_on_select: bool,
    /// Host cursor policy. Default: auto.
    pub host_cursor: HostCursorModeConfig,
    /// Modifier that lets right-click gestures pass through to pane apps. Empty disables it.
    pub right_click_passthrough_modifier: RightClickPassthroughModifierConfig,
    /// Force a full host-terminal redraw when the outer terminal regains focus. Default: true.
    pub redraw_on_focus_gained: bool,
    /// Lines to scroll per mouse wheel notch. Default: 3.
    pub mouse_scroll_lines: Option<NonZeroUsize>,
    /// Ask for confirmation before closing a workspace. Default: true.
    pub confirm_close: bool,
    /// Ask for a tab name before creating a new tab. Default: true.
    pub prompt_new_tab_name: bool,
    /// Ask for a workspace name before interactive creation. Default: false.
    pub prompt_new_workspace_name: bool,
    /// Draw borders around split panes. Default: true.
    pub pane_borders: bool,
    /// Draw borders along the outside edge of the pane area. Default: true.
    pub pane_outer_borders: bool,
    /// Draw interactive scrollbars beside terminal panes. Default: true.
    pub pane_scrollbars: bool,
    /// Keep split panes visually separated instead of sharing divider borders. Default: true.
    pub pane_gaps: bool,
    /// Show agent labels in split pane borders when no manual pane label is set. Default: false.
    pub show_agent_labels_on_pane_borders: bool,
    /// Hide the tab row when the workspace has one tab. Default: false.
    pub hide_tab_bar_when_single_tab: bool,
    /// Desktop tab row placement. Default: top.
    pub tab_bar_position: TabBarPositionConfig,
    /// Ordered entries shown at the right edge of the desktop tab row. Empty by default.
    pub tab_bar_right: Vec<TabBarRightEntryConfig>,
    /// Text inserted between visible right-side tab bar entries. Default: one space.
    pub tab_bar_right_separator: String,
    /// Format for the outer terminal window title. Empty leaves the title alone.
    /// Default: "{hostname}: {workspace}".
    pub window_title: String,
    /// Agent sidebar ordering. Saved values are "spaces" or "priority". Default: "spaces".
    pub agent_panel_sort: AgentPanelSortConfig,
    /// Retired setting that Herdr wrote before the workspace filter was removed.
    #[serde(rename = "agent_panel_scope")]
    _legacy_agent_panel_scope: Option<LegacyAgentPanelScopeConfig>,
    /// Agent status indicator style. Saved values are "dots" or "symbols". Default: "dots".
    pub status_indicators: StatusIndicatorStyle,
    /// Expanded sidebar row composition.
    pub sidebar: SidebarConfig,
    /// Accent color for highlights, borders, and navigation UI.
    /// Accepts hex (#89b4fa), named colors (cyan, blue), or RGB (rgb(137,180,250)).
    pub accent: String,
    /// Optional visual toast notifications for background workspace events.
    pub toast: ToastConfig,
    /// Play sounds when agents change state in background workspaces.
    pub sound: SoundConfig,
}

/// Cursor shape (DECSCUSR) used for the forced IME anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImeCursorShape {
    Block,
    #[default]
    SteadyBlock,
    Underline,
    SteadyUnderline,
    Bar,
    SteadyBar,
}

impl ImeCursorShape {
    /// Convert to DECSCUSR parameter (1–6).
    pub fn to_decscusr(self) -> u8 {
        match self {
            Self::Block => 1,
            Self::SteadyBlock => 2,
            Self::Underline => 3,
            Self::SteadyUnderline => 4,
            Self::Bar => 5,
            Self::SteadyBar => 6,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// Virtual terminal width used when no client is attached. Default: 120.
    pub headless_cols: u16,
    /// Virtual terminal height used when no client is attached. Default: 40.
    pub headless_rows: u16,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct AdvancedConfig {
    /// Maximum scrollback buffer size in bytes retained per pane terminal. Default: 10000000.
    #[serde(alias = "scrollback_lines")]
    pub scrollback_limit_bytes: usize,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct RemoteConfig {
    /// Add keepalive fallbacks and private connection reuse for `herdr --remote`.
    /// Set false to run plain ssh unchanged. Default: true.
    pub manage_ssh_config: bool,
}

impl Default for RemoteConfig {
    fn default() -> Self {
        Self {
            manage_ssh_config: true,
        }
    }
}

/// `[fleet]` — hosts aggregated by the fork's fleet runtime.
///
/// Field names are the user-facing contract: add fields, never rename them.
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct FleetConfig {
    /// Include this machine's default session as host "local". Default: true.
    pub include_local: bool,
    /// Additional hosts; see `[[fleet.hosts]]` in `herdr --default-config`.
    pub hosts: Vec<FleetHostConfig>,
}

impl Default for FleetConfig {
    fn default() -> Self {
        Self {
            include_local: true,
            hosts: Vec::new(),
        }
    }
}

/// One `[[fleet.hosts]]` entry.
///
/// Every field has a default so a malformed entry is reported as a
/// diagnostic instead of failing the whole config parse, matching how the
/// other sections behave.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct FleetHostConfig {
    /// Display name and id prefix (`workbox/w1:p1`). Required.
    pub name: String,
    /// Transport used to reach the host. Default: "ssh".
    pub kind: FleetHostKind,
    /// ssh destination (alias, `user@host`, `ssh://host:2222`). Required for
    /// `kind = "ssh"`.
    pub target: Option<String>,
    /// Named session on that host. Required for `kind = "local"`.
    pub session: Option<String>,
    /// Whether the fleet connects to this host. Default: true.
    pub enabled: bool,
}

impl Default for FleetHostConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            kind: FleetHostKind::default(),
            target: None,
            session: None,
            enabled: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FleetHostKind {
    #[default]
    Ssh,
    Local,
}

impl FleetConfig {
    /// Validate `[fleet]` without touching the network or the filesystem.
    ///
    /// Every problem is reported; the caller decides whether to keep going
    /// (the TUI shows the diagnostic banner) or to refuse (`herdr fleet`).
    pub fn diagnostics(&self) -> Vec<String> {
        let mut diagnostics = Vec::new();
        let mut seen: BTreeSet<&str> = BTreeSet::new();

        for (index, host) in self.hosts.iter().enumerate() {
            let field = |key: &str| format!("fleet.hosts[{index}].{key}");

            match validate_fleet_host_name(&host.name) {
                Err(reason) => diagnostics.push(format!(
                    "invalid fleet host name: {} = {:?}; {reason}; ignoring [fleet] hosts",
                    field("name"),
                    host.name
                )),
                Ok(()) => {
                    if self.include_local && host.name == FLEET_LOCAL_HOST_NAME {
                        diagnostics.push(format!(
                            "reserved fleet host name: {} = {:?} names this machine's default session; set fleet.include_local = false or rename the host; ignoring [fleet] hosts",
                            field("name"),
                            host.name
                        ));
                    }
                    if !seen.insert(host.name.as_str()) {
                        diagnostics.push(format!(
                            "duplicate fleet host name: {} = {:?}; host names must be unique; ignoring [fleet] hosts",
                            field("name"),
                            host.name
                        ));
                    }
                }
            }

            match host.kind {
                FleetHostKind::Ssh => match host.target.as_deref() {
                    None => diagnostics.push(format!(
                        "missing fleet host target: {} is required for kind = \"ssh\"; ignoring [fleet] hosts",
                        field("target")
                    )),
                    Some(target) => {
                        if let Err(reason) = validate_fleet_host_target(target) {
                            diagnostics.push(format!(
                                "invalid fleet host target: {} = {target:?}; {reason}; ignoring [fleet] hosts",
                                field("target")
                            ));
                        }
                    }
                },
                FleetHostKind::Local => {
                    if host.session.is_none() {
                        diagnostics.push(format!(
                            "missing fleet host session: {} is required for kind = \"local\"; ignoring [fleet] hosts",
                            field("session")
                        ));
                    }
                    if host.target.is_some() {
                        // Refused rather than ignored: silently dropping the
                        // target would attach the user to a same-named local
                        // session instead of the machine they named.
                        diagnostics.push(format!(
                            "unused fleet host target: {} is only used for kind = \"ssh\"; remove it or set kind = \"ssh\"; ignoring [fleet] hosts",
                            field("target")
                        ));
                    }
                }
            }

            if let Some(session) = host.session.as_deref() {
                if let Err(reason) = crate::session::validate_name(session) {
                    diagnostics.push(format!(
                        "invalid fleet host session: {} = {session:?}; {reason}; ignoring [fleet] hosts",
                        field("session")
                    ));
                }
            }
        }

        diagnostics
    }
}

/// Reserved id of this machine's default session.
pub const FLEET_LOCAL_HOST_NAME: &str = "local";

/// Fleet host names follow session-name rules so they can be used verbatim as
/// the `host/` prefix of a fleet id; `session::validate_name` stays the single
/// source of truth for the character class.
pub(crate) fn validate_fleet_host_name(name: &str) -> Result<(), String> {
    crate::session::validate_name(name).map_err(|err| err.replacen("session name", "host name", 1))
}

/// Same rule `--remote` applies to its target, so a target can never be read
/// as an ssh flag, plus a shape check `--remote` gets from the shell: an ssh
/// destination is one argv element, so whitespace or control characters mean
/// the value is malformed (and ` -oProxyCommand=...` would otherwise slip past
/// the leading-dash check).
pub(crate) fn validate_fleet_host_target(target: &str) -> Result<(), String> {
    if target.is_empty() {
        return Err("ssh target cannot be empty".to_string());
    }
    if target.starts_with('-') {
        return Err("ssh target must not start with '-'".to_string());
    }
    if target
        .chars()
        .any(|ch| ch.is_whitespace() || ch.is_control())
    {
        return Err("ssh target must not contain whitespace or control characters".to_string());
    }
    Ok(())
}

/// `[gateway]` — the fork's HTTP/WebSocket gateway (`herdr gateway`).
///
/// The section is unconditional: an upstream-shaped `--no-default-features`
/// build still parses and validates it (pure data, no optional dependency)
/// instead of reporting `[gateway]` as an unknown top-level key.
///
/// Field names are the user-facing contract: add fields, never rename them.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct GatewayConfig {
    /// Listen address. Loopback by default; any other address also requires
    /// `allowed_origins`. Default: "127.0.0.1:7788".
    pub bind: String,
    /// Browser origins allowed to call the API, as `scheme://host[:port]`.
    /// Required for a non-loopback bind. Default: empty.
    pub allowed_origins: Vec<String>,
    /// Origin that pairing URLs and QR codes advertise. Implicitly allowed;
    /// an `https` scheme marks device cookies `Secure`. Default: empty.
    pub public_url: String,
    /// Failed authentications from one peer address before it is refused for
    /// the rest of the window. Default: 5.
    pub auth_failure_limit: u32,
    /// Length of the failed-authentication window, in seconds. Default: 60.
    pub auth_failure_window_secs: u64,
    /// How long a pairing URL from `herdr gateway pair` stays valid, in
    /// seconds. Default: 600.
    pub pairing_ttl_secs: u64,
}

/// Default listen address, used when `[gateway] bind` is absent or invalid.
pub const DEFAULT_GATEWAY_BIND: &str = "127.0.0.1:7788";
/// Inclusive bounds for `[gateway] pairing_ttl_secs` (30 s to 24 h).
pub const MIN_GATEWAY_PAIRING_TTL_SECS: u64 = 30;
pub const MAX_GATEWAY_PAIRING_TTL_SECS: u64 = 86_400;

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            bind: DEFAULT_GATEWAY_BIND.to_string(),
            allowed_origins: Vec::new(),
            public_url: String::new(),
            auth_failure_limit: 5,
            auth_failure_window_secs: 60,
            pairing_ttl_secs: 600,
        }
    }
}

impl GatewayConfig {
    /// Validate `[gateway]` without touching the network or the filesystem.
    ///
    /// Every problem is reported. A bad value is a diagnostic, not a parse
    /// failure, so one typo never drops the rest of the section; the gateway
    /// falls back to the documented default for that key. Refusing to listen
    /// at all (a non-loopback bind without origins) is a run-time decision in
    /// `gateway::policy::BindPolicy`, not a config diagnostic, because a
    /// `--bind` flag can still make the same config valid.
    pub fn diagnostics(&self) -> Vec<String> {
        let defaults = Self::default();
        let mut diagnostics = Vec::new();

        if self.bind_addr().is_none() {
            diagnostics.push(format!(
                "invalid gateway bind: gateway.bind = {:?}; expected ADDRESS:PORT (for example {DEFAULT_GATEWAY_BIND}); using {DEFAULT_GATEWAY_BIND}",
                self.bind
            ));
        }

        let mut seen: BTreeSet<String> = BTreeSet::new();
        for (index, origin) in self.allowed_origins.iter().enumerate() {
            match parse_gateway_origin(origin) {
                Err(reason) => diagnostics.push(format!(
                    "invalid gateway origin: gateway.allowed_origins[{index}] = {origin:?}; {reason}; ignoring this origin"
                )),
                Ok(parsed) => {
                    // Two spellings of the same origin (case, or a repeat) are
                    // reported so the allowlist reads as what it enforces.
                    let normalized = parsed.to_header_value();
                    if !seen.insert(normalized.clone()) {
                        diagnostics.push(format!(
                            "duplicate gateway origin: gateway.allowed_origins[{index}] = {origin:?} repeats {normalized:?}"
                        ));
                    }
                }
            }
        }

        if !self.public_url.is_empty() {
            if let Err(reason) = parse_gateway_origin(&self.public_url) {
                diagnostics.push(format!(
                    "invalid gateway public url: gateway.public_url = {:?}; {reason}; ignoring it",
                    self.public_url
                ));
            }
        }

        if self.auth_failure_limit == 0 {
            diagnostics.push(format!(
                "invalid gateway auth failure limit: gateway.auth_failure_limit = 0 would refuse every request; using {}",
                defaults.auth_failure_limit
            ));
        }
        if self.auth_failure_window_secs == 0 {
            diagnostics.push(format!(
                "invalid gateway auth failure window: gateway.auth_failure_window_secs = 0 disables rate limiting; using {}",
                defaults.auth_failure_window_secs
            ));
        }
        if !(MIN_GATEWAY_PAIRING_TTL_SECS..=MAX_GATEWAY_PAIRING_TTL_SECS)
            .contains(&self.pairing_ttl_secs)
        {
            diagnostics.push(format!(
                "invalid gateway pairing ttl: gateway.pairing_ttl_secs = {}; expected {MIN_GATEWAY_PAIRING_TTL_SECS}..={MAX_GATEWAY_PAIRING_TTL_SECS}; using {}",
                self.pairing_ttl_secs, defaults.pairing_ttl_secs
            ));
        }

        diagnostics
    }

    /// The configured listen address, or `None` when `bind` does not parse.
    pub fn bind_addr(&self) -> Option<std::net::SocketAddr> {
        self.bind.parse().ok()
    }
}

/// One browser origin: `scheme://host[:port]` and nothing else.
///
/// The canonical parser lives here rather than in `src/gateway/policy.rs`
/// because `[gateway]` is unconditional while the gateway module is behind a
/// cargo feature: a single parser means a `--no-default-features` build
/// reports exactly the origins a gateway build would refuse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayOrigin {
    /// Lowercase scheme, `http` or `https`.
    pub scheme: String,
    /// Lowercase host: a name, an IPv4 literal, or a bracketed IPv6 literal.
    pub host: String,
    /// Explicit port, or `None` when the origin uses the scheme default.
    pub port: Option<u16>,
}

impl GatewayOrigin {
    /// Serialize back to the `scheme://host[:port]` form browsers send.
    pub fn to_header_value(&self) -> String {
        match self.port {
            Some(port) => format!("{}://{}:{port}", self.scheme, self.host),
            None => format!("{}://{}", self.scheme, self.host),
        }
    }
}

/// Parse a `scheme://host[:port]` origin, rejecting everything else.
///
/// Rejected on purpose: schemes other than http/https, userinfo, a path (even
/// a bare trailing `/`), a query, a fragment, an empty host, and `null`. A
/// browser never sends any of them in an `Origin` header, so accepting them
/// would only widen the allowlist.
pub fn parse_gateway_origin(value: &str) -> Result<GatewayOrigin, String> {
    if value.is_empty() {
        return Err("origin cannot be empty".to_string());
    }
    if value
        .chars()
        .any(|ch| ch.is_whitespace() || ch.is_control())
    {
        return Err("origin must not contain whitespace or control characters".to_string());
    }

    let Some((scheme, rest)) = value.split_once("://") else {
        return Err("expected scheme://host[:port]".to_string());
    };
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Err(format!(
            "unsupported scheme {scheme:?}; expected http or https"
        ));
    }
    if rest.contains('/') {
        return Err("origin must not contain a path".to_string());
    }
    if rest.contains('?') || rest.contains('#') {
        return Err("origin must not contain a query or fragment".to_string());
    }
    if rest.contains('@') {
        return Err("origin must not contain userinfo".to_string());
    }

    let (host, port) = split_origin_host_port(rest)?;
    if host.is_empty() {
        return Err("origin host cannot be empty".to_string());
    }

    Ok(GatewayOrigin {
        scheme,
        host: host.to_ascii_lowercase(),
        port,
    })
}

/// Split `host[:port]`, honouring the `[..]` brackets of an IPv6 literal.
fn split_origin_host_port(rest: &str) -> Result<(&str, Option<u16>), String> {
    if let Some(after_bracket) = rest.strip_prefix('[') {
        let Some((inside, tail)) = after_bracket.split_once(']') else {
            return Err("unterminated IPv6 literal; expected [address]".to_string());
        };
        if inside.parse::<std::net::Ipv6Addr>().is_err() {
            return Err(format!("invalid IPv6 literal {inside:?}"));
        }
        let port = parse_origin_port(tail)?;
        // Keep the brackets: they are part of the host as browsers send it.
        let host_end = 1 + inside.len() + 1;
        return Ok((&rest[..host_end], port));
    }

    match rest.split_once(':') {
        Some((host, port)) => {
            if host.contains(':') || port.contains(':') {
                return Err("IPv6 literals must be bracketed, as [::1]".to_string());
            }
            Ok((host, parse_origin_port(&format!(":{port}"))?))
        }
        None => Ok((rest, None)),
    }
}

fn parse_origin_port(tail: &str) -> Result<Option<u16>, String> {
    if tail.is_empty() {
        return Ok(None);
    }
    let Some(digits) = tail.strip_prefix(':') else {
        return Err("expected scheme://host[:port]".to_string());
    };
    if digits.is_empty() {
        return Err("origin port cannot be empty".to_string());
    }
    digits
        .parse::<u16>()
        .map(Some)
        .map_err(|_| format!("invalid origin port {digits:?}"))
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ExperimentalConfig {
    /// Allow launching herdr inside an existing herdr pane. Default: false.
    pub allow_nested: bool,
    /// Deprecated compatibility key for `terminal.kitty_graphics`.
    pub kitty_graphics: Option<bool>,
    /// Persist pane screen history to session-history.json. Default: false.
    pub pane_history: bool,
    /// Expose the focused pane's cursor anchor to the outer terminal even when
    /// the pane requested `?25l`, so macOS native input methods keep tracking
    /// the candidate window when TUIs paint their own cursor (Claude Code, pi,
    /// codex, etc.). Default: false.
    ///
    /// When the pane reports no cursor position, falls back to the pane's
    /// top-left so a stable IME anchor is always available.
    ///
    /// Trade-off when enabled: an extra hardware cursor will be visible in the
    /// outer terminal for apps that hide the cursor without painting a
    /// replacement (vim normal mode, etc.). See #149.
    pub reveal_hidden_cursor_for_cjk_ime: bool,
    /// Restrict `reveal_hidden_cursor_for_cjk_ime` to focused panes whose
    /// detected agent matches one of these names (case-insensitive). Empty
    /// list means apply to any focused pane. Unknown agent names are ignored;
    /// if the list contains no valid names, the reveal does not apply.
    /// Accepted names: pi, claude, codex, gemini, cursor, devin, cline,
    /// opencode, copilot, kimi, kiro, droid, amp, grok, hermes, kilo,
    /// qodercli, qoder, qwen, qwen-code, maki.
    /// Default: empty.
    pub cjk_ime_agents: Vec<String>,
    /// Cursor shape rendered for the IME anchor when
    /// `reveal_hidden_cursor_for_cjk_ime` is enabled. Default: "steady_block".
    pub cjk_ime_cursor_shape: ImeCursorShape,
    /// While prefix mode is active, temporarily switch the host input source
    /// to an ASCII-capable mode so prefix commands are read as ASCII even when
    /// an IME is active, then restore the previous input source when prefix
    /// mode exits. On macOS this selects the ASCII-capable keyboard layout; on
    /// Windows it switches the IME to English (ASCII) input. Windows support is
    /// currently limited to the Korean IME; with an IME for any other language,
    /// the input source is left unchanged. macOS and Windows only; a no-op
    /// elsewhere and a best-effort no-op if the switch fails.
    /// Default: false.
    pub switch_ascii_input_source_in_prefix: bool,
}

impl Default for KeysConfig {
    fn default() -> Self {
        Self {
            prefix: "ctrl+b".into(),
            help: BindingConfig::one("prefix+?"),
            settings: BindingConfig::one("prefix+s"),
            new_workspace: BindingConfig::one("prefix+shift+n"),
            new_worktree: BindingConfig::one("prefix+shift+g"),
            open_worktree: BindingConfig::empty(),
            remove_worktree: BindingConfig::empty(),
            rename_workspace: BindingConfig::one("prefix+shift+w"),
            close_workspace: BindingConfig::one("prefix+shift+d"),
            workspace_picker: BindingConfig::one("prefix+w"),
            goto: BindingConfig::one("prefix+g"),
            navigate_workspace_up: BindingConfig::one("up"),
            navigate_workspace_down: BindingConfig::one("down"),
            navigate_pane_left: BindingConfig::one("h"),
            navigate_pane_down: BindingConfig::one("j"),
            navigate_pane_up: BindingConfig::one("k"),
            navigate_pane_right: BindingConfig::one("l"),
            detach: BindingConfig::one("prefix+q"),
            reload_config: BindingConfig::one("prefix+shift+r"),
            open_notification_target: BindingConfig::one("prefix+o"),
            previous_workspace: BindingConfig::empty(),
            next_workspace: BindingConfig::empty(),
            previous_agent: BindingConfig::empty(),
            next_agent: BindingConfig::empty(),
            focus_agent: BindingConfig::empty(),
            remote_image_paste: "ctrl+v".into(),
            new_tab: BindingConfig::one("prefix+c"),
            rename_tab: BindingConfig::one("prefix+shift+t"),
            previous_tab: BindingConfig::one("prefix+p"),
            next_tab: BindingConfig::one("prefix+n"),
            move_tab_previous: BindingConfig::empty(),
            move_tab_next: BindingConfig::empty(),
            switch_tab: BindingConfig::one("prefix+1..9"),
            switch_workspace: BindingConfig::empty(),
            close_tab: BindingConfig::one("prefix+shift+x"),
            rename_pane: BindingConfig::one("prefix+shift+p"),
            edit_scrollback: BindingConfig::one("prefix+e"),
            copy_mode: BindingConfig::one("prefix+["),
            focus_pane_left: BindingConfig::one("prefix+h"),
            focus_pane_down: BindingConfig::one("prefix+j"),
            focus_pane_up: BindingConfig::one("prefix+k"),
            focus_pane_right: BindingConfig::one("prefix+l"),
            swap_pane_left: BindingConfig::one("prefix+shift+h"),
            swap_pane_down: BindingConfig::one("prefix+shift+j"),
            swap_pane_up: BindingConfig::one("prefix+shift+k"),
            swap_pane_right: BindingConfig::one("prefix+shift+l"),
            cycle_pane_next: BindingConfig::one("prefix+tab"),
            cycle_pane_previous: BindingConfig::one("prefix+shift+tab"),
            last_pane: BindingConfig::empty(),
            split_vertical: BindingConfig::one("prefix+v"),
            split_horizontal: BindingConfig::one("prefix+minus"),
            close_pane: BindingConfig::one("prefix+x"),
            zoom: BindingConfig::one("prefix+z"),
            resize_mode: BindingConfig::one("prefix+r"),
            resize_pane_left: BindingConfig::empty(),
            resize_pane_down: BindingConfig::empty(),
            resize_pane_up: BindingConfig::empty(),
            resize_pane_right: BindingConfig::empty(),
            toggle_sidebar: BindingConfig::one("prefix+b"),
            indexed: IndexedKeysConfig::default(),
            command: Vec::new(),
            user_fields: BTreeSet::new(),
        }
    }
}

impl Default for WorktreesConfig {
    fn default() -> Self {
        Self {
            directory: "~/.herdr/worktrees".into(),
        }
    }
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            sidebar_width: 26,
            sidebar_min_width: 18,
            sidebar_max_width: 36,
            sidebar_start_collapsed: false,
            sidebar_collapsed_mode: SidebarCollapsedModeConfig::Compact,
            mobile_width_threshold: DEFAULT_MOBILE_WIDTH_THRESHOLD,
            mouse_capture: true,
            copy_on_select: true,
            host_cursor: HostCursorModeConfig::Auto,
            right_click_passthrough_modifier: RightClickPassthroughModifierConfig::default(),
            redraw_on_focus_gained: true,
            mouse_scroll_lines: None,
            confirm_close: true,
            prompt_new_tab_name: true,
            prompt_new_workspace_name: false,
            pane_borders: true,
            pane_outer_borders: true,
            pane_scrollbars: true,
            pane_gaps: true,
            show_agent_labels_on_pane_borders: false,
            hide_tab_bar_when_single_tab: false,
            tab_bar_position: TabBarPositionConfig::Top,
            tab_bar_right: Vec::new(),
            tab_bar_right_separator: " ".into(),
            window_title: super::window_title::default_window_title(),
            agent_panel_sort: AgentPanelSortConfig::Spaces,
            _legacy_agent_panel_scope: None,
            status_indicators: StatusIndicatorStyle::Dots,
            sidebar: SidebarConfig::default(),
            accent: "cyan".into(),
            toast: ToastConfig::default(),
            sound: SoundConfig::default(),
        }
    }
}

impl UiConfig {
    pub fn mouse_scroll_lines(&self) -> usize {
        self.mouse_scroll_lines
            .map(NonZeroUsize::get)
            .unwrap_or(DEFAULT_MOUSE_SCROLL_LINES)
    }

    pub fn right_click_passthrough_modifiers(&self) -> Option<KeyModifiers> {
        self.right_click_passthrough_modifier.modifiers()
    }
}

impl Default for ToastConfig {
    fn default() -> Self {
        Self {
            delivery: ToastDelivery::Off,
            delay_seconds: 1,
            herdr: HerdrToastConfig::default(),
            clipboard: ClipboardToastConfig::default(),
        }
    }
}

impl Default for HerdrToastConfig {
    fn default() -> Self {
        Self {
            position: ToastHerdrPosition::BottomRight,
        }
    }
}

impl Default for ClipboardToastConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            position: ToastClipboardPosition::BottomCenter,
        }
    }
}

impl<'de> Deserialize<'de> for ToastConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct RawToastConfig {
            delivery: Option<ToastDelivery>,
            enabled: Option<bool>,
            delay_seconds: Option<u64>,
            herdr: HerdrToastConfig,
            clipboard: ClipboardToastConfig,
        }

        let raw = RawToastConfig::deserialize(deserializer)?;
        let legacy_delivery = match raw.enabled {
            Some(true) => ToastDelivery::Herdr,
            Some(false) | None => ToastDelivery::Off,
        };
        let delivery = raw.delivery.unwrap_or(legacy_delivery);
        let default = Self::default();
        let delay_seconds = raw.delay_seconds.unwrap_or(default.delay_seconds);
        if delay_seconds > MAX_TOAST_DELAY_SECONDS {
            return Err(de::Error::custom(format!(
                "ui.toast.delay_seconds must be between 0 and {MAX_TOAST_DELAY_SECONDS}"
            )));
        }
        Ok(Self {
            delivery,
            delay_seconds,
            herdr: raw.herdr,
            clipboard: raw.clipboard,
        })
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            headless_cols: crate::config::DEFAULT_HEADLESS_COLS,
            headless_rows: crate::config::DEFAULT_HEADLESS_ROWS,
        }
    }
}

impl Default for AdvancedConfig {
    fn default() -> Self {
        Self {
            scrollback_limit_bytes: DEFAULT_SCROLLBACK_LIMIT_BYTES,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_config_defaults_and_parses() {
        let default_config = Config::default();
        assert_eq!(default_config.update.channel, default_update_channel());
        assert!(default_config.update.version_check);
        assert!(default_config.update.manifest_check);

        let toml = r#"
[update]
channel = "preview"
version_check = false
manifest_check = false
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.update.channel, UpdateChannelConfig::Preview);
        assert_eq!(config.update.channel.as_str(), "preview");
        assert!(!config.update.version_check);
        assert!(!config.update.manifest_check);
    }

    #[test]
    fn update_channel_default_follows_windows_build_identity() {
        assert_eq!(
            default_update_channel_for_build(true, true),
            UpdateChannelConfig::Preview
        );
        assert_eq!(
            default_update_channel_for_build(true, false),
            UpdateChannelConfig::Stable
        );
        assert_eq!(
            default_update_channel_for_build(false, true),
            UpdateChannelConfig::Stable
        );
    }

    #[test]
    fn missing_update_channel_uses_build_default() {
        let empty: Config = toml::from_str("").unwrap();
        let without_update_channel: Config =
            toml::from_str("[update]\nversion_check = false").unwrap();

        assert_eq!(Config::default().update.channel, default_update_channel());
        assert_eq!(empty.update.channel, default_update_channel());
        assert_eq!(
            without_update_channel.update.channel,
            default_update_channel()
        );
    }

    #[test]
    fn terminal_default_shell_defaults_empty_and_parses() {
        let default_config = Config::default();
        assert!(default_config.terminal.default_shell.is_empty());
        assert_eq!(default_config.terminal.shell_mode, ShellModeConfig::Auto);

        let toml = r#"
[terminal]
default_shell = "nu"
shell_mode = "non_login"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.terminal.default_shell, "nu");
        assert_eq!(config.terminal.shell_mode, ShellModeConfig::NonLogin);
    }

    #[test]
    fn terminal_new_cwd_defaults_follow_and_parses() {
        let default_config = Config::default();
        assert_eq!(
            default_config.terminal.new_cwd,
            NewTerminalCwdConfig::Follow
        );

        let config: Config = toml::from_str(
            r#"
[terminal]
new_cwd = "home"
"#,
        )
        .unwrap();
        assert_eq!(config.terminal.new_cwd, NewTerminalCwdConfig::Home);

        let config: Config = toml::from_str(
            r#"
[terminal]
new_cwd = "~/Projects"
"#,
        )
        .unwrap();
        assert_eq!(
            config.terminal.new_cwd,
            NewTerminalCwdConfig::Path("~/Projects".into())
        );
    }

    #[test]
    fn resume_agents_on_restore_defaults_on_and_parses() {
        let default_config = Config::default();
        assert!(default_config.session.resume_agents_on_restore);

        let toml = r#"
[session]
resume_agents_on_restore = false
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(!config.session.resume_agents_on_restore);
    }

    #[test]
    fn agent_panel_sort_config_parses_alias_and_defaults() {
        assert_eq!(
            Config::default().ui.agent_panel_sort,
            AgentPanelSortConfig::Spaces
        );

        let toml = r#"
[ui]
agent_panel_sort = "priority"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.ui.agent_panel_sort, AgentPanelSortConfig::Priority);

        let toml = r#"
[ui]
agent_panel_sort = "workspaces"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.ui.agent_panel_sort, AgentPanelSortConfig::Spaces);

        let toml = r#"
[ui]
agent_panel_scope = "current"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.ui.agent_panel_sort, AgentPanelSortConfig::Spaces);
    }

    #[test]
    fn status_indicator_style_defaults_to_dots_and_parses_symbols() {
        assert_eq!(
            Config::default().ui.status_indicators,
            StatusIndicatorStyle::Dots
        );

        let config: Config = toml::from_str(
            r#"
[ui]
status_indicators = "symbols"
"#,
        )
        .unwrap();
        assert_eq!(config.ui.status_indicators, StatusIndicatorStyle::Symbols);
    }

    #[test]
    fn pane_appearance_defaults_and_parse() {
        let default_config = Config::default();
        assert!(default_config.ui.pane_borders);
        assert!(default_config.ui.pane_outer_borders);
        assert!(default_config.ui.pane_scrollbars);
        assert!(default_config.ui.pane_gaps);
        assert!(!default_config.ui.show_agent_labels_on_pane_borders);
        assert!(!default_config.ui.hide_tab_bar_when_single_tab);
        assert_eq!(
            default_config.ui.tab_bar_position,
            TabBarPositionConfig::Top
        );
        assert!(default_config.ui.tab_bar_right.is_empty());
        assert_eq!(default_config.ui.tab_bar_right_separator, " ");

        let toml = r#"
[ui]
pane_borders = false
pane_outer_borders = false
pane_scrollbars = false
pane_gaps = true
show_agent_labels_on_pane_borders = true
hide_tab_bar_when_single_tab = true
tab_bar_position = "bottom"
tab_bar_right = [
  { type = "zoom" },
  { type = "hostname" },
  { type = "datetime", format = "%H:%M" },
  { type = "text", text = "prod" },
  { type = "command", command = "status.sh", interval_seconds = 10, timeout_seconds = 3 },
]
tab_bar_right_separator = " · "
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(!config.ui.pane_borders);
        assert!(!config.ui.pane_outer_borders);
        assert!(!config.ui.pane_scrollbars);
        assert!(config.ui.pane_gaps);
        assert!(config.ui.show_agent_labels_on_pane_borders);
        assert!(config.ui.hide_tab_bar_when_single_tab);
        assert_eq!(config.ui.tab_bar_position, TabBarPositionConfig::Bottom);
        assert_eq!(config.ui.tab_bar_right.len(), 5);
        assert!(matches!(
            config.ui.tab_bar_right[1],
            TabBarRightEntryConfig::Hostname
        ));
        assert_eq!(config.ui.tab_bar_right_separator, " · ");
    }

    #[test]
    fn worktrees_directory_defaults_and_parses() {
        let default_config = Config::default();
        assert_eq!(default_config.worktrees.directory, "~/.herdr/worktrees");

        let toml = r#"
[worktrees]
directory = "~/Projects/herdr-worktrees"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.worktrees.directory, "~/Projects/herdr-worktrees");
    }

    #[test]
    fn prompt_new_tab_name_defaults_on_and_parses() {
        let default_config = Config::default();
        assert!(default_config.ui.prompt_new_tab_name);

        let toml = r#"
[ui]
prompt_new_tab_name = false
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(!config.ui.prompt_new_tab_name);
    }

    #[test]
    fn prompt_new_workspace_name_defaults_off_and_parses() {
        let default_config = Config::default();
        assert!(!default_config.ui.prompt_new_workspace_name);

        let toml = r#"
[ui]
prompt_new_workspace_name = true
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(config.ui.prompt_new_workspace_name);
    }

    #[test]
    fn reveal_hidden_cursor_for_cjk_ime_default_off_and_parse() {
        let default_config = Config::default();
        assert!(!default_config.experimental.reveal_hidden_cursor_for_cjk_ime);

        let toml = r#"
[experimental]
reveal_hidden_cursor_for_cjk_ime = true
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(config.experimental.reveal_hidden_cursor_for_cjk_ime);
    }

    #[test]
    fn switch_ascii_input_source_in_prefix_default_off_and_parse() {
        let default_config = Config::default();
        assert!(
            !default_config
                .experimental
                .switch_ascii_input_source_in_prefix
        );

        let toml = r#"
[experimental]
switch_ascii_input_source_in_prefix = true
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(config.experimental.switch_ascii_input_source_in_prefix);
    }

    #[test]
    fn cjk_ime_cursor_shape_default_steady_block_and_parse() {
        let default_config = Config::default();
        assert_eq!(
            default_config.experimental.cjk_ime_cursor_shape,
            ImeCursorShape::SteadyBlock
        );

        let toml = r#"
[experimental]
cjk_ime_cursor_shape = "bar"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(
            config.experimental.cjk_ime_cursor_shape,
            ImeCursorShape::Bar
        );
    }

    #[test]
    fn cjk_ime_agents_default_empty_and_parse() {
        let default_config = Config::default();
        assert!(default_config.experimental.cjk_ime_agents.is_empty());

        let toml = r#"
[experimental]
cjk_ime_agents = ["claude", "codex"]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(
            config.experimental.cjk_ime_agents,
            vec!["claude".to_string(), "codex".to_string()]
        );
    }

    #[test]
    fn sidebar_bounds_default_and_parse() {
        let default_config = Config::default();
        assert_eq!(default_config.ui.sidebar_min_width, 18);
        assert_eq!(default_config.ui.sidebar_max_width, 36);
        assert_eq!(
            default_config.ui.mobile_width_threshold,
            DEFAULT_MOBILE_WIDTH_THRESHOLD
        );

        let toml = r#"
[ui]
sidebar_min_width = 12
sidebar_max_width = 80
mobile_width_threshold = 96
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.ui.sidebar_min_width, 12);
        assert_eq!(config.ui.sidebar_max_width, 80);
        assert_eq!(config.ui.mobile_width_threshold, 96);
    }

    #[test]
    fn sidebar_start_collapsed_defaults_off_and_parses_on() {
        let default_config = Config::default();
        assert!(!default_config.ui.sidebar_start_collapsed);

        let toml = r#"
[ui]
sidebar_start_collapsed = true
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(config.ui.sidebar_start_collapsed);
    }

    #[test]
    fn sidebar_collapsed_mode_defaults_compact_and_parses_hidden() {
        let default_config = Config::default();
        assert_eq!(
            default_config.ui.sidebar_collapsed_mode,
            SidebarCollapsedModeConfig::Compact
        );

        let toml = r#"
[ui]
sidebar_collapsed_mode = "hidden"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(
            config.ui.sidebar_collapsed_mode,
            SidebarCollapsedModeConfig::Hidden
        );
    }

    #[test]
    fn validated_sidebar_bounds_rejects_inverted() {
        assert_eq!(validated_sidebar_bounds(18, 36), Some((18, 36)));
        assert_eq!(validated_sidebar_bounds(20, 20), Some((20, 20)));
        assert_eq!(validated_sidebar_bounds(0, u16::MAX), Some((0, u16::MAX)));
        assert_eq!(validated_sidebar_bounds(50, 30), None);
        assert_eq!(validated_sidebar_bounds(u16::MAX, 0), None);
    }

    #[test]
    fn mouse_capture_default_on_and_parse() {
        let default_config = Config::default();
        assert!(default_config.ui.mouse_capture);

        let toml = r#"
[ui]
mouse_capture = false
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(!config.ui.mouse_capture);
    }

    #[test]
    fn copy_on_select_default_on_and_parse() {
        let default_config = Config::default();
        assert!(default_config.ui.copy_on_select);

        let toml = r#"
[ui]
copy_on_select = false
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(!config.ui.copy_on_select);
    }

    #[test]
    fn right_click_passthrough_modifier_defaults_off_and_parses() {
        let default_config = Config::default();
        assert_eq!(default_config.ui.right_click_passthrough_modifiers(), None);

        for value in ["", "off", "none", "disabled"] {
            let toml = format!(
                r#"
[ui]
right_click_passthrough_modifier = "{value}"
"#
            );
            let config: Config = toml::from_str(&toml).unwrap();
            assert_eq!(
                config.ui.right_click_passthrough_modifiers(),
                None,
                "value {value:?} should disable passthrough"
            );
        }

        for (value, expected) in [
            ("ctrl", KeyModifiers::CONTROL),
            ("control", KeyModifiers::CONTROL),
            ("alt", KeyModifiers::ALT),
            ("option", KeyModifiers::ALT),
            ("cmd", KeyModifiers::SUPER),
            ("command", KeyModifiers::SUPER),
            ("super", KeyModifiers::SUPER),
            ("meta", KeyModifiers::META),
            ("hyper", KeyModifiers::HYPER),
        ] {
            let toml = format!(
                r#"
[ui]
right_click_passthrough_modifier = "{value}"
"#
            );
            let config: Config = toml::from_str(&toml).unwrap();
            assert_eq!(
                config.ui.right_click_passthrough_modifiers(),
                Some(expected),
                "value {value:?} should parse"
            );
        }

        let toml = r#"
[ui]
right_click_passthrough_modifier = "cmd+alt"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(
            config.ui.right_click_passthrough_modifiers(),
            Some(KeyModifiers::SUPER | KeyModifiers::ALT)
        );
    }

    #[test]
    fn right_click_passthrough_modifier_rejects_shift() {
        for value in ["shift", "shift+ctrl", "ctrl+", "ctrl++alt", "banana"] {
            let toml = format!(
                r#"
[ui]
right_click_passthrough_modifier = "{value}"
"#
            );
            assert!(
                toml::from_str::<Config>(&toml).is_err(),
                "value {value:?} should be rejected"
            );
        }
    }

    #[test]
    fn redraw_on_focus_gained_default_on_and_parse() {
        let default_config = Config::default();
        assert!(default_config.ui.redraw_on_focus_gained);

        let toml = r#"
[ui]
redraw_on_focus_gained = false
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(!config.ui.redraw_on_focus_gained);
    }

    #[test]
    fn mouse_scroll_lines_defaults_to_three_and_parses() {
        let default_config = Config::default();
        assert_eq!(
            default_config.ui.mouse_scroll_lines(),
            DEFAULT_MOUSE_SCROLL_LINES
        );

        let toml = r#"
[ui]
mouse_scroll_lines = 1
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.ui.mouse_scroll_lines(), 1);
    }

    #[test]
    fn mouse_scroll_lines_rejects_zero() {
        let toml = r#"
[ui]
mouse_scroll_lines = 0
"#;
        assert!(toml::from_str::<Config>(toml).is_err());
    }

    #[test]
    fn toast_config_parses() {
        let toml = r#"
[ui.toast]
delivery = "terminal"
delay_seconds = 2

[ui.toast.herdr]
position = "top-left"

[ui.toast.clipboard]
enabled = false
position = "top-center"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.ui.toast.delivery, ToastDelivery::Terminal);
        assert_eq!(config.ui.toast.delay_seconds, 2);
        assert_eq!(config.ui.toast.herdr.position, ToastHerdrPosition::TopLeft);
        assert!(!config.ui.toast.clipboard.enabled);
        assert_eq!(
            config.ui.toast.clipboard.position,
            ToastClipboardPosition::TopCenter
        );
    }

    #[test]
    fn toast_config_defaults_preserve_existing_behavior_with_delay() {
        let config = Config::default();
        assert_eq!(config.ui.toast.delivery, ToastDelivery::Off);
        assert_eq!(config.ui.toast.delay_seconds, 1);
        assert_eq!(
            config.ui.toast.herdr.position,
            ToastHerdrPosition::BottomRight
        );
        assert!(config.ui.toast.clipboard.enabled);
        assert_eq!(
            config.ui.toast.clipboard.position,
            ToastClipboardPosition::BottomCenter
        );
    }

    #[test]
    fn toast_config_parses_system_delivery() {
        let toml = r#"
[ui.toast]
delivery = "system"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.ui.toast.delivery, ToastDelivery::System);
    }

    #[test]
    fn toast_config_legacy_enabled_true_maps_to_herdr() {
        let toml = r#"
[ui.toast]
enabled = true
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.ui.toast.delivery, ToastDelivery::Herdr);
    }

    #[test]
    fn toast_config_legacy_enabled_false_maps_to_off() {
        let toml = r#"
[ui.toast]
enabled = false
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.ui.toast.delivery, ToastDelivery::Off);
    }

    #[test]
    fn toast_config_delivery_wins_over_legacy_enabled() {
        let toml = r#"
[ui.toast]
enabled = true
delivery = "terminal"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.ui.toast.delivery, ToastDelivery::Terminal);
    }

    #[test]
    fn toast_config_rejects_unbounded_delay() {
        let toml = format!(
            r#"
[ui.toast]
delay_seconds = {}
"#,
            MAX_TOAST_DELAY_SECONDS + 1
        );

        let error = toml::from_str::<Config>(&toml).unwrap_err().to_string();

        assert!(error.contains("ui.toast.delay_seconds must be between 0 and 3600"));
    }

    #[test]
    fn missing_onboarding_shows_setup() {
        let config = Config::default();
        assert!(config.should_show_onboarding());
    }

    #[test]
    fn onboarding_false_skips_setup() {
        let config: Config = toml::from_str("onboarding = false").unwrap();
        assert!(!config.should_show_onboarding());
    }

    #[test]
    fn server_headless_size_defaults_and_parses() {
        let default_config = Config::default();
        assert_eq!(
            default_config.server.headless_cols,
            crate::config::DEFAULT_HEADLESS_COLS
        );
        assert_eq!(
            default_config.server.headless_rows,
            crate::config::DEFAULT_HEADLESS_ROWS
        );

        let config: Config = toml::from_str(
            r#"[server]
headless_cols = 160
headless_rows = 50
"#,
        )
        .unwrap();
        assert_eq!(config.server.headless_cols, 160);
        assert_eq!(config.server.headless_rows, 50);

        let invalid: Config = toml::from_str(
            r#"[server]
headless_cols = 0
headless_rows = 50
"#,
        )
        .unwrap();
        assert!(invalid.invalid_headless_size_diagnostic().is_some());
        assert_eq!(
            invalid.headless_size(),
            (
                crate::config::DEFAULT_HEADLESS_COLS,
                crate::config::DEFAULT_HEADLESS_ROWS
            )
        );
    }

    #[test]
    fn advanced_defaults_include_scrollback_limit_bytes() {
        let config = Config::default();
        assert_eq!(
            config.advanced.scrollback_limit_bytes,
            DEFAULT_SCROLLBACK_LIMIT_BYTES
        );
    }

    #[test]
    fn pane_history_persistence_is_opt_in() {
        assert!(!Config::default().experimental.pane_history);

        let toml = r#"
[experimental]
pane_history = true
"#;
        let config: Config = toml::from_str(toml).unwrap();

        assert!(config.experimental.pane_history);
    }

    #[test]
    fn kitty_graphics_default_on_with_stable_opt_out() {
        assert!(Config::default().kitty_graphics_enabled());

        let config: Config = toml::from_str(
            r#"
[terminal]
kitty_graphics = false
"#,
        )
        .unwrap();
        assert!(!config.kitty_graphics_enabled());
    }

    #[test]
    fn legacy_experimental_kitty_graphics_setting_remains_compatible() {
        let disabled: Config = toml::from_str(
            r#"
[experimental]
kitty_graphics = false
"#,
        )
        .unwrap();
        assert!(!disabled.kitty_graphics_enabled());

        let stable_setting_wins: Config = toml::from_str(
            r#"
[terminal]
kitty_graphics = false

[experimental]
kitty_graphics = true
"#,
        )
        .unwrap();
        assert!(!stable_setting_wins.kitty_graphics_enabled());
    }

    #[test]
    fn experimental_config_parses() {
        let toml = r#"
[experimental]
allow_nested = true
kitty_graphics = true
pane_history = true
switch_ascii_input_source_in_prefix = true
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(config.experimental.allow_nested);
        assert_eq!(config.experimental.kitty_graphics, Some(true));
        assert!(config.kitty_graphics_enabled());
        assert!(config.experimental.pane_history);
        assert!(config.experimental.switch_ascii_input_source_in_prefix);
    }

    #[test]
    fn advanced_config_parses() {
        let toml = r#"
[advanced]
scrollback_limit_bytes = 12345
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.advanced.scrollback_limit_bytes, 12345);
    }

    #[test]
    fn advanced_legacy_scrollback_lines_alias_parses() {
        let toml = r#"
[advanced]
scrollback_lines = 12345
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.advanced.scrollback_limit_bytes, 12345);
    }

    fn fleet_config(toml: &str) -> Config {
        toml::from_str::<Config>(toml).expect("fleet fixture parses")
    }

    fn fleet_diagnostics(toml: &str) -> Vec<String> {
        fleet_config(toml).fleet.diagnostics()
    }

    #[test]
    fn fleet_config_defaults_when_the_section_is_absent() {
        for config in [Config::default(), fleet_config("[advanced]\n")] {
            assert!(config.fleet.include_local);
            assert!(config.fleet.hosts.is_empty());
            assert!(config.fleet.diagnostics().is_empty());
        }
    }

    #[test]
    fn fleet_config_parses_a_full_multi_host_sample_without_diagnostics() {
        let config = fleet_config(
            r#"
[fleet]
include_local = false

[[fleet.hosts]]
name = "workbox"
kind = "ssh"
target = "ssh://workbox:2222"
session = "agents"
enabled = true

[[fleet.hosts]]
name = "side"
kind = "local"
session = "side"
enabled = false
"#,
        );

        assert!(!config.fleet.include_local);
        assert_eq!(config.fleet.hosts.len(), 2);
        assert_eq!(config.fleet.hosts[0].name, "workbox");
        assert_eq!(config.fleet.hosts[0].kind, FleetHostKind::Ssh);
        assert_eq!(
            config.fleet.hosts[0].target.as_deref(),
            Some("ssh://workbox:2222")
        );
        assert_eq!(config.fleet.hosts[0].session.as_deref(), Some("agents"));
        assert!(config.fleet.hosts[0].enabled);
        assert_eq!(config.fleet.hosts[1].kind, FleetHostKind::Local);
        assert_eq!(config.fleet.hosts[1].target, None);
        assert!(!config.fleet.hosts[1].enabled);
        assert_eq!(config.fleet.diagnostics(), Vec::<String>::new());
    }

    #[test]
    fn fleet_host_entry_defaults_are_ssh_enabled_and_unnamed() {
        let config = fleet_config("[[fleet.hosts]]\ntarget = \"workbox\"\n");
        let host = &config.fleet.hosts[0];
        assert_eq!(host.name, "");
        assert_eq!(host.kind, FleetHostKind::Ssh);
        assert_eq!(host.session, None);
        assert!(host.enabled);
    }

    #[test]
    fn fleet_diagnostic_reports_a_missing_or_invalid_host_name() {
        assert_eq!(
            fleet_diagnostics("[[fleet.hosts]]\ntarget = \"workbox\"\n"),
            vec![
                "invalid fleet host name: fleet.hosts[0].name = \"\"; host name cannot be empty; \
                 ignoring [fleet] hosts"
                    .to_string()
            ]
        );

        for name in ["bad/name", "has space", "..", "n\u{e4}me"] {
            let diagnostics = fleet_diagnostics(&format!(
                "[[fleet.hosts]]\nname = {name:?}\ntarget = \"t\"\n"
            ));
            assert_eq!(diagnostics.len(), 1, "{name:?}: {diagnostics:?}");
            assert!(
                diagnostics[0].starts_with("invalid fleet host name: fleet.hosts[0].name = "),
                "{name:?}: {diagnostics:?}"
            );
            assert!(
                !diagnostics[0].contains("session name"),
                "host diagnostics must not talk about sessions: {diagnostics:?}"
            );
        }

        let long = "a".repeat(65);
        let diagnostics = fleet_diagnostics(&format!(
            "[[fleet.hosts]]\nname = \"{long}\"\ntarget = \"t\"\n"
        ));
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(diagnostics[0].contains("host name cannot be longer than"));
    }

    #[test]
    fn fleet_diagnostic_reports_duplicate_host_names() {
        let diagnostics = fleet_diagnostics(
            r#"
[[fleet.hosts]]
name = "workbox"
target = "workbox"

[[fleet.hosts]]
name = "workbox"
target = "other"
"#,
        );
        assert_eq!(
            diagnostics,
            vec![
                "duplicate fleet host name: fleet.hosts[1].name = \"workbox\"; host names must be \
                 unique; ignoring [fleet] hosts"
                    .to_string()
            ]
        );
    }

    #[test]
    fn fleet_diagnostic_reports_the_reserved_local_name_only_while_include_local() {
        let reserved = fleet_diagnostics(
            "[[fleet.hosts]]\nname = \"local\"\nkind = \"local\"\nsession = \"agents\"\n",
        );
        assert_eq!(reserved.len(), 1, "{reserved:?}");
        assert!(
            reserved[0].starts_with("reserved fleet host name: fleet.hosts[0].name = \"local\""),
            "{reserved:?}"
        );
        assert!(reserved[0].contains("fleet.include_local = false"));

        assert!(fleet_diagnostics(
            "[fleet]\ninclude_local = false\n\n[[fleet.hosts]]\nname = \"local\"\nkind = \"local\"\nsession = \"agents\"\n",
        )
        .is_empty());
    }

    #[test]
    fn fleet_diagnostic_reports_ssh_target_problems() {
        assert_eq!(
            fleet_diagnostics("[[fleet.hosts]]\nname = \"workbox\"\n"),
            vec![
                "missing fleet host target: fleet.hosts[0].target is required for kind = \"ssh\"; \
                 ignoring [fleet] hosts"
                    .to_string()
            ]
        );

        for (target, reason) in [
            ("", "ssh target cannot be empty"),
            ("-oProxyCommand=id", "ssh target must not start with '-'"),
            (
                " -oProxyCommand=id",
                "ssh target must not contain whitespace or control characters",
            ),
            (
                "   ",
                "ssh target must not contain whitespace or control characters",
            ),
            (
                "workbox\nProxyCommand id",
                "ssh target must not contain whitespace or control characters",
            ),
        ] {
            let diagnostics = fleet_diagnostics(&format!(
                "[[fleet.hosts]]\nname = \"workbox\"\ntarget = {target:?}\n"
            ));
            assert_eq!(diagnostics.len(), 1, "{target:?}: {diagnostics:?}");
            assert!(
                diagnostics[0].starts_with("invalid fleet host target: fleet.hosts[0].target = "),
                "{target:?}: {diagnostics:?}"
            );
            assert!(diagnostics[0].contains(reason), "{diagnostics:?}");
        }

        for target in ["workbox", "user@host", "ssh://host:2222", "10.0.0.2"] {
            assert!(
                fleet_diagnostics(&format!(
                    "[[fleet.hosts]]\nname = \"workbox\"\ntarget = {target:?}\n"
                ))
                .is_empty(),
                "{target:?} is a valid ssh destination"
            );
        }
    }

    #[test]
    fn fleet_diagnostic_reports_local_host_session_problems() {
        assert_eq!(
            fleet_diagnostics("[[fleet.hosts]]\nname = \"side\"\nkind = \"local\"\n"),
            vec![
                "missing fleet host session: fleet.hosts[0].session is required for kind = \
                 \"local\"; ignoring [fleet] hosts"
                    .to_string()
            ]
        );

        let diagnostics = fleet_diagnostics(
            "[[fleet.hosts]]\nname = \"side\"\nkind = \"local\"\nsession = \"side\"\ntarget = \"workbox\"\n",
        );
        assert_eq!(
            diagnostics,
            vec![
                "unused fleet host target: fleet.hosts[0].target is only used for kind = \"ssh\"; \
                 remove it or set kind = \"ssh\"; ignoring [fleet] hosts"
                    .to_string()
            ]
        );
    }

    #[test]
    fn fleet_diagnostic_reports_an_invalid_session_name_for_either_kind() {
        for kind in ["ssh", "local"] {
            let diagnostics = fleet_diagnostics(&format!(
                "[[fleet.hosts]]\nname = \"workbox\"\nkind = {kind:?}\ntarget = \"workbox\"\nsession = \"bad name\"\n"
            ));
            assert!(
                diagnostics.iter().any(|diagnostic| {
                    diagnostic.starts_with(
                        "invalid fleet host session: fleet.hosts[0].session = \"bad name\"",
                    )
                }),
                "{kind}: {diagnostics:?}"
            );
        }

        let diagnostics = fleet_diagnostics(
            "[[fleet.hosts]]\nname = \"workbox\"\ntarget = \"workbox\"\nsession = \"\"\n",
        );
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(diagnostics[0].contains("session name cannot be empty"));
    }

    #[test]
    fn fleet_diagnostics_still_validate_disabled_hosts_and_stay_ordered() {
        let diagnostics = fleet_diagnostics(
            r#"
[[fleet.hosts]]
name = "workbox"
enabled = false

[[fleet.hosts]]
name = "bad/name"
target = "other"
"#,
        );
        assert_eq!(diagnostics.len(), 2, "{diagnostics:?}");
        assert!(diagnostics[0].contains("fleet.hosts[0].target"));
        assert!(diagnostics[1].contains("fleet.hosts[1].name"));
    }

    #[test]
    fn collect_diagnostics_includes_fleet_diagnostics() {
        let config = fleet_config(
            r#"
[[fleet.hosts]]
name = "workbox"
target = "workbox"

[[fleet.hosts]]
name = "workbox"
target = "workbox"
"#,
        );
        assert_eq!(config.collect_diagnostics(), config.fleet.diagnostics());
        assert!(Config::default().collect_diagnostics().is_empty());
    }
}

#[cfg(test)]
mod gateway_tests {
    use super::*;

    fn gateway(toml: &str) -> GatewayConfig {
        toml::from_str(toml).expect("gateway section parses")
    }

    #[test]
    fn defaults_are_loopback_and_clean() {
        let config = GatewayConfig::default();
        assert_eq!(config.bind, "127.0.0.1:7788");
        assert_eq!(
            config.bind_addr().map(|addr| addr.port()),
            Some(7788),
            "the default bind must parse"
        );
        assert!(config.allowed_origins.is_empty());
        assert!(config.public_url.is_empty());
        assert_eq!(config.auth_failure_limit, 5);
        assert_eq!(config.auth_failure_window_secs, 60);
        assert_eq!(config.pairing_ttl_secs, 600);
        assert!(
            config.diagnostics().is_empty(),
            "{:?}",
            config.diagnostics()
        );
        assert!(
            Config::default().gateway.diagnostics().is_empty(),
            "an absent [gateway] section is valid"
        );
    }

    #[test]
    fn every_bad_value_is_reported_once() {
        let config = gateway(
            r#"
bind = "127.0.0.1"
allowed_origins = ["https://ok.test", "http://x/path", "https://ok.test", "ftp://x.test"]
public_url = "https://user@x.test"
auth_failure_limit = 0
auth_failure_window_secs = 0
pairing_ttl_secs = 5
"#,
        );
        let diagnostics = config.diagnostics();
        assert_eq!(diagnostics.len(), 6, "{diagnostics:#?}");
        assert!(diagnostics[0].contains("gateway.bind"), "{diagnostics:?}");
        assert!(
            diagnostics[1].contains("gateway.allowed_origins[1]")
                && diagnostics[1].contains("path"),
            "{diagnostics:?}"
        );
        assert!(
            diagnostics[2].contains("gateway.allowed_origins[2]")
                && diagnostics[2].contains("duplicate"),
            "{diagnostics:?}"
        );
        assert!(
            diagnostics[3].contains("gateway.allowed_origins[3]")
                && diagnostics[3].contains("scheme"),
            "{diagnostics:?}"
        );
        assert!(
            diagnostics[4].contains("gateway.public_url") && diagnostics[4].contains("userinfo"),
            "{diagnostics:?}"
        );
        // The two zero limits and the out-of-range ttl are folded into the
        // remaining lines; assert on the keys so wording can change.
        let rest = diagnostics[5..].join("\n");
        assert!(rest.contains("pairing_ttl_secs"), "{rest}");

        let config = gateway("auth_failure_limit = 0\nauth_failure_window_secs = 0\n");
        let diagnostics = config.diagnostics();
        assert_eq!(diagnostics.len(), 2, "{diagnostics:?}");
        assert!(diagnostics[0].contains("auth_failure_limit"));
        assert!(diagnostics[1].contains("auth_failure_window_secs"));
    }

    #[test]
    fn the_pairing_ttl_has_documented_bounds() {
        assert!(gateway("pairing_ttl_secs = 30").diagnostics().is_empty());
        assert!(gateway("pairing_ttl_secs = 86400").diagnostics().is_empty());
        assert_eq!(gateway("pairing_ttl_secs = 29").diagnostics().len(), 1);
        assert_eq!(gateway("pairing_ttl_secs = 86401").diagnostics().len(), 1);
        assert_eq!(gateway("pairing_ttl_secs = 0").diagnostics().len(), 1);
    }

    #[test]
    fn gateway_diagnostics_reach_collect_diagnostics() {
        let config: Config = toml::from_str("[gateway]\nbind = \"nope\"\n").expect("config parses");
        let diagnostics = config.collect_diagnostics();
        assert_eq!(diagnostics, config.gateway.diagnostics());
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    }

    #[test]
    fn origins_parse_scheme_host_and_optional_port() {
        let origin = parse_gateway_origin("HTTPS://Fleet.Example.TS.net").expect("parses");
        assert_eq!(origin.scheme, "https");
        assert_eq!(origin.host, "fleet.example.ts.net");
        assert_eq!(origin.port, None);
        assert_eq!(origin.to_header_value(), "https://fleet.example.ts.net");

        let origin = parse_gateway_origin("http://127.0.0.1:7788").expect("parses");
        assert_eq!(origin.port, Some(7788));
        assert_eq!(origin.to_header_value(), "http://127.0.0.1:7788");

        let origin = parse_gateway_origin("http://[::1]:7788").expect("parses");
        assert_eq!(origin.host, "[::1]");
        assert_eq!(origin.port, Some(7788));
        assert_eq!(origin.to_header_value(), "http://[::1]:7788");
        assert_eq!(
            parse_gateway_origin("http://[::1]").expect("parses").host,
            "[::1]"
        );
    }

    #[test]
    fn origins_reject_everything_a_browser_never_sends() {
        for value in [
            "",
            "null",
            "example.test",
            "http://",
            "https://example.test/",
            "https://example.test/app",
            "https://example.test?q=1",
            "https://example.test#f",
            "https://user:pw@example.test",
            "ftp://example.test",
            "file://example.test",
            "https://example.test:70000",
            "https://example.test:",
            "https://example.test:https",
            "https://::1:7788",
            "https://[::1:7788",
            "https://[not-an-address]:1",
            "https://exa mple.test",
        ] {
            assert!(
                parse_gateway_origin(value).is_err(),
                "{value:?} must be refused"
            );
        }
    }
}
