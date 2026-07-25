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
    BindingId, Chrome, KeyBinding, KeyModifiers, ShellBindings, ShellConfig, Theme,
    key_modifiers_config_label, parse_key_modifiers,
    tui::style::{Color, Style},
};

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
    shell: ShellBindingSettings,
    leader: LeaderBindingSettings,
    action: ActionBindingSettings,
    session: SessionBindingSettings,
    dashboard: DashboardBindingSettings,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ShellBindingSettings {
    detach: Option<Vec<String>>,
    next_screen: Option<Vec<String>>,
    previous_screen: Option<Vec<String>>,
    help: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct LeaderBindingSettings {
    palette: Option<Vec<String>>,
    next_screen: Option<Vec<String>>,
    previous_screen: Option<Vec<String>>,
    scroll_up: Option<Vec<String>>,
    scroll_down: Option<Vec<String>>,
    close: Option<Vec<String>>,
    detach: Option<Vec<String>>,
    help: Option<Vec<String>>,
    jump_modifiers: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ActionBindingSettings {
    next_screen: Option<Vec<String>>,
    previous_screen: Option<Vec<String>>,
    scroll_up: Option<Vec<String>>,
    scroll_down: Option<Vec<String>>,
    close: Option<Vec<String>>,
    detach: Option<Vec<String>>,
    help: Option<Vec<String>>,
    clear_query: Option<Vec<String>>,
    jump_modifiers: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct SessionBindingSettings {
    release_driver: Option<Vec<String>>,
    take_driver: Option<Vec<String>>,
    clear: Option<Vec<String>>,
    interrupt: Option<Vec<String>>,
    detach: Option<Vec<String>>,
    delete_to_start: Option<Vec<String>>,
    word_left: Option<Vec<String>>,
    word_right: Option<Vec<String>>,
    line_start: Option<Vec<String>>,
    line_end: Option<Vec<String>>,
    delete_word: Option<Vec<String>>,
    complete: Option<Vec<String>>,
    scroll_up: Option<Vec<String>>,
    scroll_down: Option<Vec<String>>,
    scroll_top: Option<Vec<String>>,
    scroll_bottom: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct DashboardBindingSettings {
    up: Option<Vec<String>>,
    down: Option<Vec<String>>,
    view: Option<Vec<String>>,
    take: Option<Vec<String>>,
    search: Option<Vec<String>>,
    new: Option<Vec<String>>,
    rename: Option<Vec<String>>,
    delete: Option<Vec<String>>,
    stop: Option<Vec<String>>,
    keybindings: Option<Vec<String>>,
    close: Option<Vec<String>>,
}

pub(crate) fn shell_config(title: &str) -> io::Result<ShellConfig> {
    let location = active_location()?;
    let settings = read_settings(&location)?;
    resolve(title, settings)
}

pub(crate) fn save_binding(action: BindingId, binding: KeyBinding) -> io::Result<ShellConfig> {
    let location = active_location()?;
    let source = match fs::read_to_string(&location.path) {
        Ok(source) => source,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            location.format.template().to_owned()
        }
        Err(error) => return Err(error),
    };
    let label = binding
        .config_label()
        .map_err(|error| invalid(error.to_string()))?;
    let updated = match location.format {
        ConfigFormat::Kdl => update_kdl_binding(
            &source,
            action.context().config_scope(),
            action.config_name(),
            &label,
            &location.path,
        )?,
        ConfigFormat::Toml => update_toml_binding(
            &source,
            action.context().config_scope(),
            action.config_name(),
            &label,
            &location.path,
        )?,
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
        ConfigFormat::Toml => parse_toml(source, &location.path),
    }
}

fn parse_toml(source: &str, path: &Path) -> io::Result<SettingsFile> {
    let mut value = ::toml::from_str::<::toml::Value>(source).map_err(|error| {
        invalid(format!(
            "invalid configuration at {}: {error}",
            path.display()
        ))
    })?;
    normalize_legacy_toml_bindings(&mut value, path)?;
    value.try_into().map_err(|error| {
        invalid(format!(
            "invalid configuration at {}: {error}",
            path.display()
        ))
    })
}

fn normalize_legacy_toml_bindings(value: &mut ::toml::Value, path: &Path) -> io::Result<()> {
    let Some(bindings) = value
        .as_table_mut()
        .and_then(|root| root.get_mut("bindings"))
        .and_then(::toml::Value::as_table_mut)
    else {
        return Ok(());
    };
    for scope in ["shell", "leader", "action", "session", "dashboard"] {
        let prefix = format!("{scope}_");
        let legacy = bindings
            .keys()
            .filter_map(|key| {
                key.strip_prefix(&prefix)
                    .map(|name| (key.clone(), name.to_owned()))
            })
            .collect::<Vec<_>>();
        for (legacy_name, nested_name) in legacy {
            let legacy_value = bindings
                .remove(&legacy_name)
                .expect("legacy key came from this table");
            let group = bindings
                .entry(scope)
                .or_insert_with(|| ::toml::Value::Table(::toml::map::Map::new()))
                .as_table_mut()
                .ok_or_else(|| {
                    invalid(format!(
                        "invalid configuration at {}: bindings.{scope} must be a table",
                        path.display()
                    ))
                })?;
            if group.contains_key(&nested_name) {
                return Err(invalid(format!(
                    "invalid configuration at {}: bindings.{scope}.{nested_name} is set in both nested and legacy flat form",
                    path.display()
                )));
            }
            group.insert(nested_name, legacy_value);
        }
    }
    Ok(())
}

fn update_kdl_binding(
    source: &str,
    scope: Option<&str>,
    name: &str,
    label: &str,
    path: &Path,
) -> io::Result<String> {
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
    let children = if let Some(scope) = scope {
        let legacy_name = format!("{scope}-{name}");
        if let Some(node) = children.get_mut(&legacy_name) {
            node.clear_entries();
            node.push(label);
            node.fmt();
            return Ok(document.to_string());
        }
        if children.get(scope).is_none() {
            children.nodes_mut().push(::kdl::KdlNode::new(scope));
        }
        children
            .get_mut(scope)
            .ok_or_else(|| invalid(format!("could not create bindings group '{scope}'")))?
            .ensure_children()
    } else {
        children
    };
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

fn update_toml_binding(
    source: &str,
    scope: Option<&str>,
    name: &str,
    label: &str,
    path: &Path,
) -> io::Result<String> {
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
    let table = if let Some(scope) = scope {
        let legacy_key = format!("{scope}_{}", name.replace('-', "_"));
        if document["bindings"]
            .as_table()
            .is_some_and(|bindings| bindings.contains_key(&legacy_key))
        {
            let table = &mut document["bindings"];
            replace_toml_binding(table, &legacy_key, label);
            return Ok(document.to_string());
        }
        if !document["bindings"]
            .as_table()
            .is_some_and(|bindings| bindings.contains_key(scope))
        {
            document["bindings"][scope] = toml_edit::Item::Table(toml_edit::Table::new());
        }
        if !document["bindings"][scope].is_table() {
            return Err(invalid(format!("bindings.{scope} must be a TOML table")));
        }
        &mut document["bindings"][scope]
    } else {
        &mut document["bindings"]
    };
    let key = name.replace('-', "_");
    replace_toml_binding(table, &key, label);
    Ok(document.to_string())
}

fn replace_toml_binding(table: &mut toml_edit::Item, key: &str, label: &str) {
    let decor = table[&key].as_value().map(|value| value.decor().clone());
    let mut values = toml_edit::Array::new();
    values.push(label);
    let mut value = toml_edit::Value::Array(values);
    if let Some(decor) = decor {
        *value.decor_mut() = decor;
    }
    table[&key] = toml_edit::Item::Value(value);
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
            println!("{}", path.display());
            Ok(())
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
        ConfigFormat::Toml => parse_toml(&source, &location.path),
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
        shell_detach: parse_bindings(
            bindings.shell.detach,
            defaults.shell_detach,
            "bindings.shell.detach",
        )?,
        shell_next_screen: parse_bindings(
            bindings.shell.next_screen,
            defaults.shell_next_screen,
            "bindings.shell.next_screen",
        )?,
        shell_previous_screen: parse_bindings(
            bindings.shell.previous_screen,
            defaults.shell_previous_screen,
            "bindings.shell.previous_screen",
        )?,
        shell_help: parse_bindings(
            bindings.shell.help,
            defaults.shell_help,
            "bindings.shell.help",
        )?,
        leader_palette: parse_bindings(
            bindings.leader.palette,
            defaults.leader_palette,
            "bindings.leader.palette",
        )?,
        leader_next_screen: parse_bindings(
            bindings.leader.next_screen,
            defaults.leader_next_screen,
            "bindings.leader.next_screen",
        )?,
        leader_previous_screen: parse_bindings(
            bindings.leader.previous_screen,
            defaults.leader_previous_screen,
            "bindings.leader.previous_screen",
        )?,
        leader_scroll_up: parse_bindings(
            bindings.leader.scroll_up,
            defaults.leader_scroll_up,
            "bindings.leader.scroll_up",
        )?,
        leader_scroll_down: parse_bindings(
            bindings.leader.scroll_down,
            defaults.leader_scroll_down,
            "bindings.leader.scroll_down",
        )?,
        leader_close: parse_bindings(
            bindings.leader.close,
            defaults.leader_close,
            "bindings.leader.close",
        )?,
        leader_detach: parse_bindings(
            bindings.leader.detach,
            defaults.leader_detach,
            "bindings.leader.detach",
        )?,
        leader_help: parse_bindings(
            bindings.leader.help,
            defaults.leader_help,
            "bindings.leader.help",
        )?,
        leader_jump_modifiers: parse_modifiers(
            bindings.leader.jump_modifiers,
            defaults.leader_jump_modifiers,
            "bindings.leader.jump_modifiers",
        )?,
        action_next_screen: parse_bindings(
            bindings.action.next_screen,
            defaults.action_next_screen,
            "bindings.action.next_screen",
        )?,
        action_previous_screen: parse_bindings(
            bindings.action.previous_screen,
            defaults.action_previous_screen,
            "bindings.action.previous_screen",
        )?,
        action_scroll_up: parse_bindings(
            bindings.action.scroll_up,
            defaults.action_scroll_up,
            "bindings.action.scroll_up",
        )?,
        action_scroll_down: parse_bindings(
            bindings.action.scroll_down,
            defaults.action_scroll_down,
            "bindings.action.scroll_down",
        )?,
        action_close: parse_bindings(
            bindings.action.close,
            defaults.action_close,
            "bindings.action.close",
        )?,
        action_detach: parse_bindings(
            bindings.action.detach,
            defaults.action_detach,
            "bindings.action.detach",
        )?,
        action_help: parse_bindings(
            bindings.action.help,
            defaults.action_help,
            "bindings.action.help",
        )?,
        action_clear_query: parse_bindings(
            bindings.action.clear_query,
            defaults.action_clear_query,
            "bindings.action.clear_query",
        )?,
        action_jump_modifiers: parse_modifiers(
            bindings.action.jump_modifiers,
            defaults.action_jump_modifiers,
            "bindings.action.jump_modifiers",
        )?,
        session_release_driver: parse_bindings(
            bindings.session.release_driver,
            defaults.session_release_driver,
            "bindings.session.release_driver",
        )?,
        session_take_driver: parse_bindings(
            bindings.session.take_driver,
            defaults.session_take_driver,
            "bindings.session.take_driver",
        )?,
        session_clear: parse_bindings(
            bindings.session.clear,
            defaults.session_clear,
            "bindings.session.clear",
        )?,
        session_interrupt: parse_bindings(
            bindings.session.interrupt,
            defaults.session_interrupt,
            "bindings.session.interrupt",
        )?,
        session_detach: parse_bindings(
            bindings.session.detach,
            defaults.session_detach,
            "bindings.session.detach",
        )?,
        session_delete_to_start: parse_bindings(
            bindings.session.delete_to_start,
            defaults.session_delete_to_start,
            "bindings.session.delete_to_start",
        )?,
        session_word_left: parse_bindings(
            bindings.session.word_left,
            defaults.session_word_left,
            "bindings.session.word_left",
        )?,
        session_word_right: parse_bindings(
            bindings.session.word_right,
            defaults.session_word_right,
            "bindings.session.word_right",
        )?,
        session_line_start: parse_bindings(
            bindings.session.line_start,
            defaults.session_line_start,
            "bindings.session.line_start",
        )?,
        session_line_end: parse_bindings(
            bindings.session.line_end,
            defaults.session_line_end,
            "bindings.session.line_end",
        )?,
        session_delete_word: parse_bindings(
            bindings.session.delete_word,
            defaults.session_delete_word,
            "bindings.session.delete_word",
        )?,
        session_complete: parse_bindings(
            bindings.session.complete,
            defaults.session_complete,
            "bindings.session.complete",
        )?,
        session_scroll_up: parse_bindings(
            bindings.session.scroll_up,
            defaults.session_scroll_up,
            "bindings.session.scroll_up",
        )?,
        session_scroll_down: parse_bindings(
            bindings.session.scroll_down,
            defaults.session_scroll_down,
            "bindings.session.scroll_down",
        )?,
        session_scroll_top: parse_bindings(
            bindings.session.scroll_top,
            defaults.session_scroll_top,
            "bindings.session.scroll_top",
        )?,
        session_scroll_bottom: parse_bindings(
            bindings.session.scroll_bottom,
            defaults.session_scroll_bottom,
            "bindings.session.scroll_bottom",
        )?,
        dashboard_up: parse_bindings(
            bindings.dashboard.up,
            defaults.dashboard_up,
            "bindings.dashboard.up",
        )?,
        dashboard_down: parse_bindings(
            bindings.dashboard.down,
            defaults.dashboard_down,
            "bindings.dashboard.down",
        )?,
        dashboard_view: parse_bindings(
            bindings.dashboard.view,
            defaults.dashboard_view,
            "bindings.dashboard.view",
        )?,
        dashboard_take: parse_bindings(
            bindings.dashboard.take,
            defaults.dashboard_take,
            "bindings.dashboard.take",
        )?,
        dashboard_search: parse_bindings(
            bindings.dashboard.search,
            defaults.dashboard_search,
            "bindings.dashboard.search",
        )?,
        dashboard_new: parse_bindings(
            bindings.dashboard.new,
            defaults.dashboard_new,
            "bindings.dashboard.new",
        )?,
        dashboard_rename: parse_bindings(
            bindings.dashboard.rename,
            defaults.dashboard_rename,
            "bindings.dashboard.rename",
        )?,
        dashboard_delete: parse_bindings(
            bindings.dashboard.delete,
            defaults.dashboard_delete,
            "bindings.dashboard.delete",
        )?,
        dashboard_stop: parse_bindings(
            bindings.dashboard.stop,
            defaults.dashboard_stop,
            "bindings.dashboard.stop",
        )?,
        dashboard_keybindings: parse_bindings(
            bindings.dashboard.keybindings,
            defaults.dashboard_keybindings,
            "bindings.dashboard.keybindings",
        )?,
        dashboard_close: parse_bindings(
            bindings.dashboard.close,
            defaults.dashboard_close,
            "bindings.dashboard.close",
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
                value.parse::<KeyBinding>().map_err(|error| {
                    let path = if field.starts_with("bindings.") {
                        field.to_owned()
                    } else {
                        format!("bindings.{field}")
                    };
                    invalid(format!("{path}: {error}"))
                })
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
                parse_key_modifiers(value).map_err(|error| {
                    let path = if field.starts_with("bindings.") {
                        field.to_owned()
                    } else {
                        format!("bindings.{field}")
                    };
                    invalid(format!("{path}: {error}"))
                })
            })
            .collect()
    })
}

fn validate_bindings(bindings: &ShellBindings) -> io::Result<()> {
    bindings
        .validate()
        .map_err(|error| invalid(error.to_string()))
}

fn modifier_label(modifiers: KeyModifiers) -> String {
    key_modifiers_config_label(modifiers).unwrap_or_else(|_| format!("{modifiers:?}"))
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
        .map(|binding| {
            let label = binding
                .config_label()
                .unwrap_or_else(|_| binding.label().to_ascii_lowercase());
            format!("\"{label}\"")
        })
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
    use turtletap::KeyCode;

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
    fn text_input_contexts_reject_unmodified_characters_and_arrows() {
        let defaults = ShellBindings::default();
        let character = ShellBindings {
            session_interrupt: vec![KeyBinding::new(KeyCode::Char('x'), KeyModifiers::empty())],
            ..defaults.clone()
        };
        let arrow = ShellBindings {
            action_next_screen: vec![KeyBinding::new(KeyCode::Right, KeyModifiers::empty())],
            ..defaults
        };

        assert!(
            validate_bindings(&character)
                .expect_err("unmodified session text should fail")
                .to_string()
                .contains("bindings.session.interrupt")
        );
        assert!(
            validate_bindings(&arrow)
                .expect_err("unmodified action-bar arrow should fail")
                .to_string()
                .contains("bindings.action.next-screen")
        );
    }

    #[test]
    fn single_keys_remain_valid_in_leader_and_dashboard_contexts() {
        let defaults = ShellBindings::default();
        let bindings = ShellBindings {
            leader_close: vec![KeyBinding::new(KeyCode::Char('z'), KeyModifiers::empty())],
            dashboard_view: vec![KeyBinding::new(KeyCode::Char('z'), KeyModifiers::empty())],
            ..defaults
        };

        validate_bindings(&bindings).expect("scoped single-key actions should remain valid");
    }

    #[test]
    fn unmodified_digit_groups_parse_for_leader_sequences() {
        assert_eq!(parse_key_modifiers("none"), Ok(KeyModifiers::empty()));
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
            KeyBinding::new(KeyCode::F(5), KeyModifiers::NONE)
        );
        assert_eq!(
            config.bindings.action_close[0],
            KeyBinding::new(KeyCode::Char('x'), KeyModifiers::ALT)
        );
        assert_eq!(
            config.bindings.dashboard_new[0],
            KeyBinding::new(KeyCode::Char('n'), KeyModifiers::empty())
        );
        assert_eq!(config.bindings, ShellBindings::default());
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
    fn legacy_flat_context_binding_names_remain_readable() {
        let kdl = "bindings {\n    session-interrupt \"ctrl-c\"\n}\n";
        let toml = "[bindings]\nsession_interrupt = [\"ctrl-c\"]\n";

        let kdl_config = resolve(
            "test",
            parse_kdl(kdl, Path::new("config.kdl")).expect("legacy flat KDL should parse"),
        )
        .expect("legacy flat KDL should resolve");
        let toml_config = resolve(
            "test",
            parse_toml(toml, Path::new("config.toml")).expect("legacy flat TOML should parse"),
        )
        .expect("legacy flat TOML should resolve");

        assert_eq!(
            kdl_config.bindings.session_interrupt,
            toml_config.bindings.session_interrupt
        );
    }

    #[test]
    fn duplicate_nested_and_legacy_binding_names_are_rejected() {
        let kdl = "bindings {\n    session-interrupt \"ctrl-c\"\n    session {\n        interrupt \"ctrl-x\"\n    }\n}\n";
        let toml = "[bindings]\nsession_interrupt = [\"ctrl-c\"]\n[bindings.session]\ninterrupt = [\"ctrl-x\"]\n";

        let kdl_error =
            parse_kdl(kdl, Path::new("config.kdl")).expect_err("duplicate KDL should fail");
        let toml_error =
            parse_toml(toml, Path::new("config.toml")).expect_err("duplicate TOML should fail");

        assert!(kdl_error.to_string().contains("more than once"));
        assert!(
            toml_error
                .to_string()
                .contains("nested and legacy flat form")
        );
    }

    #[test]
    fn kdl_binding_update_preserves_surrounding_comments_and_resolves() {
        let source = "// keep me\nbindings {\n    palette \"ctrl-p\" // keep this too\n}\n";
        let updated = update_kdl_binding(
            source,
            None,
            "palette",
            "ctrl-space",
            Path::new("config.kdl"),
        )
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
        let updated = update_toml_binding(
            source,
            None,
            "palette",
            "ctrl-space",
            Path::new("config.toml"),
        )
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
    fn scoped_binding_updates_stay_inside_their_kdl_group() {
        let source = "bindings {\n    session {\n        interrupt \"ctrl-c\" // keep\n    }\n}\n";
        let updated = update_kdl_binding(
            source,
            Some("session"),
            "interrupt",
            "ctrl-x",
            Path::new("config.kdl"),
        )
        .expect("scoped binding should update");

        assert!(updated.contains("session {"));
        assert!(updated.contains("interrupt \"ctrl-x\""));
        assert!(updated.contains("// keep"));
        let settings =
            parse_kdl(&updated, Path::new("config.kdl")).expect("updated KDL should parse");
        let config = resolve("test", settings).expect("updated KDL should resolve");
        assert_eq!(
            config.bindings.session_interrupt,
            vec![KeyBinding::new(KeyCode::Char('x'), KeyModifiers::CONTROL)]
        );
    }

    #[test]
    fn scoped_binding_updates_stay_inside_their_toml_table() {
        let source = "[bindings.session]\ninterrupt = [\"ctrl-c\"] # keep\n\n[bindings.dashboard]\nview = [\"v\"]\n";
        let updated = update_toml_binding(
            source,
            Some("session"),
            "interrupt",
            "ctrl-x",
            Path::new("config.toml"),
        )
        .expect("scoped binding should update");

        assert!(updated.contains("[bindings.session]"));
        assert!(updated.contains("interrupt = [\"ctrl-x\"]"));
        assert!(updated.contains("# keep"));
        assert!(updated.contains("[bindings.dashboard]"));
        let settings: SettingsFile = ::toml::from_str(&updated).expect("updated TOML should parse");
        let config = resolve("test", settings).expect("updated TOML should resolve");
        assert_eq!(
            config.bindings.session_interrupt,
            vec![KeyBinding::new(KeyCode::Char('x'), KeyModifiers::CONTROL)]
        );
    }

    #[test]
    fn scoped_binding_updates_preserve_legacy_flat_layout() {
        let kdl = "bindings {\n    session-interrupt \"ctrl-c\" // keep\n}\n";
        let toml = "[bindings]\nsession_interrupt = [\"ctrl-c\"] # keep\n";

        let updated_kdl = update_kdl_binding(
            kdl,
            Some("session"),
            "interrupt",
            "ctrl-x",
            Path::new("config.kdl"),
        )
        .expect("legacy KDL binding should update");
        let updated_toml = update_toml_binding(
            toml,
            Some("session"),
            "interrupt",
            "ctrl-x",
            Path::new("config.toml"),
        )
        .expect("legacy TOML binding should update");

        assert!(updated_kdl.contains("session-interrupt \"ctrl-x\""));
        assert!(!updated_kdl.contains("session {"));
        assert!(updated_kdl.contains("// keep"));
        assert!(updated_toml.contains("session_interrupt = [\"ctrl-x\"]"));
        assert!(!updated_toml.contains("[bindings.session]"));
        assert!(updated_toml.contains("# keep"));
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
