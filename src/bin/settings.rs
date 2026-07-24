//! User-facing shell settings and keybinding configuration.

use std::{
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use serde::Deserialize;
use turtletap::{
    KeyBinding, KeyCode, KeyModifiers, ShellBindings, ShellConfig, Theme,
    tui::style::{Color, Style},
};

const TOML_TEMPLATE: &str = r#"# TurtleTap settings

[shell]
mouse_capture = false
direct_detach = true
tick_rate_ms = 100

[theme]
chrome = "white"
muted = "dark-gray"
accent = "cyan"
working = "blue"
attention = "yellow"
failed = "red"
complete = "green"

[theme.selected]
foreground = "black"
background = "cyan"

[bindings]
# The first entry is shown in the footer and help. Additional entries are fallbacks.
leaders = ["ctrl-space", "ctrl-g"]
palette = ["ctrl-p"]
redraw = ["ctrl-/", "ctrl-_"]
next_screen = ["ctrl-pagedown"]
previous_screen = ["ctrl-pageup"]
jump_modifiers = ["alt"]
"#;

const KDL_TEMPLATE: &str = r#"// TurtleTap settings

shell mouse-capture=false direct-detach=true tick-rate-ms=100

theme {
    chrome "white"
    muted "dark-gray"
    selected foreground="black" background="cyan"
    accent "cyan"
    working "blue"
    attention "yellow"
    failed "red"
    complete "green"
}

bindings {
    // The first entry is shown in the footer and help. Others are fallbacks.
    leaders "ctrl-space" "ctrl-g"
    palette "ctrl-p"
    redraw "ctrl-/" "ctrl-_"
    next-screen "ctrl-pagedown"
    previous-screen "ctrl-pageup"
    jump-modifiers "alt"
}
"#;

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
}

pub(crate) fn shell_config(title: &str) -> io::Result<ShellConfig> {
    let location = active_location()?;
    let settings = read_settings(&location)?;
    resolve(title, settings)
}

pub(crate) fn command(arguments: &[String]) -> io::Result<()> {
    match arguments {
        [] => show_config(None),
        [help] if matches!(help.as_str(), "-h" | "--help" | "help") => print_help(None),
        [action, help] if matches!(help.as_str(), "-h" | "--help") => print_help(Some(action)),
        [help, action] if help == "help" => print_help(Some(action)),
        [show] if show == "show" => show_config(None),
        [show, format] if show == "show" => show_config(Some(ConfigFormat::parse(format)?)),
        [path] if path == "path" => {
            println!("{}", active_location()?.path.display());
            Ok(())
        }
        [check] if check == "check" => {
            let location = active_location()?;
            let _ = shell_config("TurtleTap")?;
            if location.path.exists() {
                println!("Configuration is valid: {}", location.path.display());
            } else {
                println!(
                    "No configuration file at {}; built-in defaults are valid.",
                    location.path.display()
                );
            }
            Ok(())
        }
        [init] if init == "init" => init_config(ConfigFormat::Kdl),
        [init, format] if init == "init" => init_config(ConfigFormat::parse(format)?),
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
             Actions:\n  show [kdl|toml]  Print resolved settings; optionally translate formats\n  path             Print the active configuration path\n  check            Validate the active configuration\n  init [kdl|toml]  Create a starter file; defaults to KDL\n\n\
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
            "Create a commented starter configuration without overwriting a file.\n\n\
             Usage:\n  turtletap config init [kdl|toml]\n\n\
             KDL is used when the format is omitted."
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

fn init_config(format: ConfigFormat) -> io::Result<()> {
    let location = location_for(format)?;
    let path = location.path;
    if path.exists() {
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
        ConfigFormat::Toml => toml::from_str(&source).map_err(|error| {
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

fn parse_kdl(source: &str, path: &Path) -> io::Result<SettingsFile> {
    let document = source.parse::<kdl::KdlDocument>().map_err(|error| {
        invalid(format!(
            "invalid KDL configuration at {}: {error}",
            path.display()
        ))
    })?;
    let mut settings = SettingsFile::default();
    let mut saw_shell = false;
    let mut saw_theme = false;
    let mut saw_bindings = false;
    for node in document.nodes() {
        match node.name().value() {
            "shell" if !saw_shell => {
                saw_shell = true;
                parse_kdl_shell(node, &mut settings.shell).map_err(|message| {
                    invalid(format!(
                        "invalid configuration at {}: {message}",
                        path.display()
                    ))
                })?;
            }
            "bindings" if !saw_bindings => {
                saw_bindings = true;
                parse_kdl_bindings(node, &mut settings.bindings).map_err(|message| {
                    invalid(format!(
                        "invalid configuration at {}: {message}",
                        path.display()
                    ))
                })?;
            }
            "theme" if !saw_theme => {
                saw_theme = true;
                parse_kdl_theme(node, &mut settings.theme).map_err(|message| {
                    invalid(format!(
                        "invalid configuration at {}: {message}",
                        path.display()
                    ))
                })?;
            }
            "shell" | "theme" | "bindings" => {
                return Err(invalid(format!(
                    "invalid configuration at {}: duplicate '{}' node",
                    path.display(),
                    node.name().value()
                )));
            }
            unknown => {
                return Err(invalid(format!(
                    "invalid configuration at {}: unknown top-level node '{unknown}'",
                    path.display()
                )));
            }
        }
    }
    Ok(settings)
}

fn parse_kdl_shell(node: &kdl::KdlNode, shell: &mut ShellSettings) -> Result<(), String> {
    if node
        .children()
        .is_some_and(|children| !children.nodes().is_empty())
    {
        return Err("shell does not accept child nodes".to_owned());
    }
    let mut seen = Vec::new();
    for entry in node.entries() {
        let name = entry
            .name()
            .map(|name| name.value())
            .ok_or_else(|| "shell accepts properties, not positional arguments".to_owned())?;
        if seen.contains(&name) {
            return Err(format!("shell property '{name}' appears more than once"));
        }
        seen.push(name);
        match name {
            "mouse-capture" => {
                shell.mouse_capture = Some(kdl_bool(entry.value(), "shell mouse-capture")?);
            }
            "direct-detach" => {
                shell.direct_detach = Some(kdl_bool(entry.value(), "shell direct-detach")?);
            }
            "tick-rate-ms" => {
                let value = entry
                    .value()
                    .as_i64()
                    .ok_or_else(|| "shell tick-rate-ms must be an integer".to_owned())?;
                shell.tick_rate_ms = Some(
                    u64::try_from(value)
                        .map_err(|_| "shell tick-rate-ms cannot be negative".to_owned())?,
                );
            }
            unknown => return Err(format!("unknown shell property '{unknown}'")),
        }
    }
    Ok(())
}

fn parse_kdl_theme(node: &kdl::KdlNode, theme: &mut ThemeSettings) -> Result<(), String> {
    if !node.entries().is_empty() {
        return Err("theme accepts child nodes, not inline entries".to_owned());
    }
    let Some(children) = node.children() else {
        return Ok(());
    };
    let mut seen = Vec::new();
    for child in children.nodes() {
        let name = child.name().value();
        if seen.contains(&name) {
            return Err(format!("theme child '{name}' appears more than once"));
        }
        seen.push(name);
        match name {
            "selected" => parse_kdl_selected_theme(child, &mut theme.selected)?,
            "chrome" => theme.chrome = Some(kdl_theme_color(child)?),
            "muted" => theme.muted = Some(kdl_theme_color(child)?),
            "accent" => theme.accent = Some(kdl_theme_color(child)?),
            "working" => theme.working = Some(kdl_theme_color(child)?),
            "attention" => theme.attention = Some(kdl_theme_color(child)?),
            "failed" => theme.failed = Some(kdl_theme_color(child)?),
            "complete" => theme.complete = Some(kdl_theme_color(child)?),
            unknown => return Err(format!("unknown theme child '{unknown}'")),
        }
    }
    Ok(())
}

fn kdl_theme_color(node: &kdl::KdlNode) -> Result<String, String> {
    if node
        .children()
        .is_some_and(|children| !children.nodes().is_empty())
    {
        return Err(format!(
            "theme child '{}' cannot have children",
            node.name().value()
        ));
    }
    let [entry] = node.entries() else {
        return Err(format!(
            "theme child '{}' requires exactly one color argument",
            node.name().value()
        ));
    };
    if entry.name().is_some() {
        return Err(format!(
            "theme child '{}' accepts a color argument, not a property",
            node.name().value()
        ));
    }
    entry.value().as_string().map(str::to_owned).ok_or_else(|| {
        format!(
            "theme child '{}' color must be a string",
            node.name().value()
        )
    })
}

fn parse_kdl_selected_theme(
    node: &kdl::KdlNode,
    selected: &mut SelectedThemeSettings,
) -> Result<(), String> {
    if node
        .children()
        .is_some_and(|children| !children.nodes().is_empty())
    {
        return Err("theme selected cannot have children".to_owned());
    }
    let mut seen = Vec::new();
    for entry in node.entries() {
        let name = entry
            .name()
            .map(|name| name.value())
            .ok_or_else(|| "theme selected accepts properties, not arguments".to_owned())?;
        if seen.contains(&name) {
            return Err(format!(
                "theme selected property '{name}' appears more than once"
            ));
        }
        seen.push(name);
        let value = entry
            .value()
            .as_string()
            .map(str::to_owned)
            .ok_or_else(|| format!("theme selected {name} must be a color string"))?;
        match name {
            "foreground" => selected.foreground = Some(value),
            "background" => selected.background = Some(value),
            unknown => return Err(format!("unknown theme selected property '{unknown}'")),
        }
    }
    Ok(())
}

fn parse_kdl_bindings(node: &kdl::KdlNode, bindings: &mut BindingSettings) -> Result<(), String> {
    if !node.entries().is_empty() {
        return Err("bindings accepts child nodes, not inline entries".to_owned());
    }
    let Some(children) = node.children() else {
        return Ok(());
    };
    let mut seen = Vec::new();
    for child in children.nodes() {
        let name = child.name().value();
        if seen.contains(&name) {
            return Err(format!("bindings child '{name}' appears more than once"));
        }
        seen.push(name);
        if child
            .children()
            .is_some_and(|nested| !nested.nodes().is_empty())
        {
            return Err(format!("bindings child '{name}' cannot have children"));
        }
        let values = child
            .entries()
            .iter()
            .map(|entry| {
                if entry.name().is_some() {
                    return Err(format!(
                        "bindings child '{name}' accepts string arguments, not properties"
                    ));
                }
                entry
                    .value()
                    .as_string()
                    .map(str::to_owned)
                    .ok_or_else(|| format!("bindings child '{name}' values must be strings"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        match name {
            "leaders" => bindings.leaders = Some(values),
            "palette" => bindings.palette = Some(values),
            "redraw" => bindings.redraw = Some(values),
            "next-screen" => bindings.next_screen = Some(values),
            "previous-screen" => bindings.previous_screen = Some(values),
            "jump-modifiers" => bindings.jump_modifiers = Some(values),
            unknown => return Err(format!("unknown bindings child '{unknown}'")),
        }
    }
    Ok(())
}

fn kdl_bool(value: &kdl::KdlValue, field: &str) -> Result<bool, String> {
    value
        .as_bool()
        .ok_or_else(|| format!("{field} must be true or false"))
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
        jump_modifiers: parse_modifiers(bindings.jump_modifiers, defaults.jump_modifiers)?,
    };
    validate_bindings(&bindings)?;
    config.bindings = bindings;
    Ok(config)
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
) -> io::Result<Vec<KeyModifiers>> {
    configured.map_or(Ok(defaults), |values| {
        values
            .iter()
            .map(|value| {
                parse_modifier_set(value)
                    .map_err(|message| invalid(format!("bindings.jump_modifiers: {message}")))
            })
            .collect()
    })
}

fn validate_bindings(bindings: &ShellBindings) -> io::Result<()> {
    if bindings.leaders.is_empty() && bindings.palette.is_empty() {
        return Err(invalid(
            "bindings.leaders and bindings.palette cannot both be empty; captured screens need an escape hatch",
        ));
    }
    if bindings.next_screen.is_empty()
        && bindings.previous_screen.is_empty()
        && bindings.jump_modifiers.is_empty()
    {
        return Err(invalid(
            "configure at least one next_screen, previous_screen, or jump_modifiers binding",
        ));
    }
    if let Some(duplicate) =
        bindings
            .jump_modifiers
            .iter()
            .enumerate()
            .find_map(|(item, modifiers)| {
                bindings.jump_modifiers[item + 1..]
                    .contains(modifiers)
                    .then_some(*modifiers)
            })
    {
        let label = KeyBinding::new(KeyCode::Char('1'), duplicate).label();
        return Err(invalid(format!(
            "bindings.jump_modifiers contains {} more than once",
            label.trim_end_matches("-1")
        )));
    }
    let groups = [
        ("leaders", &bindings.leaders),
        ("palette", &bindings.palette),
        ("redraw", &bindings.redraw),
        ("next_screen", &bindings.next_screen),
        ("previous_screen", &bindings.previous_screen),
    ];
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
        for modifiers in &bindings.jump_modifiers {
            for digit in '1'..='9' {
                let jump = KeyBinding::new(KeyCode::Char(digit), *modifiers);
                if left.contains(&jump) {
                    return Err(invalid(format!(
                        "{} is assigned to both bindings.{left_name} and bindings.jump_modifiers",
                        jump.label()
                    )));
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
    let parts: Vec<&str> = normalized.split(['-', '+']).collect();
    let modifiers = parse_modifier_parts(&parts, value)?;
    if modifiers.is_empty() {
        return Err(format!("'{value}' must contain at least one modifier"));
    }
    Ok(modifiers)
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

fn print_resolved_toml(config: &ShellConfig) {
    println!("[shell]");
    println!("mouse_capture = {}", config.mouse_capture);
    println!("direct_detach = {}", config.direct_detach);
    println!("tick_rate_ms = {}", config.tick_rate.as_millis());
    println!();
    println!("[theme]");
    println!("chrome = \"{}\"", style_foreground(config.theme.chrome));
    println!("muted = \"{}\"", style_foreground(config.theme.muted));
    println!("accent = \"{}\"", style_foreground(config.theme.accent));
    println!("working = \"{}\"", style_foreground(config.theme.working));
    println!(
        "attention = \"{}\"",
        style_foreground(config.theme.attention)
    );
    println!("failed = \"{}\"", style_foreground(config.theme.failed));
    println!("complete = \"{}\"", style_foreground(config.theme.complete));
    println!();
    println!("[theme.selected]");
    println!(
        "foreground = \"{}\"",
        style_foreground(config.theme.selected)
    );
    println!("background = \"{}\"", color_name(config.theme.selected.bg));
    println!();
    println!("[bindings]");
    print_binding_list("leaders", &config.bindings.leaders);
    print_binding_list("palette", &config.bindings.palette);
    print_binding_list("redraw", &config.bindings.redraw);
    print_binding_list("next_screen", &config.bindings.next_screen);
    print_binding_list("previous_screen", &config.bindings.previous_screen);
    let modifiers = config
        .bindings
        .jump_modifiers
        .iter()
        .map(|modifiers| {
            KeyBinding::new(KeyCode::Char('1'), *modifiers)
                .label()
                .trim_end_matches("-1")
                .to_ascii_lowercase()
        })
        .map(|label| format!("\"{label}\""))
        .collect::<Vec<_>>()
        .join(", ");
    println!("jump_modifiers = [{modifiers}]");
}

fn print_resolved_kdl(config: &ShellConfig) {
    println!(
        "shell mouse-capture={} direct-detach={} tick-rate-ms={}",
        config.mouse_capture,
        config.direct_detach,
        config.tick_rate.as_millis()
    );
    println!();
    println!("theme {{");
    println!("    chrome \"{}\"", style_foreground(config.theme.chrome));
    println!("    muted \"{}\"", style_foreground(config.theme.muted));
    println!(
        "    selected foreground=\"{}\" background=\"{}\"",
        style_foreground(config.theme.selected),
        color_name(config.theme.selected.bg)
    );
    println!("    accent \"{}\"", style_foreground(config.theme.accent));
    println!("    working \"{}\"", style_foreground(config.theme.working));
    println!(
        "    attention \"{}\"",
        style_foreground(config.theme.attention)
    );
    println!("    failed \"{}\"", style_foreground(config.theme.failed));
    println!(
        "    complete \"{}\"",
        style_foreground(config.theme.complete)
    );
    println!("}}");
    println!();
    println!("bindings {{");
    print_kdl_binding_node("leaders", &config.bindings.leaders);
    print_kdl_binding_node("palette", &config.bindings.palette);
    print_kdl_binding_node("redraw", &config.bindings.redraw);
    print_kdl_binding_node("next-screen", &config.bindings.next_screen);
    print_kdl_binding_node("previous-screen", &config.bindings.previous_screen);
    let values = config
        .bindings
        .jump_modifiers
        .iter()
        .map(|modifiers| {
            KeyBinding::new(KeyCode::Char('1'), *modifiers)
                .label()
                .trim_end_matches("-1")
                .to_ascii_lowercase()
        })
        .map(|value| format!(" \"{value}\""))
        .collect::<String>();
    println!("    jump-modifiers{values}");
    println!("}}");
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

fn print_kdl_binding_node(name: &str, bindings: &[KeyBinding]) {
    let values = bindings
        .iter()
        .map(|binding| format!(" \"{}\"", binding.label().to_ascii_lowercase()))
        .collect::<String>();
    println!("    {name}{values}");
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
            parse_key_binding("Ctrl-PgDown"),
            Ok(KeyBinding::new(KeyCode::PageDown, KeyModifiers::CONTROL))
        );
        assert_eq!(
            parse_key_binding("ctrl-/"),
            Ok(KeyBinding::new(KeyCode::Char('/'), KeyModifiers::CONTROL))
        );
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
    fn empty_binding_lists_disable_an_action() {
        let source = "[bindings]\npalette = []\n";
        let settings: SettingsFile = toml::from_str(source).expect("settings should parse");
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
            KeyBinding::new(KeyCode::Char(' '), KeyModifiers::CONTROL)
        );
        assert_eq!(
            config.bindings.next_screen[0],
            KeyBinding::new(KeyCode::PageDown, KeyModifiers::CONTROL)
        );
        assert_eq!(
            config.bindings.redraw[0],
            KeyBinding::new(KeyCode::Char('/'), KeyModifiers::CONTROL)
        );
        assert_eq!(config.theme.selected.bg, Some(Color::Cyan));
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
            toml::from_str(toml).expect("TOML theme should parse"),
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
            toml::from_str("[theme]\naccent = \"ultraviolet\"\n").expect("TOML should parse");
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
