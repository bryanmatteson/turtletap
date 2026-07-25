//! KDL configuration parsing and rendering.

use std::{
    io::{self},
    path::Path,
};

use turtletap::{KeyBinding, ShellConfig};

use super::*;

pub(super) const KDL_TEMPLATE: &str = r#"// TurtleTap settings

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
    leaders "ctrl-g"
    palette "ctrl-`" "ctrl-space"
    redraw "f5"
    next-screen
    previous-screen
    jump-modifiers

    shell {
        detach "ctrl-d"
        next-screen "tab"
        previous-screen "backtab"
        help "?" "f1" "alt-h"
    }

    leader {
        palette "s"
        next-screen "n" "tab" "right"
        previous-screen "p" "backtab" "left"
        scroll-up "k" "up"
        scroll-down "j" "down"
        close "x"
        detach "d"
        help "?" "h"
        jump-modifiers "none"
    }

    action {
        next-screen "alt-right"
        previous-screen "alt-left"
        scroll-up "alt-up"
        scroll-down "alt-down"
        close "alt-x"
        detach "alt-d"
        help "alt-?"
        clear-query "ctrl-u"
        jump-modifiers "alt"
    }

    session {
        release-driver "f2"
        take-driver "f3"
        clear "ctrl-l" "cmd-k"
        interrupt "ctrl-c"
        detach "ctrl-d"
        delete-to-start "ctrl-u" "cmd-backspace"
        word-left "alt-b" "alt-left"
        word-right "alt-f" "alt-right"
        line-start "ctrl-a" "cmd-left"
        line-end "ctrl-e" "cmd-right"
        delete-word "ctrl-w" "alt-backspace"
        complete "tab"
        scroll-up "pageup"
        scroll-down "pagedown"
        scroll-top "ctrl-home"
        scroll-bottom "ctrl-end"
    }

    dashboard {
        up "k"
        down "j"
        view "v"
        take "t"
        search "/"
        new "n"
        rename "r"
        delete "x"
        stop "!"
        keybindings "b"
        close "q"
    }
}
"#;
pub(super) fn parse_kdl(source: &str, path: &Path) -> io::Result<SettingsFile> {
    let document = source.parse::<::kdl::KdlDocument>().map_err(|error| {
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

fn parse_kdl_shell(node: &::kdl::KdlNode, shell: &mut ShellSettings) -> Result<(), String> {
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
                    .as_integer()
                    .ok_or_else(|| "shell tick-rate-ms must be an integer".to_owned())?;
                shell.tick_rate_ms = Some(
                    u64::try_from(value)
                        .map_err(|_| "shell tick-rate-ms cannot be negative".to_owned())?,
                );
            }
            "chrome" => {
                shell.chrome = Some(
                    entry
                        .value()
                        .as_string()
                        .ok_or_else(|| "shell chrome must be a string".to_owned())?
                        .to_owned(),
                );
            }
            "rail-width" => shell.rail_width = Some(kdl_u16(entry.value(), "shell rail-width")?),
            "rail-narrow" => shell.rail_narrow = Some(kdl_u16(entry.value(), "shell rail-narrow")?),
            "rail-min-content" => {
                shell.rail_min_content = Some(kdl_u16(entry.value(), "shell rail-min-content")?);
            }
            unknown => return Err(format!("unknown shell property '{unknown}'")),
        }
    }
    Ok(())
}

fn parse_kdl_theme(node: &::kdl::KdlNode, theme: &mut ThemeSettings) -> Result<(), String> {
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

fn kdl_theme_color(node: &::kdl::KdlNode) -> Result<String, String> {
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
    node: &::kdl::KdlNode,
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

fn parse_kdl_bindings(node: &::kdl::KdlNode, bindings: &mut BindingSettings) -> Result<(), String> {
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
        match name {
            "leaders" => bindings.leaders = Some(kdl_binding_values(child, "bindings.leaders")?),
            "palette" => bindings.palette = Some(kdl_binding_values(child, "bindings.palette")?),
            "redraw" => bindings.redraw = Some(kdl_binding_values(child, "bindings.redraw")?),
            "next-screen" => {
                bindings.next_screen = Some(kdl_binding_values(child, "bindings.next-screen")?);
            }
            "previous-screen" => {
                bindings.previous_screen =
                    Some(kdl_binding_values(child, "bindings.previous-screen")?);
            }
            "jump-modifiers" => {
                bindings.jump_modifiers =
                    Some(kdl_binding_values(child, "bindings.jump-modifiers")?);
            }
            "shell" => parse_kdl_shell_bindings(child, &mut bindings.shell)?,
            "leader" => parse_kdl_leader_bindings(child, &mut bindings.leader)?,
            "action" => parse_kdl_action_bindings(child, &mut bindings.action)?,
            "session" => parse_kdl_session_bindings(child, &mut bindings.session)?,
            "dashboard" => parse_kdl_dashboard_bindings(child, &mut bindings.dashboard)?,
            legacy => {
                let Some((scope, name)) = legacy.split_once('-') else {
                    return Err(format!("unknown bindings child '{legacy}'"));
                };
                let values = kdl_binding_values(child, &format!("bindings.{legacy}"))?;
                set_kdl_group_binding(bindings, scope, name, values, true)?;
            }
        }
    }
    Ok(())
}

fn kdl_binding_values(node: &::kdl::KdlNode, path: &str) -> Result<Vec<String>, String> {
    if node
        .children()
        .is_some_and(|children| !children.nodes().is_empty())
    {
        return Err(format!("{path} accepts key strings, not child nodes"));
    }
    node.entries()
        .iter()
        .map(|entry| {
            if entry.name().is_some() {
                return Err(format!("{path} accepts string arguments, not properties"));
            }
            entry
                .value()
                .as_string()
                .map(str::to_owned)
                .ok_or_else(|| format!("{path} values must be strings"))
        })
        .collect()
}

fn kdl_binding_group(
    node: &::kdl::KdlNode,
    group: &str,
) -> Result<Vec<(String, Vec<String>)>, String> {
    if !node.entries().is_empty() {
        return Err(format!(
            "bindings.{group} accepts child nodes, not arguments"
        ));
    }
    let Some(children) = node.children() else {
        return Ok(Vec::new());
    };
    let mut seen = Vec::new();
    let mut values = Vec::new();
    for child in children.nodes() {
        let name = child.name().value();
        if seen.contains(&name) {
            return Err(format!("bindings.{group}.{name} appears more than once"));
        }
        seen.push(name);
        values.push((
            name.to_owned(),
            kdl_binding_values(child, &format!("bindings.{group}.{name}"))?,
        ));
    }
    Ok(values)
}

fn parse_kdl_shell_bindings(
    node: &::kdl::KdlNode,
    settings: &mut ShellBindingSettings,
) -> Result<(), String> {
    for (name, values) in kdl_binding_group(node, "shell")? {
        set_shell_binding(settings, &name, values, false)?;
    }
    Ok(())
}

fn parse_kdl_leader_bindings(
    node: &::kdl::KdlNode,
    settings: &mut LeaderBindingSettings,
) -> Result<(), String> {
    for (name, values) in kdl_binding_group(node, "leader")? {
        set_leader_binding(settings, &name, values, false)?;
    }
    Ok(())
}

fn parse_kdl_action_bindings(
    node: &::kdl::KdlNode,
    settings: &mut ActionBindingSettings,
) -> Result<(), String> {
    for (name, values) in kdl_binding_group(node, "action")? {
        set_action_binding(settings, &name, values, false)?;
    }
    Ok(())
}

fn parse_kdl_session_bindings(
    node: &::kdl::KdlNode,
    settings: &mut SessionBindingSettings,
) -> Result<(), String> {
    for (name, values) in kdl_binding_group(node, "session")? {
        set_session_binding(settings, &name, values, false)?;
    }
    Ok(())
}

fn parse_kdl_dashboard_bindings(
    node: &::kdl::KdlNode,
    settings: &mut DashboardBindingSettings,
) -> Result<(), String> {
    for (name, values) in kdl_binding_group(node, "dashboard")? {
        set_dashboard_binding(settings, &name, values, false)?;
    }
    Ok(())
}

fn set_kdl_group_binding(
    bindings: &mut BindingSettings,
    scope: &str,
    name: &str,
    values: Vec<String>,
    legacy: bool,
) -> Result<(), String> {
    match scope {
        "shell" => set_shell_binding(&mut bindings.shell, name, values, legacy),
        "leader" => set_leader_binding(&mut bindings.leader, name, values, legacy),
        "action" => set_action_binding(&mut bindings.action, name, values, legacy),
        "session" => set_session_binding(&mut bindings.session, name, values, legacy),
        "dashboard" => set_dashboard_binding(&mut bindings.dashboard, name, values, legacy),
        _ => Err(format!("unknown bindings child '{scope}-{name}'")),
    }
}

macro_rules! set_binding_field {
    ($settings:expr, $field:ident, $values:expr, $path:expr, $legacy:expr) => {{
        if $settings.$field.is_some() {
            let form = if $legacy {
                "nested and legacy flat form"
            } else {
                "more than once"
            };
            return Err(format!("{} is set in {form}", $path));
        }
        $settings.$field = Some($values);
        Ok(())
    }};
}

fn set_shell_binding(
    settings: &mut ShellBindingSettings,
    name: &str,
    values: Vec<String>,
    legacy: bool,
) -> Result<(), String> {
    match name {
        "detach" => set_binding_field!(settings, detach, values, "bindings.shell.detach", legacy),
        "next-screen" => set_binding_field!(
            settings,
            next_screen,
            values,
            "bindings.shell.next-screen",
            legacy
        ),
        "previous-screen" => set_binding_field!(
            settings,
            previous_screen,
            values,
            "bindings.shell.previous-screen",
            legacy
        ),
        "help" => set_binding_field!(settings, help, values, "bindings.shell.help", legacy),
        unknown => Err(format!("unknown bindings.shell child '{unknown}'")),
    }
}

fn set_leader_binding(
    settings: &mut LeaderBindingSettings,
    name: &str,
    values: Vec<String>,
    legacy: bool,
) -> Result<(), String> {
    match name {
        "palette" => {
            set_binding_field!(settings, palette, values, "bindings.leader.palette", legacy)
        }
        "next-screen" => set_binding_field!(
            settings,
            next_screen,
            values,
            "bindings.leader.next-screen",
            legacy
        ),
        "previous-screen" => set_binding_field!(
            settings,
            previous_screen,
            values,
            "bindings.leader.previous-screen",
            legacy
        ),
        "scroll-up" => set_binding_field!(
            settings,
            scroll_up,
            values,
            "bindings.leader.scroll-up",
            legacy
        ),
        "scroll-down" => set_binding_field!(
            settings,
            scroll_down,
            values,
            "bindings.leader.scroll-down",
            legacy
        ),
        "close" => set_binding_field!(settings, close, values, "bindings.leader.close", legacy),
        "detach" => set_binding_field!(settings, detach, values, "bindings.leader.detach", legacy),
        "help" => set_binding_field!(settings, help, values, "bindings.leader.help", legacy),
        "jump-modifiers" => set_binding_field!(
            settings,
            jump_modifiers,
            values,
            "bindings.leader.jump-modifiers",
            legacy
        ),
        unknown => Err(format!("unknown bindings.leader child '{unknown}'")),
    }
}

fn set_action_binding(
    settings: &mut ActionBindingSettings,
    name: &str,
    values: Vec<String>,
    legacy: bool,
) -> Result<(), String> {
    match name {
        "next-screen" => set_binding_field!(
            settings,
            next_screen,
            values,
            "bindings.action.next-screen",
            legacy
        ),
        "previous-screen" => set_binding_field!(
            settings,
            previous_screen,
            values,
            "bindings.action.previous-screen",
            legacy
        ),
        "scroll-up" => set_binding_field!(
            settings,
            scroll_up,
            values,
            "bindings.action.scroll-up",
            legacy
        ),
        "scroll-down" => set_binding_field!(
            settings,
            scroll_down,
            values,
            "bindings.action.scroll-down",
            legacy
        ),
        "close" => set_binding_field!(settings, close, values, "bindings.action.close", legacy),
        "detach" => set_binding_field!(settings, detach, values, "bindings.action.detach", legacy),
        "help" => set_binding_field!(settings, help, values, "bindings.action.help", legacy),
        "clear-query" => set_binding_field!(
            settings,
            clear_query,
            values,
            "bindings.action.clear-query",
            legacy
        ),
        "jump-modifiers" => set_binding_field!(
            settings,
            jump_modifiers,
            values,
            "bindings.action.jump-modifiers",
            legacy
        ),
        unknown => Err(format!("unknown bindings.action child '{unknown}'")),
    }
}

fn set_session_binding(
    settings: &mut SessionBindingSettings,
    name: &str,
    values: Vec<String>,
    legacy: bool,
) -> Result<(), String> {
    match name {
        "release-driver" => set_binding_field!(
            settings,
            release_driver,
            values,
            "bindings.session.release-driver",
            legacy
        ),
        "take-driver" => set_binding_field!(
            settings,
            take_driver,
            values,
            "bindings.session.take-driver",
            legacy
        ),
        "clear" => set_binding_field!(settings, clear, values, "bindings.session.clear", legacy),
        "interrupt" => set_binding_field!(
            settings,
            interrupt,
            values,
            "bindings.session.interrupt",
            legacy
        ),
        "detach" => set_binding_field!(settings, detach, values, "bindings.session.detach", legacy),
        "delete-to-start" => set_binding_field!(
            settings,
            delete_to_start,
            values,
            "bindings.session.delete-to-start",
            legacy
        ),
        "word-left" => set_binding_field!(
            settings,
            word_left,
            values,
            "bindings.session.word-left",
            legacy
        ),
        "word-right" => set_binding_field!(
            settings,
            word_right,
            values,
            "bindings.session.word-right",
            legacy
        ),
        "line-start" => set_binding_field!(
            settings,
            line_start,
            values,
            "bindings.session.line-start",
            legacy
        ),
        "line-end" => set_binding_field!(
            settings,
            line_end,
            values,
            "bindings.session.line-end",
            legacy
        ),
        "delete-word" => set_binding_field!(
            settings,
            delete_word,
            values,
            "bindings.session.delete-word",
            legacy
        ),
        "complete" => set_binding_field!(
            settings,
            complete,
            values,
            "bindings.session.complete",
            legacy
        ),
        "scroll-up" => set_binding_field!(
            settings,
            scroll_up,
            values,
            "bindings.session.scroll-up",
            legacy
        ),
        "scroll-down" => set_binding_field!(
            settings,
            scroll_down,
            values,
            "bindings.session.scroll-down",
            legacy
        ),
        "scroll-top" => set_binding_field!(
            settings,
            scroll_top,
            values,
            "bindings.session.scroll-top",
            legacy
        ),
        "scroll-bottom" => set_binding_field!(
            settings,
            scroll_bottom,
            values,
            "bindings.session.scroll-bottom",
            legacy
        ),
        unknown => Err(format!("unknown bindings.session child '{unknown}'")),
    }
}

fn set_dashboard_binding(
    settings: &mut DashboardBindingSettings,
    name: &str,
    values: Vec<String>,
    legacy: bool,
) -> Result<(), String> {
    match name {
        "up" => set_binding_field!(settings, up, values, "bindings.dashboard.up", legacy),
        "down" => set_binding_field!(settings, down, values, "bindings.dashboard.down", legacy),
        "view" => set_binding_field!(settings, view, values, "bindings.dashboard.view", legacy),
        "take" => set_binding_field!(settings, take, values, "bindings.dashboard.take", legacy),
        "search" => set_binding_field!(
            settings,
            search,
            values,
            "bindings.dashboard.search",
            legacy
        ),
        "new" => set_binding_field!(settings, new, values, "bindings.dashboard.new", legacy),
        "rename" => set_binding_field!(
            settings,
            rename,
            values,
            "bindings.dashboard.rename",
            legacy
        ),
        "delete" => set_binding_field!(
            settings,
            delete,
            values,
            "bindings.dashboard.delete",
            legacy
        ),
        "stop" => set_binding_field!(settings, stop, values, "bindings.dashboard.stop", legacy),
        "keybindings" => set_binding_field!(
            settings,
            keybindings,
            values,
            "bindings.dashboard.keybindings",
            legacy
        ),
        "close" => set_binding_field!(settings, close, values, "bindings.dashboard.close", legacy),
        unknown => Err(format!("unknown bindings.dashboard child '{unknown}'")),
    }
}

fn kdl_bool(value: &::kdl::KdlValue, field: &str) -> Result<bool, String> {
    value
        .as_bool()
        .ok_or_else(|| format!("{field} must be true or false"))
}

fn kdl_u16(value: &::kdl::KdlValue, field: &str) -> Result<u16, String> {
    let integer = value
        .as_integer()
        .ok_or_else(|| format!("{field} must be an integer"))?;
    u16::try_from(integer).map_err(|_| format!("{field} must be between 0 and 65535"))
}

pub(super) fn print_resolved_kdl(config: &ShellConfig) {
    let (chrome, width, narrow, min_content) = chrome_fields(config.chrome);
    if chrome == "rail" {
        println!(
            "shell mouse-capture={} direct-detach={} tick-rate-ms={} chrome=\"rail\" rail-width={width} rail-narrow={narrow} rail-min-content={min_content}",
            config.mouse_capture,
            config.direct_detach,
            config.tick_rate.as_millis(),
        );
    } else if chrome == "tabs" {
        println!(
            "shell mouse-capture={} direct-detach={} tick-rate-ms={} chrome=\"tabs\"",
            config.mouse_capture,
            config.direct_detach,
            config.tick_rate.as_millis(),
        );
    } else {
        println!(
            "shell mouse-capture={} direct-detach={} tick-rate-ms={} chrome=\"none\"",
            config.mouse_capture,
            config.direct_detach,
            config.tick_rate.as_millis(),
        );
    }
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
    print_kdl_modifier_node("jump-modifiers", &config.bindings.jump_modifiers);
    println!();
    println!("    shell {{");
    print_kdl_group_binding_node("detach", &config.bindings.shell_detach);
    print_kdl_group_binding_node("next-screen", &config.bindings.shell_next_screen);
    print_kdl_group_binding_node("previous-screen", &config.bindings.shell_previous_screen);
    print_kdl_group_binding_node("help", &config.bindings.shell_help);
    println!("    }}");
    println!();
    println!("    leader {{");
    print_kdl_group_binding_node("palette", &config.bindings.leader_palette);
    print_kdl_group_binding_node("next-screen", &config.bindings.leader_next_screen);
    print_kdl_group_binding_node("previous-screen", &config.bindings.leader_previous_screen);
    print_kdl_group_binding_node("scroll-up", &config.bindings.leader_scroll_up);
    print_kdl_group_binding_node("scroll-down", &config.bindings.leader_scroll_down);
    print_kdl_group_binding_node("close", &config.bindings.leader_close);
    print_kdl_group_binding_node("detach", &config.bindings.leader_detach);
    print_kdl_group_binding_node("help", &config.bindings.leader_help);
    print_kdl_group_modifier_node("jump-modifiers", &config.bindings.leader_jump_modifiers);
    println!("    }}");
    println!();
    println!("    action {{");
    print_kdl_group_binding_node("next-screen", &config.bindings.action_next_screen);
    print_kdl_group_binding_node("previous-screen", &config.bindings.action_previous_screen);
    print_kdl_group_binding_node("scroll-up", &config.bindings.action_scroll_up);
    print_kdl_group_binding_node("scroll-down", &config.bindings.action_scroll_down);
    print_kdl_group_binding_node("close", &config.bindings.action_close);
    print_kdl_group_binding_node("detach", &config.bindings.action_detach);
    print_kdl_group_binding_node("help", &config.bindings.action_help);
    print_kdl_group_binding_node("clear-query", &config.bindings.action_clear_query);
    print_kdl_group_modifier_node("jump-modifiers", &config.bindings.action_jump_modifiers);
    println!("    }}");
    println!();
    println!("    session {{");
    print_kdl_group_binding_node("release-driver", &config.bindings.session_release_driver);
    print_kdl_group_binding_node("take-driver", &config.bindings.session_take_driver);
    print_kdl_group_binding_node("clear", &config.bindings.session_clear);
    print_kdl_group_binding_node("interrupt", &config.bindings.session_interrupt);
    print_kdl_group_binding_node("detach", &config.bindings.session_detach);
    print_kdl_group_binding_node("delete-to-start", &config.bindings.session_delete_to_start);
    print_kdl_group_binding_node("word-left", &config.bindings.session_word_left);
    print_kdl_group_binding_node("word-right", &config.bindings.session_word_right);
    print_kdl_group_binding_node("line-start", &config.bindings.session_line_start);
    print_kdl_group_binding_node("line-end", &config.bindings.session_line_end);
    print_kdl_group_binding_node("delete-word", &config.bindings.session_delete_word);
    print_kdl_group_binding_node("complete", &config.bindings.session_complete);
    print_kdl_group_binding_node("scroll-up", &config.bindings.session_scroll_up);
    print_kdl_group_binding_node("scroll-down", &config.bindings.session_scroll_down);
    print_kdl_group_binding_node("scroll-top", &config.bindings.session_scroll_top);
    print_kdl_group_binding_node("scroll-bottom", &config.bindings.session_scroll_bottom);
    println!("    }}");
    println!();
    println!("    dashboard {{");
    print_kdl_group_binding_node("up", &config.bindings.dashboard_up);
    print_kdl_group_binding_node("down", &config.bindings.dashboard_down);
    print_kdl_group_binding_node("view", &config.bindings.dashboard_view);
    print_kdl_group_binding_node("take", &config.bindings.dashboard_take);
    print_kdl_group_binding_node("search", &config.bindings.dashboard_search);
    print_kdl_group_binding_node("new", &config.bindings.dashboard_new);
    print_kdl_group_binding_node("rename", &config.bindings.dashboard_rename);
    print_kdl_group_binding_node("delete", &config.bindings.dashboard_delete);
    print_kdl_group_binding_node("stop", &config.bindings.dashboard_stop);
    print_kdl_group_binding_node("keybindings", &config.bindings.dashboard_keybindings);
    print_kdl_group_binding_node("close", &config.bindings.dashboard_close);
    println!("    }}");
    println!("}}");
}

pub(super) fn print_kdl_binding_node(name: &str, bindings: &[KeyBinding]) {
    let values = bindings
        .iter()
        .map(|binding| {
            let label = binding
                .config_label()
                .unwrap_or_else(|_| binding.label().to_ascii_lowercase());
            format!(" \"{label}\"")
        })
        .collect::<String>();
    println!("    {name}{values}");
}

fn print_kdl_group_binding_node(name: &str, bindings: &[KeyBinding]) {
    let values = bindings
        .iter()
        .map(|binding| {
            let label = binding
                .config_label()
                .unwrap_or_else(|_| binding.label().to_ascii_lowercase());
            format!(" \"{label}\"")
        })
        .collect::<String>();
    println!("        {name}{values}");
}

fn print_kdl_modifier_node(name: &str, modifiers: &[KeyModifiers]) {
    let values = modifiers
        .iter()
        .map(|modifiers| format!(" \"{}\"", modifier_label(*modifiers)))
        .collect::<String>();
    println!("    {name}{values}");
}

fn print_kdl_group_modifier_node(name: &str, modifiers: &[KeyModifiers]) {
    let values = modifiers
        .iter()
        .map(|modifiers| format!(" \"{}\"", modifier_label(*modifiers)))
        .collect::<String>();
    println!("        {name}{values}");
}
