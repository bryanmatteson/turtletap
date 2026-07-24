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
    palette "ctrl-`" "ctrl-space" "ctrl-p"
    redraw "ctrl-/" "ctrl-_"
    next-screen
    previous-screen
    jump-modifiers

    // Shortcuts active on shell-managed surfaces.
    shell-detach "ctrl-d"
    shell-next-screen "tab"
    shell-previous-screen "backtab"
    shell-help "?"

    // Keys pressed after the leader.
    leader-palette "s"
    leader-next-screen "n" "tab" "right"
    leader-previous-screen "p" "backtab" "left"
    leader-scroll-up "k" "up"
    leader-scroll-down "j" "down"
    leader-close "x"
    leader-detach "d"
    leader-help "?" "h"
    leader-jump-modifiers "none"

    // Accelerators active only while the action bar is open.
    action-next-screen "alt-right"
    action-previous-screen "alt-left"
    action-scroll-up "alt-up"
    action-scroll-down "alt-down"
    action-close "alt-x"
    action-detach "alt-d"
    action-help "alt-?"
    action-clear-query "ctrl-u"
    action-jump-modifiers "alt"

    // Resident command-session shortcuts.
    session-release-driver "f2"
    session-take-driver "f3"
    session-clear "cmd-k" "ctrl-l"
    session-interrupt "ctrl-c"
    session-detach "ctrl-d"
    session-delete-to-start "ctrl-u" "cmd-backspace"
    session-word-left "alt-left" "alt-b"
    session-word-right "alt-right" "alt-f"
    session-line-start "cmd-left"
    session-line-end "cmd-right"
    session-delete-word "alt-backspace"
    session-complete "tab"
    session-scroll-up "pageup"
    session-scroll-down "pagedown"
    session-scroll-top "ctrl-home"
    session-scroll-bottom "ctrl-end"

    // Overview/dashboard action shortcuts.
    dashboard-up "k"
    dashboard-down "j"
    dashboard-view "v"
    dashboard-take "t"
    dashboard-search "/"
    dashboard-new "n"
    dashboard-rename "r"
    dashboard-delete "x"
    dashboard-stop "!"
    dashboard-keybindings "b"
    dashboard-close "q"
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
                    .as_i64()
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
            "shell-detach" => bindings.shell_detach = Some(values),
            "shell-next-screen" => bindings.shell_next_screen = Some(values),
            "shell-previous-screen" => bindings.shell_previous_screen = Some(values),
            "shell-help" => bindings.shell_help = Some(values),
            "leader-palette" => bindings.leader_palette = Some(values),
            "leader-next-screen" => bindings.leader_next_screen = Some(values),
            "leader-previous-screen" => bindings.leader_previous_screen = Some(values),
            "leader-scroll-up" => bindings.leader_scroll_up = Some(values),
            "leader-scroll-down" => bindings.leader_scroll_down = Some(values),
            "leader-close" => bindings.leader_close = Some(values),
            "leader-detach" => bindings.leader_detach = Some(values),
            "leader-help" => bindings.leader_help = Some(values),
            "leader-jump-modifiers" => bindings.leader_jump_modifiers = Some(values),
            "action-next-screen" => bindings.action_next_screen = Some(values),
            "action-previous-screen" => bindings.action_previous_screen = Some(values),
            "action-scroll-up" => bindings.action_scroll_up = Some(values),
            "action-scroll-down" => bindings.action_scroll_down = Some(values),
            "action-close" => bindings.action_close = Some(values),
            "action-detach" => bindings.action_detach = Some(values),
            "action-help" => bindings.action_help = Some(values),
            "action-clear-query" => bindings.action_clear_query = Some(values),
            "action-jump-modifiers" => bindings.action_jump_modifiers = Some(values),
            "session-release-driver" => bindings.session_release_driver = Some(values),
            "session-take-driver" => bindings.session_take_driver = Some(values),
            "session-clear" => bindings.session_clear = Some(values),
            "session-interrupt" => bindings.session_interrupt = Some(values),
            "session-detach" => bindings.session_detach = Some(values),
            "session-delete-to-start" => bindings.session_delete_to_start = Some(values),
            "session-word-left" => bindings.session_word_left = Some(values),
            "session-word-right" => bindings.session_word_right = Some(values),
            "session-line-start" => bindings.session_line_start = Some(values),
            "session-line-end" => bindings.session_line_end = Some(values),
            "session-delete-word" => bindings.session_delete_word = Some(values),
            "session-complete" => bindings.session_complete = Some(values),
            "session-scroll-up" => bindings.session_scroll_up = Some(values),
            "session-scroll-down" => bindings.session_scroll_down = Some(values),
            "session-scroll-top" => bindings.session_scroll_top = Some(values),
            "session-scroll-bottom" => bindings.session_scroll_bottom = Some(values),
            "dashboard-up" => bindings.dashboard_up = Some(values),
            "dashboard-down" => bindings.dashboard_down = Some(values),
            "dashboard-view" => bindings.dashboard_view = Some(values),
            "dashboard-take" => bindings.dashboard_take = Some(values),
            "dashboard-search" => bindings.dashboard_search = Some(values),
            "dashboard-new" => bindings.dashboard_new = Some(values),
            "dashboard-rename" => bindings.dashboard_rename = Some(values),
            "dashboard-delete" => bindings.dashboard_delete = Some(values),
            "dashboard-stop" => bindings.dashboard_stop = Some(values),
            "dashboard-keybindings" => bindings.dashboard_keybindings = Some(values),
            "dashboard-close" => bindings.dashboard_close = Some(values),
            unknown => return Err(format!("unknown bindings child '{unknown}'")),
        }
    }
    Ok(())
}

fn kdl_bool(value: &::kdl::KdlValue, field: &str) -> Result<bool, String> {
    value
        .as_bool()
        .ok_or_else(|| format!("{field} must be true or false"))
}

fn kdl_u16(value: &::kdl::KdlValue, field: &str) -> Result<u16, String> {
    let integer = value
        .as_i64()
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
    } else {
        println!(
            "shell mouse-capture={} direct-detach={} tick-rate-ms={} chrome=\"tabs\"",
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
    print_kdl_binding_node("shell-detach", &config.bindings.shell_detach);
    print_kdl_binding_node("shell-next-screen", &config.bindings.shell_next_screen);
    print_kdl_binding_node(
        "shell-previous-screen",
        &config.bindings.shell_previous_screen,
    );
    print_kdl_binding_node("shell-help", &config.bindings.shell_help);
    print_kdl_binding_node("leader-palette", &config.bindings.leader_palette);
    print_kdl_binding_node("leader-next-screen", &config.bindings.leader_next_screen);
    print_kdl_binding_node(
        "leader-previous-screen",
        &config.bindings.leader_previous_screen,
    );
    print_kdl_binding_node("leader-scroll-up", &config.bindings.leader_scroll_up);
    print_kdl_binding_node("leader-scroll-down", &config.bindings.leader_scroll_down);
    print_kdl_binding_node("leader-close", &config.bindings.leader_close);
    print_kdl_binding_node("leader-detach", &config.bindings.leader_detach);
    print_kdl_binding_node("leader-help", &config.bindings.leader_help);
    print_kdl_modifier_node(
        "leader-jump-modifiers",
        &config.bindings.leader_jump_modifiers,
    );
    print_kdl_binding_node("action-next-screen", &config.bindings.action_next_screen);
    print_kdl_binding_node(
        "action-previous-screen",
        &config.bindings.action_previous_screen,
    );
    print_kdl_binding_node("action-scroll-up", &config.bindings.action_scroll_up);
    print_kdl_binding_node("action-scroll-down", &config.bindings.action_scroll_down);
    print_kdl_binding_node("action-close", &config.bindings.action_close);
    print_kdl_binding_node("action-detach", &config.bindings.action_detach);
    print_kdl_binding_node("action-help", &config.bindings.action_help);
    print_kdl_binding_node("action-clear-query", &config.bindings.action_clear_query);
    print_kdl_modifier_node(
        "action-jump-modifiers",
        &config.bindings.action_jump_modifiers,
    );
    print_kdl_binding_node(
        "session-release-driver",
        &config.bindings.session_release_driver,
    );
    print_kdl_binding_node("session-take-driver", &config.bindings.session_take_driver);
    print_kdl_binding_node("session-clear", &config.bindings.session_clear);
    print_kdl_binding_node("session-interrupt", &config.bindings.session_interrupt);
    print_kdl_binding_node("session-detach", &config.bindings.session_detach);
    print_kdl_binding_node(
        "session-delete-to-start",
        &config.bindings.session_delete_to_start,
    );
    print_kdl_binding_node("session-word-left", &config.bindings.session_word_left);
    print_kdl_binding_node("session-word-right", &config.bindings.session_word_right);
    print_kdl_binding_node("session-line-start", &config.bindings.session_line_start);
    print_kdl_binding_node("session-line-end", &config.bindings.session_line_end);
    print_kdl_binding_node("session-delete-word", &config.bindings.session_delete_word);
    print_kdl_binding_node("session-complete", &config.bindings.session_complete);
    print_kdl_binding_node("session-scroll-up", &config.bindings.session_scroll_up);
    print_kdl_binding_node("session-scroll-down", &config.bindings.session_scroll_down);
    print_kdl_binding_node("session-scroll-top", &config.bindings.session_scroll_top);
    print_kdl_binding_node(
        "session-scroll-bottom",
        &config.bindings.session_scroll_bottom,
    );
    print_kdl_binding_node("dashboard-up", &config.bindings.dashboard_up);
    print_kdl_binding_node("dashboard-down", &config.bindings.dashboard_down);
    print_kdl_binding_node("dashboard-view", &config.bindings.dashboard_view);
    print_kdl_binding_node("dashboard-take", &config.bindings.dashboard_take);
    print_kdl_binding_node("dashboard-search", &config.bindings.dashboard_search);
    print_kdl_binding_node("dashboard-new", &config.bindings.dashboard_new);
    print_kdl_binding_node("dashboard-rename", &config.bindings.dashboard_rename);
    print_kdl_binding_node("dashboard-delete", &config.bindings.dashboard_delete);
    print_kdl_binding_node("dashboard-stop", &config.bindings.dashboard_stop);
    print_kdl_binding_node(
        "dashboard-keybindings",
        &config.bindings.dashboard_keybindings,
    );
    print_kdl_binding_node("dashboard-close", &config.bindings.dashboard_close);
    println!("}}");
}

pub(super) fn print_kdl_binding_node(name: &str, bindings: &[KeyBinding]) {
    let values = bindings
        .iter()
        .map(|binding| format!(" \"{}\"", binding.label().to_ascii_lowercase()))
        .collect::<String>();
    println!("    {name}{values}");
}

fn print_kdl_modifier_node(name: &str, modifiers: &[KeyModifiers]) {
    let values = modifiers
        .iter()
        .map(|modifiers| format!(" \"{}\"", modifier_label(*modifiers)))
        .collect::<String>();
    println!("    {name}{values}");
}
