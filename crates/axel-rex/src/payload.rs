//! Pure builders for the JSON bodies sent to `rex api call`.
//!
//! Shapes were verified against `rex api describe <method> --json` (API
//! revision 7) and `rex api call block_creator.list` for the terminal flavor's
//! `options_schema`.  `session_id` is never included: the CLI injects it from
//! `-s SESSION`.

use std::path::Path;

use serde_json::{Map, Value, json};

use crate::{Error, TERMINAL_FLAVOR};

/// `/usr/bin/env K=V ... cmd args` — the only way to pass environment to a
/// block, since the terminal flavor's options have no env field.  An empty
/// `env` returns `command` unchanged.
pub fn wrap_env(command: &[String], env: &[(String, String)]) -> Result<Vec<String>, Error> {
    if env.is_empty() {
        return Ok(command.to_vec());
    }
    let mut out = Vec::with_capacity(command.len() + env.len() + 1);
    out.push("/usr/bin/env".to_string());
    for (k, v) in env {
        if k.is_empty() || k.contains('=') || k.bytes().any(|b| b == 0 || b.is_ascii_whitespace()) {
            return Err(Error::Invalid(format!(
                "environment variable name {k:?} is not valid"
            )));
        }
        out.push(format!("{k}={v}"));
    }
    out.extend(command.iter().cloned());
    Ok(out)
}

/// A `{"block": {...}}` layout node running `command` directly (`shell: "none"`)
/// in a terminal labelled `label`, closing when the program exits.
pub fn terminal_block(
    label: &str,
    command: &[String],
    cwd: Option<&Path>,
    env: &[(String, String)],
) -> Result<Value, Error> {
    if command.is_empty() || command[0].is_empty() {
        return Err(Error::Invalid("command must name a program".into()));
    }
    let label = label.trim();
    if label.chars().count() > 128 || label.chars().any(char::is_control) {
        return Err(Error::Invalid(
            "label must be ≤128 characters with no control characters".into(),
        ));
    }
    let mut options = Map::new();
    options.insert("shell".into(), json!("none"));
    options.insert("command".into(), json!(wrap_env(command, env)?));
    if let Some(cwd) = cwd {
        options.insert("cwd".into(), json!(cwd.to_string_lossy()));
    }
    options.insert("exit".into(), json!({"on_completion": true}));

    let mut block = Map::new();
    block.insert("flavor".into(), json!(TERMINAL_FLAVOR));
    if !label.is_empty() {
        block.insert("label".into(), json!(label));
    }
    block.insert("options".into(), Value::Object(options));
    Ok(json!({"block": Value::Object(block)}))
}

/// `session.new_layer`: a focused floating layer in the active window.
/// `bounds` is `[x, y, w, h]` as fractions of the window.
pub fn new_layer(
    label: &str,
    bounds: [f64; 4],
    command: &[String],
    cwd: Option<&Path>,
    env: &[(String, String)],
) -> Result<Value, Error> {
    new_layer_with_focus(label, bounds, command, cwd, env, true)
}

/// `session.new_layer` with an explicit focus flag (`false` opens the pane
/// behind the current focus, e.g. a HUD).
pub fn new_layer_with_focus(
    label: &str,
    bounds: [f64; 4],
    command: &[String],
    cwd: Option<&Path>,
    env: &[(String, String)],
    focus: bool,
) -> Result<Value, Error> {
    let [x, y, w, h] = bounds;
    if !(bounds.iter().all(|v| v.is_finite())
        && w > 0.0
        && h > 0.0
        && x >= 0.0
        && y >= 0.0
        && x + w <= 1.0 + 1e-9
        && y + h <= 1.0 + 1e-9)
    {
        return Err(Error::Invalid(format!(
            "bounds {bounds:?} must be fractions inside the window with positive size"
        )));
    }
    Ok(json!({
        "bounds": [x, y, w, h],
        "focus": focus,
        "layout": terminal_block(label, command, cwd, env)?,
    }))
}

/// `session.new_window`: a new window (tab) labelled `label` holding one terminal block.
pub fn new_window(
    label: &str,
    command: &[String],
    cwd: Option<&Path>,
    env: &[(String, String)],
    focus: bool,
) -> Result<Value, Error> {
    let layout = terminal_block(label, command, cwd, env)?;
    let mut body = Map::new();
    let label = label.trim();
    if !label.is_empty() {
        body.insert("window_label".into(), json!(label));
    }
    body.insert("focus".into(), json!(focus));
    body.insert("layout".into(), layout);
    Ok(Value::Object(body))
}

/// `session.new_window` with two terminals stacked: `top` above `bottom`,
/// `ratio` (0..1) of the height going to the top one.
pub fn new_window_stacked(
    label: &str,
    top: Value,
    bottom: Value,
    ratio: f64,
    focus: bool,
) -> Result<Value, Error> {
    if !(ratio > 0.0 && ratio < 1.0) {
        return Err(Error::Invalid("ratio must be between 0 and 1".into()));
    }
    let mut body = Map::new();
    let label = label.trim();
    if !label.is_empty() {
        body.insert("window_label".into(), json!(label));
    }
    body.insert("focus".into(), json!(focus));
    body.insert(
        "layout".into(),
        json!({"split": {"direction": "vertical", "before": top, "after": bottom, "ratio": ratio}}),
    );
    Ok(Value::Object(body))
}

/// `session.new_split`: a column to the right of `anchor` (a block or a split),
/// which keeps `keep` of the width, holding `top` above `bottom`; `top` gets
/// `stack` of the column's height.
pub fn new_split_right_stacked(anchor: &str, top: Value, bottom: Value, keep: f64, stack: f64, focus: bool) -> Result<Value, Error> {
    for r in [keep, stack] {
        if !(r > 0.0 && r < 1.0) {
            return Err(Error::Invalid("ratio must be between 0 and 1".into()));
        }
    }
    Ok(json!({
        "anchor_id": anchor,
        "direction": "horizontal",
        "side": "after",
        "ratio": keep,
        "focus": focus,
        "layout": {"split": {"direction": "vertical", "before": top, "after": bottom, "ratio": stack}},
    }))
}

/// `session.new_split`: one terminal right of `anchor` (a block or a split),
/// which keeps `keep` of the width.
pub fn new_split_right(anchor: &str, block: Value, keep: f64, focus: bool) -> Result<Value, Error> {
    if !(keep > 0.0 && keep < 1.0) {
        return Err(Error::Invalid("ratio must be between 0 and 1".into()));
    }
    Ok(json!({ "anchor_id": anchor, "direction": "horizontal", "side": "after", "ratio": keep, "focus": focus, "layout": block }))
}

/// Two layout nodes side by side, `left` keeping `keep` of the width.
pub fn row(left: Value, right: Value, keep: f64) -> Result<Value, Error> {
    if !(keep > 0.0 && keep < 1.0) {
        return Err(Error::Invalid("ratio must be between 0 and 1".into()));
    }
    Ok(json!({ "split": { "direction": "horizontal", "before": left, "after": right, "ratio": keep } }))
}

/// `session.new_split`: one terminal below `anchor`, which keeps `ratio` of the height.
pub fn new_split_below(anchor: &str, block: Value, ratio: f64, focus: bool) -> Result<Value, Error> {
    if !(ratio > 0.0 && ratio < 1.0) {
        return Err(Error::Invalid("ratio must be between 0 and 1".into()));
    }
    Ok(json!({
        "anchor_id": anchor,
        "direction": "vertical",
        "side": "after",
        "ratio": ratio,
        "focus": focus,
        "layout": block,
    }))
}

/// `session.close_layer`
pub fn close_layer(layer_id: &str) -> Value {
    json!({"layer_id": layer_id})
}

/// `com.superlogical.terminal.size` block method: `{block_id, args: {}}`.
pub fn terminal_size(block_id: &str) -> Value {
    json!({"block_id": block_id, "args": {}})
}

/// `session.resize_layer`: move/resize a floating layer to `bounds` fractions.
pub fn resize_layer(layer_id: &str, bounds: [f64; 4]) -> Value {
    json!({"layer_id": layer_id, "bounds": bounds})
}

/// `session.set_block_label` (Rex trims whitespace itself; we do too so the
/// payload matches what ends up stored).
pub fn set_block_label(block_id: &str, label: &str) -> Value {
    json!({"block_id": block_id, "label": label.trim()})
}

/// `session.set_window_label`
pub fn set_window_label(window_id: &str, label: &str) -> Value {
    json!({"window_id": window_id, "label": label.trim()})
}

/// `session.focus_window`
pub fn focus_window(window_id: &str) -> Value {
    json!({"window_id": window_id})
}

/// `com.superlogical.terminal.program_status` (a block method: its own
/// arguments go under `args`, which it has none of).
pub fn program_status(block_id: &str) -> Value {
    json!({"block_id": block_id, "args": {}})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn wrap_env_prefixes_with_usr_bin_env() {
        let wrapped = wrap_env(
            &cmd(&["claude", "--model", "sonnet"]),
            &[("A".into(), "1".into()), ("B".into(), "x y".into())],
        )
        .unwrap();
        assert_eq!(
            wrapped,
            cmd(&[
                "/usr/bin/env",
                "A=1",
                "B=x y",
                "claude",
                "--model",
                "sonnet"
            ])
        );
        assert_eq!(wrap_env(&cmd(&["sh"]), &[]).unwrap(), cmd(&["sh"]));
        assert!(wrap_env(&cmd(&["sh"]), &[("BAD=KEY".into(), "v".into())]).is_err());
        assert!(wrap_env(&cmd(&["sh"]), &[(String::new(), "v".into())]).is_err());
    }

    #[test]
    fn terminal_block_shape() {
        let b = terminal_block("lbl", &cmd(&["axel", "ui"]), Some(Path::new("/w")), &[]).unwrap();
        assert_eq!(
            b,
            json!({"block": {
                "flavor": "com.superlogical.terminal.shell",
                "label": "lbl",
                "options": {"shell": "none", "command": ["axel", "ui"], "cwd": "/w", "exit": {"on_completion": true}}
            }})
        );
        // No cwd → key omitted (empty cwd would mean $HOME anyway).
        let b = terminal_block("", &cmd(&["sh"]), None, &[]).unwrap();
        assert!(b["block"]["options"].get("cwd").is_none());
        assert!(
            b["block"].get("label").is_none(),
            "empty label omitted so Rex picks a default"
        );
    }

    #[test]
    fn terminal_block_rejects_bad_input() {
        assert!(matches!(
            terminal_block("x", &[], None, &[]),
            Err(Error::Invalid(_))
        ));
        assert!(matches!(
            terminal_block("x", &cmd(&[""]), None, &[]),
            Err(Error::Invalid(_))
        ));
        assert!(matches!(
            terminal_block("a\nb", &cmd(&["sh"]), None, &[]),
            Err(Error::Invalid(_))
        ));
        assert!(matches!(
            terminal_block(&"x".repeat(129), &cmd(&["sh"]), None, &[]),
            Err(Error::Invalid(_))
        ));
    }

    #[test]
    fn new_layer_shape_and_bounds_validation() {
        let v = new_layer("axel", [0.15, 0.10, 0.70, 0.80], &cmd(&["axel"]), None, &[]).unwrap();
        assert_eq!(v["bounds"], json!([0.15, 0.10, 0.70, 0.80]));
        assert_eq!(v["focus"], json!(true));
        assert_eq!(v["layout"]["block"]["flavor"], json!(TERMINAL_FLAVOR));
        assert!(v.get("session_id").is_none());
        for bad in [
            [0.0, 0.0, 0.0, 1.0],
            [0.5, 0.0, 0.6, 1.0],
            [-0.1, 0.0, 0.5, 0.5],
            [0.0, 0.0, f64::NAN, 1.0],
        ] {
            assert!(
                new_layer("x", bad, &cmd(&["sh"]), None, &[]).is_err(),
                "{bad:?}"
            );
        }
        assert!(new_layer("x", [0.0, 0.0, 1.0, 1.0], &cmd(&["sh"]), None, &[]).is_ok());
    }

    #[test]
    fn new_window_shape() {
        let v = new_window(
            "atlas · title",
            &cmd(&["claude"]),
            Some(Path::new("/p")),
            &[("AXEL_THREAD".into(), "t1".into())],
            true,
        )
        .unwrap();
        assert_eq!(
            v,
            json!({
                "window_label": "atlas · title",
                "focus": true,
                "layout": {"block": {
                    "flavor": "com.superlogical.terminal.shell",
                    "label": "atlas · title",
                    "options": {
                        "shell": "none",
                        "command": ["/usr/bin/env", "AXEL_THREAD=t1", "claude"],
                        "cwd": "/p",
                        "exit": {"on_completion": true}
                    }
                }}
            })
        );
        let v = new_window("", &cmd(&["sh"]), None, &[], false).unwrap();
        assert!(v.get("window_label").is_none());
        assert_eq!(v["focus"], json!(false));
    }

    #[test]
    fn small_payloads() {
        assert_eq!(close_layer("layer:1"), json!({"layer_id": "layer:1"}));
        assert_eq!(
            set_block_label("block:1", " a "),
            json!({"block_id": "block:1", "label": "a"})
        );
        assert_eq!(
            set_window_label("window:1", "w"),
            json!({"window_id": "window:1", "label": "w"})
        );
        assert_eq!(focus_window("window:1"), json!({"window_id": "window:1"}));
        assert_eq!(
            program_status("block:1"),
            json!({"block_id": "block:1", "args": {}})
        );
    }
}
