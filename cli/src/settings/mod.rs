//! User-facing shell settings and keybinding configuration.

use std::{
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use serde::Deserialize;
use turtletap::{
    Chrome, KeyBinding, KeyCode, KeyModifiers, ShellBindings, ShellConfig, Theme,
    tui::style::{Color, Style},
};

use crate::keybindings::BindingAction;

mod kdl;
mod toml;

use self::kdl::{KDL_TEMPLATE, parse_kdl, print_resolved_kdl};
use self::toml::{TOML_TEMPLATE, print_resolved_toml};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConfigFormat {
    Kdl,
    Toml,
}

impl ConfigFormat {
    fn parse(value: &str) -> io::Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "kdl" => Ok(Self::Kdl),
            "toml" => Ok(Self::Toml),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown configuration format '{value}'; expected kdl or toml"),
            )),
        }
    }

    const fn extension(self) -> &'static str {
        match self {
            Self::Kdl => "kdl",
            Self::Toml => "toml",
        }
    }

    const fn template(self) -> &'static str {
        match self {
            Self::Kdl => KDL_TEMPLATE,
            Self::Toml => TOML_TEMPLATE,
        }
    }
}

struct ConfigLocation {
    path: PathBuf,
    format: ConfigFormat,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct SettingsFile {
    shell: ShellSettings,
    theme: ThemeSettings,
    bindings: BindingSettings,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ShellSettings {
    mouse_capture: Option<bool>,
    direct_detach: Option<bool>,
    tick_rate_ms: Option<u64>,
    chrome: Option<String>,
    rail_width: Option<u16>,
    rail_narrow: Option<u16>,
    rail_min_content: Option<u16>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ThemeSettings {
    chrome: Option<String>,
    muted: Option<String>,
    selected: SelectedThemeSettings,
    accent: Option<String>,
    working: Option<String>,
    attention: Option<String>,
    failed: Option<String>,
    complete: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct SelectedThemeSettings {
    foreground: Option<String>,
    background: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct BindingSettings {
    leaders: Option<Vec<String>>,
    palette: Option<Vec<String>>,
    redraw: Option<Vec<String>>,
    next_screen: Option<Vec<String>>,
    previous_screen: Option<Vec<String>>,
    jump_modifiers: Option<Vec<String>>,
    shell_detach: Option<Vec<String>>,
    shell_next_screen: Option<Vec<String>>,
    shell_previous_screen: Option<Vec<String>>,
    shell_help: Option<Vec<String>>,
    leader_palette: Option<Vec<String>>,
    leader_next_screen: Option<Vec<String>>,
    leader_previous_screen: Option<Vec<String>>,
    leader_scroll_up: Option<Vec<String>>,
    leader_scroll_down: Option<Vec<String>>,
    leader_close: Option<Vec<String>>,
    leader_detach: Option<Vec<String>>,
    leader_help: Option<Vec<String>>,
    leader_jump_modifiers: Option<Vec<String>>,
    action_next_screen: Option<Vec<String>>,
    action_previous_screen: Option<Vec<String>>,
    action_scroll_up: Option<Vec<String>>,
    action_scroll_down: Option<Vec<String>>,
    action_close: Option<Vec<String>>,
    action_detach: Option<Vec<String>>,
    action_help: Option<Vec<String>>,
    action_clear_query: Option<Vec<String>>,
    action_jump_modifiers: Option<Vec<String>>,
    session_release_driver: Option<Vec<String>>,
    session_take_driver: Option<Vec<String>>,
    session_clear: Option<Vec<String>>,
    session_interrupt: Option<Vec<String>>,
    session_detach: Option<Vec<String>>,
    session_delete_to_start: Option<Vec<String>>,
    session_word_left: Option<Vec<String>>,
    session_word_right: Option<Vec<String>>,
    session_line_start: Option<Vec<String>>,
    session_line_end: Option<Vec<String>>,
    session_delete_word: Option<Vec<String>>,
    session_complete: Option<Vec<String>>,
    session_scroll_up: Option<Vec<String>>,
    session_scroll_down: Option<Vec<String>>,
    session_scroll_top: Option<Vec<String>>,
    session_scroll_bottom: Option<Vec<String>>,
    dashboard_up: Option<Vec<String>>,
    dashboard_down: Option<Vec<String>>,
    dashboard_view: Option<Vec<String>>,
    dashboard_take: Option<Vec<String>>,
    dashboard_search: Option<Vec<String>>,
    dashboard_new: Option<Vec<String>>,
    dashboard_rename: Option<Vec<String>>,
    dashboard_delete: Option<Vec<String>>,
    dashboard_stop: Option<Vec<String>>,
    dashboard_keybindings: Option<Vec<String>>,
    dashboard_close: Option<Vec<String>>,
}

pub(crate) fn shell_config(title: &str) -> io::Result<ShellConfig> {
    let location = active_location()?;
    let settings = read_settings(&location)?;
    resolve(title, settings)
}

pub(crate) fn validate_binding_set(bindings: &ShellBindings) -> io::Result<()> {
    validate_bindings(bindings)
}

pub(crate) fn canonical_binding(binding: KeyBinding) -> io::Result<KeyBinding> {
    let label = binding.label();
    let parsed = parse_key_binding(&label).map_err(|message| {
        invalid(format!(
            "this terminal key cannot be stored as a TurtleTap binding: {message}"
        ))
    })?;
    let normalized_code = match binding.code {
        KeyCode::Char(character) => KeyCode::Char(character.to_ascii_lowercase()),
        code => code,
    };
    if parsed != KeyBinding::new(normalized_code, binding.modifiers) {
        return Err(invalid(format!(
            "{} uses terminal modifiers TurtleTap cannot store",
            binding.label()
        )));
    }
    Ok(parsed)
}

pub(crate) fn save_binding(action: BindingAction, binding: KeyBinding) -> io::Result<ShellConfig> {
    let location = active_location()?;
    let source = match fs::read_to_string(&location.path) {
        Ok(source) => source,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            location.format.template().to_owned()
        }
        Err(error) => return Err(error),
    };
    let label = binding.label().to_ascii_lowercase();
    let updated = match location.format {
        ConfigFormat::Kdl => update_kdl_binding(&source, action.id(), &label, &location.path)?,
        ConfigFormat::Toml => update_toml_binding(&source, action.id(), &label, &location.path)?,
    };
    let settings = parse_settings_source(&updated, &location)?;
    let config = resolve("TurtleTap keybindings", settings)?;
    atomic_write(&location.path, updated.as_bytes())?;
    write_active_format(location.format)?;
    Ok(config)
}

fn parse_settings_source(source: &str, location: &ConfigLocation) -> io::Result<SettingsFile> {
    match location.format {
        ConfigFormat::Kdl => parse_kdl(source, &location.path),
        ConfigFormat::Toml => ::toml::from_str(source).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "invalid configuration at {}: {error}",
                    location.path.display()
                ),
            )
        }),
    }
}

fn update_kdl_binding(source: &str, name: &str, label: &str, path: &Path) -> io::Result<String> {
    let mut document = source.parse::<::kdl::KdlDocument>().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid configuration at {}: {error}", path.display()),
        )
    })?;
    if document.get("bindings").is_none() {
        document.nodes_mut().push(::kdl::KdlNode::new("bindings"));
    }
    let bindings = document
        .get_mut("bindings")
        .ok_or_else(|| invalid("could not create bindings section"))?;
    let children = bindings.ensure_children();
    if children.get(name).is_none() {
        children.nodes_mut().push(::kdl::KdlNode::new(name));
    }
    let node = children
        .get_mut(name)
        .ok_or_else(|| invalid(format!("could not create bindings child '{name}'")))?;
    node.clear_entries();
    node.push(label);
    node.fmt();
    Ok(document.to_string())
}

fn update_toml_binding(source: &str, name: &str, label: &str, path: &Path) -> io::Result<String> {
    let mut document = source.parse::<toml_edit::DocumentMut>().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid configuration at {}: {error}", path.display()),
        )
    })?;
    if !document.as_table().contains_key("bindings") {
        document["bindings"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    if !document["bindings"].is_table() {
        return Err(invalid("bindings must be a TOML table"));
    }
    let key = name.replace('-', "_");
    let decor = document["bindings"][&key]
        .as_value()
        .map(|value| value.decor().clone());
    let mut values = toml_edit::Array::new();
    values.push(label);
    let mut value = toml_edit::Value::Array(values);
    if let Some(decor) = decor {
        *value.decor_mut() = decor;
    }
    document["bindings"][&key] = toml_edit::Item::Value(value);
    Ok(document.to_string())
}

fn atomic_write(path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "configuration path has no parent",
        )
    })?;
    fs::create_dir_all(parent)?;
    let permissions = fs::metadata(path)
        .ok()
        .map(|metadata| metadata.permissions());
    let mut temporary = None;
    for attempt in 0..100_u8 {
        let candidate = parent.join(format!(
            ".turtletap-config-{}-{attempt}.tmp",
            std::process::id()
        ));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => {
                temporary = Some((candidate, file));
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    let (temporary_path, mut file) = temporary.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not reserve a temporary configuration file",
        )
    })?;
    let result = (|| {
        file.write_all(contents)?;
        file.sync_all()?;
        if let Some(permissions) = permissions {
            fs::set_permissions(&temporary_path, permissions)?;
        }
        fs::rename(&temporary_path, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result
}

pub(crate) fn command(
    arguments: &[String],
    output: crate::commands::OutputFormat,
) -> io::Result<()> {
    match arguments {
        [] => show_config(None),
        [help] if matches!(help.as_str(), "-h" | "--help" | "help") => print_help(None),
        [action, help] if matches!(help.as_str(), "-h" | "--help") => print_help(Some(action)),
        [help, action] if help == "help" => print_help(Some(action)),
        [show] if show == "show" => show_config(None),
        [show, format] if show == "show" => show_config(Some(ConfigFormat::parse(format)?)),
        [path] if path == "path" => {
            let path = active_location()?.path;
            if output == crate::commands::OutputFormat::Json {
                crate::commands::print_json(&serde_json::json!({ "path": path }))
            } else {
                println!("{}", path.display());
                Ok(())
            }
        }
        [check] if check == "check" => {
            let location = active_location()?;
            let _ = shell_config("TurtleTap")?;
            if output == crate::commands::OutputFormat::Json {
                crate::commands::print_json(&serde_json::json!({
                    "valid": true,
                    "path": location.path,
                    "exists": location.path.exists(),
                    "source": if location.path.exists() {
                        "configuration file"
                    } else {
                        "built-in defaults"
                    },
                }))
            } else if location.path.exists() {
                println!("Configuration is valid: {}", location.path.display());
                Ok(())
            } else {
                println!(
                    "No configuration file at {}; built-in defaults are valid.",
                    location.path.display()
                );
                Ok(())
            }
        }
        [init] if init == "init" => init_config(ConfigFormat::Kdl, false, output),
        [init, format] if init == "init" => {
            init_config(ConfigFormat::parse(format)?, false, output)
        }
        [init, format, activate] if init == "init" && activate == "--activate" => {
            init_config(ConfigFormat::parse(format)?, true, output)
        }
        [edit] if edit == "edit" => edit_config(output),
        [reload] if reload == "reload" => reload_config(output),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: turtletap config [show [kdl|toml]|path|check|init [kdl|toml]]",
        )),
    }
}

pub(crate) fn print_help(action: Option<&str>) -> io::Result<()> {
    let help = match action {
        None => {
            "Show, validate, or initialize TurtleTap settings.\n\n\
             Usage:\n  turtletap config [action]\n\n\
             Actions:\n  show [kdl|toml]  Print resolved settings; optionally translate formats\n  path             Print the active configuration path\n  check            Validate the active configuration\n  init [kdl|toml]  Create/select a starter file; defaults to KDL\n  edit             Edit and validate the active file\n  reload           Validate settings and live-reload support\n\n\
             Environment:\n  TURTLETAP_CONFIG  Use an explicit .kdl or .toml configuration file\n\n\
             Examples:\n  turtletap config show\n  turtletap config init toml\n  turtletap config check"
        }
        Some("show") => {
            "Print the resolved configuration, including built-in defaults.\n\n\
             Usage:\n  turtletap config show [kdl|toml]\n\n\
             The optional format translates the resolved settings for stdout."
        }
        Some("path") => {
            "Print the active configuration path.\n\n\
             Usage:\n  turtletap config path"
        }
        Some("check") => {
            "Validate configuration syntax, names, values, and binding conflicts.\n\n\
             Usage:\n  turtletap config check"
        }
        Some("init") => {
            "Create or select a commented starter configuration without overwriting a file.\n\n\
             Usage:\n  turtletap config init [kdl|toml] [--activate]\n\n\
             KDL is used when the format is omitted. --activate selects an existing candidate."
        }
        Some(unknown) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown config action '{unknown}'"),
            ));
        }
    };
    println!("{help}");
    Ok(())
}

fn show_config(output_format: Option<ConfigFormat>) -> io::Result<()> {
    let location = active_location()?;
    let config = shell_config("TurtleTap")?;
    let format = output_format.unwrap_or(location.format);
    let comment = if format == ConfigFormat::Kdl {
        "//"
    } else {
        "#"
    };
    println!("{comment} {}", location.path.display());
    if !location.path.exists() {
        println!("{comment} File does not exist; showing built-in defaults.");
    }
    println!();
    print_resolved(&config, format);
    Ok(())
}

fn init_config(
    format: ConfigFormat,
    activate: bool,
    output: crate::commands::OutputFormat,
) -> io::Result<()> {
    let location = location_for(format)?;
    let path = location.path;
    let other = location_for(match format {
        ConfigFormat::Kdl => ConfigFormat::Toml,
        ConfigFormat::Toml => ConfigFormat::Kdl,
    })?
    .path;
    if other.exists() && !activate {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "{} already exists; pass '--activate' to create and select a {} configuration",
                other.display(),
                format.extension()
            ),
        ));
    }
    if path.exists() {
        if activate && env::var_os("TURTLETAP_CONFIG").is_none() {
            write_active_format(format)?;
            if output == crate::commands::OutputFormat::Json {
                return crate::commands::print_json(&serde_json::json!({
                    "created": false,
                    "path": path,
                    "format": format.extension(),
                    "active": true,
                }));
            }
            println!("Selected existing configuration: {}", path.display());
            return Ok(());
        }
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "configuration already exists at {}; edit it or run 'turtletap config show'",
                path.display()
            ),
        ));
    }
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "configuration path has no parent",
        )
    })?;
    fs::create_dir_all(parent)?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    file.write_all(format.template().as_bytes())?;
    write_active_format(format)?;
    if output == crate::commands::OutputFormat::Json {
        return crate::commands::print_json(&serde_json::json!({
            "created": true,
            "path": path,
            "format": format.extension(),
            "active": true,
        }));
    }
    println!("Created {}", path.display());
    println!("Run 'turtletap config check' after editing keybindings.");
    Ok(())
}

fn active_location() -> io::Result<ConfigLocation> {
    if let Some(path) = env::var_os("TURTLETAP_CONFIG").filter(|value| !value.is_empty()) {
        let path = PathBuf::from(path);
        let format = format_for_path(&path)?;
        return Ok(ConfigLocation { path, format });
    }

    let base = config_directory()?;
    let kdl = base.join("config.kdl");
    let toml = base.join("config.toml");
    if let Ok(selected) = fs::read_to_string(base.join(".active-format")) {
        let selected = ConfigFormat::parse(selected.trim())?;
        let path = match selected {
            ConfigFormat::Kdl => &kdl,
            ConfigFormat::Toml => &toml,
        };
        if path.exists() {
            return Ok(ConfigLocation {
                path: path.clone(),
                format: selected,
            });
        }
    }
    if kdl.exists() || !toml.exists() {
        Ok(ConfigLocation {
            path: kdl,
            format: ConfigFormat::Kdl,
        })
    } else {
        Ok(ConfigLocation {
            path: toml,
            format: ConfigFormat::Toml,
        })
    }
}

pub(crate) fn active_path() -> io::Result<PathBuf> {
    Ok(active_location()?.path)
}

fn write_active_format(format: ConfigFormat) -> io::Result<()> {
    if env::var_os("TURTLETAP_CONFIG").is_some() {
        return Ok(());
    }
    let directory = config_directory()?;
    fs::create_dir_all(&directory)?;
    fs::write(directory.join(".active-format"), format.extension())
}

fn edit_config(output: crate::commands::OutputFormat) -> io::Result<()> {
    let location = active_location()?;
    if !location.path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "{} does not exist; run 'turtletap config init {}'",
                location.path.display(),
                location.format.extension()
            ),
        ));
    }
    let editor = env::var_os("VISUAL")
        .or_else(|| env::var_os("EDITOR"))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "VISUAL and EDITOR are unset; set one to use 'turtletap config edit'",
            )
        })?;
    let editor = editor.into_string().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "VISUAL or EDITOR contains non-Unicode text",
        )
    })?;
    let mut parts = shell_words::split(&editor).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("could not parse VISUAL or EDITOR: {error}"),
        )
    })?;
    if parts.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "VISUAL or EDITOR is empty",
        ));
    }
    let executable = parts.remove(0);
    let status = Command::new(executable)
        .args(parts)
        .arg(&location.path)
        .status()?;
    if !status.success() {
        return Err(io::Error::other(format!("editor exited with {status}")));
    }
    let _ = shell_config("TurtleTap")?;
    if output == crate::commands::OutputFormat::Json {
        return crate::commands::print_json(&serde_json::json!({
            "edited": true,
            "valid": true,
            "path": location.path,
        }));
    }
    println!("Configuration is valid: {}", location.path.display());
    println!("Attached dashboards reload valid changes automatically.");
    Ok(())
}

fn reload_config(output: crate::commands::OutputFormat) -> io::Result<()> {
    let location = active_location()?;
    let _ = shell_config("TurtleTap")?;
    if output == crate::commands::OutputFormat::Json {
        return crate::commands::print_json(&serde_json::json!({
            "valid": true,
            "path": location.path,
            "applies_on": "automatic_file_watch_or_next_attach",
        }));
    }
    println!("Configuration is valid: {}", location.path.display());
    println!(
        "Attached dashboards reload this file automatically; otherwise it loads on the next attach."
    );
    Ok(())
}

fn location_for(format: ConfigFormat) -> io::Result<ConfigLocation> {
    if let Some(path) = env::var_os("TURTLETAP_CONFIG").filter(|value| !value.is_empty()) {
        let path = PathBuf::from(path);
        let actual = format_for_path(&path)?;
        if actual != format {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "TURTLETAP_CONFIG points to a .{} file; cannot initialize {}",
                    actual.extension(),
                    format.extension()
                ),
            ));
        }
        return Ok(ConfigLocation { path, format });
    }
    Ok(ConfigLocation {
        path: config_directory()?.join(format!("config.{}", format.extension())),
        format,
    })
}

fn config_directory() -> io::Result<PathBuf> {
    if let Some(base) = env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(base).join("turtletap"));
    }
    let home = env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "cannot locate configuration: HOME is unset; set TURTLETAP_CONFIG",
            )
        })?;
    Ok(PathBuf::from(home).join(".config/turtletap"))
}

fn format_for_path(path: &Path) -> io::Result<ConfigFormat> {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("kdl") => Ok(ConfigFormat::Kdl),
        Some("toml") => Ok(ConfigFormat::Toml),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "configuration path {} must end in .kdl or .toml",
                path.display()
            ),
        )),
    }
}

fn read_settings(location: &ConfigLocation) -> io::Result<SettingsFile> {
    let source = match fs::read_to_string(&location.path) {
        Ok(source) => source,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(SettingsFile::default()),
        Err(error) => return Err(error),
    };
    match location.format {
        ConfigFormat::Kdl => parse_kdl(&source, &location.path),
        ConfigFormat::Toml => ::toml::from_str(&source).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "invalid configuration at {}: {error}",
                    location.path.display()
                ),
            )
        }),
    }
}

fn resolve(title: &str, settings: SettingsFile) -> io::Result<ShellConfig> {
    let SettingsFile {
        shell,
        theme,
        bindings,
    } = settings;
    let mut config = ShellConfig::new(title);
    if let Some(enabled) = shell.mouse_capture {
        config.mouse_capture = enabled;
    }
    if let Some(enabled) = shell.direct_detach {
        config.direct_detach = enabled;
    }
    if let Some(milliseconds) = shell.tick_rate_ms {
        if milliseconds == 0 {
            return Err(invalid("shell.tick_rate_ms must be greater than zero"));
        }
        config.tick_rate = Duration::from_millis(milliseconds);
    }
    config.chrome = resolve_chrome(&shell)?;
    resolve_theme(&mut config.theme, theme)?;

    let defaults = ShellBindings::default();
    let bindings = ShellBindings {
        leaders: parse_bindings(bindings.leaders, defaults.leaders, "leaders")?,
        palette: parse_bindings(bindings.palette, defaults.palette, "palette")?,
        redraw: parse_bindings(bindings.redraw, defaults.redraw, "redraw")?,
        next_screen: parse_bindings(bindings.next_screen, defaults.next_screen, "next_screen")?,
        previous_screen: parse_bindings(
            bindings.previous_screen,
            defaults.previous_screen,
            "previous_screen",
        )?,
        jump_modifiers: parse_modifiers(
            bindings.jump_modifiers,
            defaults.jump_modifiers,
            "jump_modifiers",
        )?,
        shell_detach: parse_bindings(bindings.shell_detach, defaults.shell_detach, "shell_detach")?,
        shell_next_screen: parse_bindings(
            bindings.shell_next_screen,
            defaults.shell_next_screen,
            "shell_next_screen",
        )?,
        shell_previous_screen: parse_bindings(
            bindings.shell_previous_screen,
            defaults.shell_previous_screen,
            "shell_previous_screen",
        )?,
        shell_help: parse_bindings(bindings.shell_help, defaults.shell_help, "shell_help")?,
        leader_palette: parse_bindings(
            bindings.leader_palette,
            defaults.leader_palette,
            "leader_palette",
        )?,
        leader_next_screen: parse_bindings(
            bindings.leader_next_screen,
            defaults.leader_next_screen,
            "leader_next_screen",
        )?,
        leader_previous_screen: parse_bindings(
            bindings.leader_previous_screen,
            defaults.leader_previous_screen,
            "leader_previous_screen",
        )?,
        leader_scroll_up: parse_bindings(
            bindings.leader_scroll_up,
            defaults.leader_scroll_up,
            "leader_scroll_up",
        )?,
        leader_scroll_down: parse_bindings(
            bindings.leader_scroll_down,
            defaults.leader_scroll_down,
            "leader_scroll_down",
        )?,
        leader_close: parse_bindings(bindings.leader_close, defaults.leader_close, "leader_close")?,
        leader_detach: parse_bindings(
            bindings.leader_detach,
            defaults.leader_detach,
            "leader_detach",
        )?,
        leader_help: parse_bindings(bindings.leader_help, defaults.leader_help, "leader_help")?,
        leader_jump_modifiers: parse_modifiers(
            bindings.leader_jump_modifiers,
            defaults.leader_jump_modifiers,
            "leader_jump_modifiers",
        )?,
        action_next_screen: parse_bindings(
            bindings.action_next_screen,
            defaults.action_next_screen,
            "action_next_screen",
        )?,
        action_previous_screen: parse_bindings(
            bindings.action_previous_screen,
            defaults.action_previous_screen,
            "action_previous_screen",
        )?,
        action_scroll_up: parse_bindings(
            bindings.action_scroll_up,
            defaults.action_scroll_up,
            "action_scroll_up",
        )?,
        action_scroll_down: parse_bindings(
            bindings.action_scroll_down,
            defaults.action_scroll_down,
            "action_scroll_down",
        )?,
        action_close: parse_bindings(bindings.action_close, defaults.action_close, "action_close")?,
        action_detach: parse_bindings(
            bindings.action_detach,
            defaults.action_detach,
            "action_detach",
        )?,
        action_help: parse_bindings(bindings.action_help, defaults.action_help, "action_help")?,
        action_clear_query: parse_bindings(
            bindings.action_clear_query,
            defaults.action_clear_query,
            "action_clear_query",
        )?,
        action_jump_modifiers: parse_modifiers(
            bindings.action_jump_modifiers,
            defaults.action_jump_modifiers,
            "action_jump_modifiers",
        )?,
        session_release_driver: parse_bindings(
            bindings.session_release_driver,
            defaults.session_release_driver,
            "session_release_driver",
        )?,
        session_take_driver: parse_bindings(
            bindings.session_take_driver,
            defaults.session_take_driver,
            "session_take_driver",
        )?,
        session_clear: parse_bindings(
            bindings.session_clear,
            defaults.session_clear,
            "session_clear",
        )?,
        session_interrupt: parse_bindings(
            bindings.session_interrupt,
            defaults.session_interrupt,
            "session_interrupt",
        )?,
        session_detach: parse_bindings(
            bindings.session_detach,
            defaults.session_detach,
            "session_detach",
        )?,
        session_delete_to_start: parse_bindings(
            bindings.session_delete_to_start,
            defaults.session_delete_to_start,
            "session_delete_to_start",
        )?,
        session_word_left: parse_bindings(
            bindings.session_word_left,
            defaults.session_word_left,
            "session_word_left",
        )?,
        session_word_right: parse_bindings(
            bindings.session_word_right,
            defaults.session_word_right,
            "session_word_right",
        )?,
        session_line_start: parse_bindings(
            bindings.session_line_start,
            defaults.session_line_start,
            "session_line_start",
        )?,
        session_line_end: parse_bindings(
            bindings.session_line_end,
            defaults.session_line_end,
            "session_line_end",
        )?,
        session_delete_word: parse_bindings(
            bindings.session_delete_word,
            defaults.session_delete_word,
            "session_delete_word",
        )?,
        session_complete: parse_bindings(
            bindings.session_complete,
            defaults.session_complete,
            "session_complete",
        )?,
        session_scroll_up: parse_bindings(
            bindings.session_scroll_up,
            defaults.session_scroll_up,
            "session_scroll_up",
        )?,
        session_scroll_down: parse_bindings(
            bindings.session_scroll_down,
            defaults.session_scroll_down,
            "session_scroll_down",
        )?,
        session_scroll_top: parse_bindings(
            bindings.session_scroll_top,
            defaults.session_scroll_top,
            "session_scroll_top",
        )?,
        session_scroll_bottom: parse_bindings(
            bindings.session_scroll_bottom,
            defaults.session_scroll_bottom,
            "session_scroll_bottom",
        )?,
        dashboard_up: parse_bindings(bindings.dashboard_up, defaults.dashboard_up, "dashboard_up")?,
        dashboard_down: parse_bindings(
            bindings.dashboard_down,
            defaults.dashboard_down,
            "dashboard_down",
        )?,
        dashboard_view: parse_bindings(
            bindings.dashboard_view,
            defaults.dashboard_view,
            "dashboard_view",
        )?,
        dashboard_take: parse_bindings(
            bindings.dashboard_take,
            defaults.dashboard_take,
            "dashboard_take",
        )?,
        dashboard_search: parse_bindings(
            bindings.dashboard_search,
            defaults.dashboard_search,
            "dashboard_search",
        )?,
        dashboard_new: parse_bindings(
            bindings.dashboard_new,
            defaults.dashboard_new,
            "dashboard_new",
        )?,
        dashboard_rename: parse_bindings(
            bindings.dashboard_rename,
            defaults.dashboard_rename,
            "dashboard_rename",
        )?,
        dashboard_delete: parse_bindings(
            bindings.dashboard_delete,
            defaults.dashboard_delete,
            "dashboard_delete",
        )?,
        dashboard_stop: parse_bindings(
            bindings.dashboard_stop,
            defaults.dashboard_stop,
            "dashboard_stop",
        )?,
        dashboard_keybindings: parse_bindings(
            bindings.dashboard_keybindings,
            defaults.dashboard_keybindings,
            "dashboard_keybindings",
        )?,
        dashboard_close: parse_bindings(
            bindings.dashboard_close,
            defaults.dashboard_close,
            "dashboard_close",
        )?,
    };
    validate_bindings(&bindings)?;
    config.bindings = bindings;
    Ok(config)
}

/// The config `chrome` value and rail dimensions for a resolved [`Chrome`].
/// Tabs mode reports zeroed dimensions, which the printers omit.
fn chrome_fields(chrome: Chrome) -> (&'static str, u16, u16, u16) {
    match chrome {
        Chrome::Tabs => ("tabs", 0, 0, 0),
        Chrome::Rail {
            width,
            narrow,
            min_content,
        } => ("rail", width, narrow, min_content),
    }
}

/// Resolves the chrome mode. The CLI defaults to the master-detail rail; the
/// library default is tabs, so this is set explicitly rather than inherited.
fn resolve_chrome(shell: &ShellSettings) -> io::Result<Chrome> {
    let mode = shell.chrome.as_deref().unwrap_or("rail");
    match mode {
        "tabs" => Ok(Chrome::Tabs),
        "rail" => {
            let Chrome::Rail {
                width,
                narrow,
                min_content,
            } = Chrome::rail()
            else {
                unreachable!("Chrome::rail is always a rail");
            };
            Ok(Chrome::Rail {
                width: shell.rail_width.unwrap_or(width),
                narrow: shell.rail_narrow.unwrap_or(narrow),
                min_content: shell.rail_min_content.unwrap_or(min_content),
            })
        }
        other => Err(invalid(format!(
            "shell.chrome must be 'rail' or 'tabs', not '{other}'"
        ))),
    }
}

fn resolve_theme(theme: &mut Theme, settings: ThemeSettings) -> io::Result<()> {
    apply_foreground(&mut theme.chrome, settings.chrome, "theme.chrome")?;
    apply_foreground(&mut theme.muted, settings.muted, "theme.muted")?;
    apply_foreground(&mut theme.accent, settings.accent, "theme.accent")?;
    apply_foreground(&mut theme.working, settings.working, "theme.working")?;
    apply_foreground(&mut theme.attention, settings.attention, "theme.attention")?;
    apply_foreground(&mut theme.failed, settings.failed, "theme.failed")?;
    apply_foreground(&mut theme.complete, settings.complete, "theme.complete")?;
    apply_foreground(
        &mut theme.selected,
        settings.selected.foreground,
        "theme.selected.foreground",
    )?;
    if let Some(value) = settings.selected.background {
        theme.selected = theme.selected.bg(parse_color(&value)
            .map_err(|message| invalid(format!("theme.selected.background: {message}")))?);
    }
    Ok(())
}

fn apply_foreground(style: &mut Style, value: Option<String>, field: &str) -> io::Result<()> {
    if let Some(value) = value {
        *style = style
            .fg(parse_color(&value).map_err(|message| invalid(format!("{field}: {message}")))?);
    }
    Ok(())
}

fn parse_color(value: &str) -> Result<Color, String> {
    let normalized = value.trim().to_ascii_lowercase();
    let color = match normalized.as_str() {
        "default" | "reset" => Color::Reset,
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "gray" | "grey" => Color::Gray,
        "dark-gray" | "dark-grey" | "bright-black" => Color::DarkGray,
        "light-red" | "bright-red" => Color::LightRed,
        "light-green" | "bright-green" => Color::LightGreen,
        "light-yellow" | "bright-yellow" => Color::LightYellow,
        "light-blue" | "bright-blue" => Color::LightBlue,
        "light-magenta" | "bright-magenta" => Color::LightMagenta,
        "light-cyan" | "bright-cyan" => Color::LightCyan,
        "white" | "bright-white" => Color::White,
        _ => return parse_extended_color(&normalized),
    };
    Ok(color)
}

fn parse_extended_color(value: &str) -> Result<Color, String> {
    if let Some(hex) = value.strip_prefix('#')
        && hex.len() == 6
        && hex.is_ascii()
        && let (Ok(red), Ok(green), Ok(blue)) = (
            u8::from_str_radix(&hex[0..2], 16),
            u8::from_str_radix(&hex[2..4], 16),
            u8::from_str_radix(&hex[4..6], 16),
        )
    {
        return Ok(Color::Rgb(red, green, blue));
    }
    if let Some(index) = value.strip_prefix("indexed-")
        && let Ok(index) = index.parse::<u8>()
    {
        return Ok(Color::Indexed(index));
    }
    Err(format!(
        "unknown color '{value}'; expected a named color, #rrggbb, or indexed-0 through indexed-255"
    ))
}

fn parse_bindings(
    configured: Option<Vec<String>>,
    defaults: Vec<KeyBinding>,
    field: &str,
) -> io::Result<Vec<KeyBinding>> {
    configured.map_or(Ok(defaults), |values| {
        values
            .iter()
            .map(|value| {
                parse_key_binding(value)
                    .map_err(|message| invalid(format!("bindings.{field}: {message}")))
            })
            .collect()
    })
}

fn parse_modifiers(
    configured: Option<Vec<String>>,
    defaults: Vec<KeyModifiers>,
    field: &str,
) -> io::Result<Vec<KeyModifiers>> {
    configured.map_or(Ok(defaults), |values| {
        values
            .iter()
            .map(|value| {
                parse_modifier_set(value)
                    .map_err(|message| invalid(format!("bindings.{field}: {message}")))
            })
            .collect()
    })
}

fn validate_bindings(bindings: &ShellBindings) -> io::Result<()> {
    validate_binding_context(
        &[
            ("leaders", &bindings.leaders),
            ("palette", &bindings.palette),
            ("redraw", &bindings.redraw),
            ("next_screen", &bindings.next_screen),
            ("previous_screen", &bindings.previous_screen),
        ],
        &[("jump_modifiers", &bindings.jump_modifiers)],
    )?;
    validate_binding_context(
        &[
            ("shell_detach", &bindings.shell_detach),
            ("shell_next_screen", &bindings.shell_next_screen),
            ("shell_previous_screen", &bindings.shell_previous_screen),
            ("shell_help", &bindings.shell_help),
        ],
        &[],
    )?;
    validate_binding_context(
        &[
            ("leader_palette", &bindings.leader_palette),
            ("leader_next_screen", &bindings.leader_next_screen),
            ("leader_previous_screen", &bindings.leader_previous_screen),
            ("leader_scroll_up", &bindings.leader_scroll_up),
            ("leader_scroll_down", &bindings.leader_scroll_down),
            ("leader_close", &bindings.leader_close),
            ("leader_detach", &bindings.leader_detach),
            ("leader_help", &bindings.leader_help),
        ],
        &[("leader_jump_modifiers", &bindings.leader_jump_modifiers)],
    )?;
    validate_binding_context(
        &[
            ("action_next_screen", &bindings.action_next_screen),
            ("action_previous_screen", &bindings.action_previous_screen),
            ("action_scroll_up", &bindings.action_scroll_up),
            ("action_scroll_down", &bindings.action_scroll_down),
            ("action_close", &bindings.action_close),
            ("action_detach", &bindings.action_detach),
            ("action_help", &bindings.action_help),
            ("action_clear_query", &bindings.action_clear_query),
        ],
        &[("action_jump_modifiers", &bindings.action_jump_modifiers)],
    )?;
    validate_binding_context(
        &[
            ("session_release_driver", &bindings.session_release_driver),
            ("session_take_driver", &bindings.session_take_driver),
            ("session_clear", &bindings.session_clear),
            ("session_interrupt", &bindings.session_interrupt),
            ("session_detach", &bindings.session_detach),
            ("session_delete_to_start", &bindings.session_delete_to_start),
            ("session_word_left", &bindings.session_word_left),
            ("session_word_right", &bindings.session_word_right),
            ("session_line_start", &bindings.session_line_start),
            ("session_line_end", &bindings.session_line_end),
            ("session_delete_word", &bindings.session_delete_word),
            ("session_complete", &bindings.session_complete),
            ("session_scroll_up", &bindings.session_scroll_up),
            ("session_scroll_down", &bindings.session_scroll_down),
            ("session_scroll_top", &bindings.session_scroll_top),
            ("session_scroll_bottom", &bindings.session_scroll_bottom),
        ],
        &[],
    )?;
    validate_binding_context(
        &[
            ("dashboard_up", &bindings.dashboard_up),
            ("dashboard_down", &bindings.dashboard_down),
            ("dashboard_view", &bindings.dashboard_view),
            ("dashboard_take", &bindings.dashboard_take),
            ("dashboard_search", &bindings.dashboard_search),
            ("dashboard_new", &bindings.dashboard_new),
            ("dashboard_rename", &bindings.dashboard_rename),
            ("dashboard_delete", &bindings.dashboard_delete),
            ("dashboard_stop", &bindings.dashboard_stop),
            ("dashboard_keybindings", &bindings.dashboard_keybindings),
            ("dashboard_close", &bindings.dashboard_close),
        ],
        &[],
    )
}

fn validate_binding_context(
    groups: &[(&str, &Vec<KeyBinding>)],
    modifier_groups: &[(&str, &Vec<KeyModifiers>)],
) -> io::Result<()> {
    for (name, modifier_list) in modifier_groups {
        if let Some(duplicate) = modifier_list
            .iter()
            .enumerate()
            .find_map(|(item, modifier)| {
                modifier_list[item + 1..]
                    .contains(modifier)
                    .then_some(*modifier)
            })
        {
            return Err(invalid(format!(
                "bindings.{name} contains {} more than once",
                modifier_label(duplicate)
            )));
        }
    }
    for (index, (left_name, left)) in groups.iter().enumerate() {
        if let Some(duplicate) = left
            .iter()
            .enumerate()
            .find_map(|(item, binding)| left[item + 1..].contains(binding).then_some(*binding))
        {
            return Err(invalid(format!(
                "bindings.{left_name} contains {} more than once",
                duplicate.label()
            )));
        }
        for (right_name, right) in &groups[index + 1..] {
            if let Some(collision) = left.iter().find(|binding| right.contains(binding)) {
                return Err(invalid(format!(
                    "{} is assigned to both bindings.{left_name} and bindings.{right_name}",
                    collision.label()
                )));
            }
        }
        for (modifier_name, modifiers) in modifier_groups {
            for modifiers in *modifiers {
                for digit in '1'..='9' {
                    let jump = KeyBinding::new(KeyCode::Char(digit), *modifiers);
                    if left.contains(&jump) {
                        return Err(invalid(format!(
                            "{} is assigned to both bindings.{left_name} and bindings.{modifier_name}",
                            jump.label()
                        )));
                    }
                }
            }
        }
    }
    Ok(())
}

fn parse_key_binding(value: &str) -> Result<KeyBinding, String> {
    let normalized = value.trim().to_ascii_lowercase();
    let mut parts: Vec<&str> = normalized.split('-').collect();
    let key = parts
        .pop()
        .filter(|key| !key.is_empty())
        .ok_or_else(|| format!("'{value}' has no key"))?;
    let modifiers = parse_modifier_parts(&parts, value)?;
    let code = match key {
        "backspace" => KeyCode::Backspace,
        "enter" | "return" => KeyCode::Enter,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdown" => KeyCode::PageDown,
        "tab" => KeyCode::Tab,
        "backtab" => KeyCode::BackTab,
        "delete" | "del" => KeyCode::Delete,
        "insert" | "ins" => KeyCode::Insert,
        "escape" | "esc" => KeyCode::Esc,
        "space" => KeyCode::Char(' '),
        key if key.len() == 1 => KeyCode::Char(key.chars().next().unwrap_or_default()),
        key if key.starts_with('f') => {
            let number = key[1..]
                .parse::<u8>()
                .ok()
                .filter(|number| (1..=24).contains(number))
                .ok_or_else(|| format!("'{key}' is not a supported function key"))?;
            KeyCode::F(number)
        }
        _ => return Err(format!("'{key}' is not a supported key name")),
    };
    Ok(KeyBinding::new(code, modifiers))
}

fn parse_modifier_set(value: &str) -> Result<KeyModifiers, String> {
    let normalized = value.trim().to_ascii_lowercase();
    if normalized == "none" {
        return Ok(KeyModifiers::empty());
    }
    let parts: Vec<&str> = normalized.split(['-', '+']).collect();
    let modifiers = parse_modifier_parts(&parts, value)?;
    if modifiers.is_empty() {
        return Err(format!("'{value}' must contain at least one modifier"));
    }
    Ok(modifiers)
}

fn modifier_label(modifiers: KeyModifiers) -> String {
    if modifiers.is_empty() {
        "none".to_owned()
    } else {
        KeyBinding::new(KeyCode::Char('1'), modifiers)
            .label()
            .trim_end_matches("-1")
            .to_ascii_lowercase()
    }
}

fn parse_modifier_parts(parts: &[&str], original: &str) -> Result<KeyModifiers, String> {
    let mut modifiers = KeyModifiers::empty();
    for part in parts {
        let modifier = match *part {
            "ctrl" | "control" => KeyModifiers::CONTROL,
            "alt" | "option" | "opt" => KeyModifiers::ALT,
            "shift" => KeyModifiers::SHIFT,
            "super" | "cmd" | "command" => KeyModifiers::SUPER,
            "" => return Err(format!("'{original}' contains an empty modifier")),
            unknown => return Err(format!("'{unknown}' is not a supported modifier")),
        };
        if modifiers.contains(modifier) {
            return Err(format!("'{original}' repeats a modifier"));
        }
        modifiers.insert(modifier);
    }
    Ok(modifiers)
}

fn print_resolved(config: &ShellConfig, format: ConfigFormat) {
    match format {
        ConfigFormat::Kdl => print_resolved_kdl(config),
        ConfigFormat::Toml => print_resolved_toml(config),
    }
}

fn style_foreground(style: Style) -> String {
    color_name(style.fg)
}

fn color_name(color: Option<Color>) -> String {
    match color.unwrap_or(Color::Reset) {
        Color::Reset => "default".to_owned(),
        Color::Black => "black".to_owned(),
        Color::Red => "red".to_owned(),
        Color::Green => "green".to_owned(),
        Color::Yellow => "yellow".to_owned(),
        Color::Blue => "blue".to_owned(),
        Color::Magenta => "magenta".to_owned(),
        Color::Cyan => "cyan".to_owned(),
        Color::Gray => "gray".to_owned(),
        Color::DarkGray => "dark-gray".to_owned(),
        Color::LightRed => "light-red".to_owned(),
        Color::LightGreen => "light-green".to_owned(),
        Color::LightYellow => "light-yellow".to_owned(),
        Color::LightBlue => "light-blue".to_owned(),
        Color::LightMagenta => "light-magenta".to_owned(),
        Color::LightCyan => "light-cyan".to_owned(),
        Color::White => "white".to_owned(),
        Color::Indexed(index) => format!("indexed-{index}"),
        Color::Rgb(red, green, blue) => format!("#{red:02x}{green:02x}{blue:02x}"),
    }
}

fn print_binding_list(name: &str, bindings: &[KeyBinding]) {
    let values = bindings
        .iter()
        .map(|binding| format!("\"{}\"", binding.label().to_ascii_lowercase()))
        .collect::<Vec<_>>()
        .join(", ");
    println!("{name} = [{values}]");
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_chords_parse_with_terminal_friendly_aliases() {
        assert_eq!(
            parse_key_binding("Option-Right"),
            Ok(KeyBinding::new(KeyCode::Right, KeyModifiers::ALT))
        );
        assert_eq!(
            parse_key_binding("ctrl-space"),
            Ok(KeyBinding::new(KeyCode::Char(' '), KeyModifiers::CONTROL))
        );
        assert_eq!(
            parse_key_binding("ctrl-`"),
            Ok(KeyBinding::new(KeyCode::Char('`'), KeyModifiers::CONTROL))
        );
        assert_eq!(
            parse_key_binding("Ctrl-PgDown"),
            Ok(KeyBinding::new(KeyCode::PageDown, KeyModifiers::CONTROL))
        );
        assert_eq!(
            parse_key_binding("ctrl-/"),
            Ok(KeyBinding::new(KeyCode::Char('/'), KeyModifiers::CONTROL))
        );
    }

    #[test]
    fn captured_chords_reject_unrepresentable_modifiers() {
        let binding = KeyBinding::new(KeyCode::Char('h'), KeyModifiers::HYPER);
        let error = canonical_binding(binding).expect_err("Hyper cannot be persisted");

        assert!(error.to_string().contains("cannot store"));
    }

    #[test]
    fn conflicting_bindings_are_rejected() {
        let defaults = ShellBindings::default();
        let bindings = ShellBindings {
            next_screen: defaults.palette.clone(),
            ..defaults
        };
        let error = validate_bindings(&bindings).expect_err("collision should fail");
        assert!(error.to_string().contains("assigned to both"));
    }

    #[test]
    fn identical_keys_are_allowed_in_separate_interaction_contexts() {
        let defaults = ShellBindings::default();
        let bindings = ShellBindings {
            leader_close: vec![KeyBinding::new(KeyCode::Char('z'), KeyModifiers::empty())],
            dashboard_close: vec![KeyBinding::new(KeyCode::Char('z'), KeyModifiers::empty())],
            ..defaults
        };
        validate_bindings(&bindings).expect("separate contexts may reuse a key");
    }

    #[test]
    fn unmodified_digit_groups_parse_for_leader_sequences() {
        assert_eq!(parse_modifier_set("none"), Ok(KeyModifiers::empty()));
    }

    #[test]
    fn empty_binding_lists_disable_an_action() {
        let source = "[bindings]\npalette = []\n";
        let settings: SettingsFile = ::toml::from_str(source).expect("settings should parse");
        let config = resolve("test", settings).expect("settings should resolve");
        assert!(config.bindings.palette.is_empty());
    }

    #[test]
    fn canonical_kdl_maps_to_the_same_resolved_settings() {
        let settings =
            parse_kdl(KDL_TEMPLATE, Path::new("config.kdl")).expect("canonical KDL should parse");
        let config = resolve("test", settings).expect("KDL settings should resolve");

        assert_eq!(config.tick_rate, Duration::from_millis(100));
        assert_eq!(
            config.bindings.leaders[0],
            KeyBinding::new(KeyCode::Char('g'), KeyModifiers::CONTROL)
        );
        assert_eq!(
            config.bindings.palette[0],
            KeyBinding::new(KeyCode::Char('`'), KeyModifiers::CONTROL)
        );
        assert!(config.bindings.next_screen.is_empty());
        assert_eq!(
            config.bindings.redraw[0],
            KeyBinding::new(KeyCode::Char('/'), KeyModifiers::CONTROL)
        );
        assert_eq!(
            config.bindings.action_close[0],
            KeyBinding::new(KeyCode::Char('x'), KeyModifiers::ALT)
        );
        assert_eq!(
            config.bindings.dashboard_new[0],
            KeyBinding::new(KeyCode::Char('n'), KeyModifiers::empty())
        );
        assert_eq!(config.theme.selected.bg, Some(Color::Cyan));
    }

    #[test]
    fn canonical_toml_includes_every_binding_group() {
        let settings: SettingsFile =
            ::toml::from_str(TOML_TEMPLATE).expect("canonical TOML should parse");
        let config = resolve("test", settings).expect("TOML settings should resolve");

        assert_eq!(config.bindings, ShellBindings::default());
    }

    #[test]
    fn kdl_binding_update_preserves_surrounding_comments_and_resolves() {
        let source = "// keep me\nbindings {\n    palette \"ctrl-p\" // keep this too\n}\n";
        let updated = update_kdl_binding(source, "palette", "ctrl-space", Path::new("config.kdl"))
            .expect("binding should update");

        assert!(updated.contains("// keep me"));
        assert!(updated.contains("// keep this too"));
        let settings =
            parse_kdl(&updated, Path::new("config.kdl")).expect("updated KDL should parse");
        let config = resolve("test", settings).expect("updated KDL should resolve");
        assert_eq!(
            config.bindings.palette,
            vec![KeyBinding::new(KeyCode::Char(' '), KeyModifiers::CONTROL)]
        );
    }

    #[test]
    fn toml_binding_update_preserves_surrounding_comments_and_resolves() {
        let source = "# keep me\n[bindings]\npalette = [\"ctrl-p\"] # keep this too\n";
        let updated =
            update_toml_binding(source, "palette", "ctrl-space", Path::new("config.toml"))
                .expect("binding should update");

        assert!(updated.contains("# keep me"));
        assert!(updated.contains("# keep this too"));
        let settings: SettingsFile = ::toml::from_str(&updated).expect("updated TOML should parse");
        let config = resolve("test", settings).expect("updated TOML should resolve");
        assert_eq!(
            config.bindings.palette,
            vec![KeyBinding::new(KeyCode::Char(' '), KeyModifiers::CONTROL)]
        );
    }

    #[test]
    fn kdl_and_toml_theme_colors_resolve_equivalently() {
        let kdl = r##"theme {
    chrome "default"
    selected foreground="white" background="#123456"
    working "indexed-42"
}
"##;
        let toml = r##"[theme]
chrome = "default"
working = "indexed-42"

[theme.selected]
foreground = "white"
background = "#123456"
"##;
        let kdl = resolve(
            "test",
            parse_kdl(kdl, Path::new("config.kdl")).expect("KDL theme should parse"),
        )
        .expect("KDL theme should resolve");
        let toml = resolve(
            "test",
            ::toml::from_str(toml).expect("TOML theme should parse"),
        )
        .expect("TOML theme should resolve");

        assert_eq!(kdl.theme.chrome.fg, Some(Color::Reset));
        assert_eq!(kdl.theme.working.fg, Some(Color::Indexed(42)));
        assert_eq!(kdl.theme.selected.fg, Some(Color::White));
        assert_eq!(kdl.theme.selected.bg, Some(Color::Rgb(0x12, 0x34, 0x56)));
        assert_eq!(kdl.theme.chrome, toml.theme.chrome);
        assert_eq!(kdl.theme.working, toml.theme.working);
        assert_eq!(kdl.theme.selected, toml.theme.selected);
    }

    #[test]
    fn theme_rejects_unknown_colors_and_children() {
        let settings: SettingsFile =
            ::toml::from_str("[theme]\naccent = \"ultraviolet\"\n").expect("TOML should parse");
        let error = resolve("test", settings).expect_err("unknown color should fail");
        assert!(error.to_string().contains("theme.accent"));

        assert!(parse_color("#aébc?").is_err());

        let error = parse_kdl(
            "theme {\n    surprise \"cyan\"\n}\n",
            Path::new("config.kdl"),
        )
        .expect_err("unknown theme child should fail");
        assert!(error.to_string().contains("unknown theme child"));
    }

    #[test]
    fn kdl_rejects_unknown_nodes_and_properties() {
        let error = parse_kdl("shell mystery=true\nother {}\n", Path::new("config.kdl"))
            .expect_err("unknown KDL vocabulary should fail");
        assert!(error.to_string().contains("unknown shell property"));
    }
}
