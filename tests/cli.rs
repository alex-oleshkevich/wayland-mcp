#![cfg(target_os = "linux")]

use std::process::{Command, Output};

fn cli(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_wayland-mcp"))
        .args(args)
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("WAYLAND_MCP_PORTAL")
        .env_remove("WAYLAND_MCP_TOUCH_UINPUT")
        .env_remove("WAYLAND_MCP_NO_INDICATOR")
        .output()
        .expect("run wayland-mcp")
}

#[test]
fn tools_name_prints_description_and_schema_without_a_display() {
    let result = cli(&["tools", "screenshot"]);
    assert!(result.status.success(), "{:?}", result);
    let text = String::from_utf8(result.stdout).unwrap();
    assert!(text.starts_with("screenshot\nCapture a screenshot:"), "{text}");
    let (_, schema) = text.split_once("ARGS_JSON schema:\n").expect("schema heading");
    let schema: serde_json::Value = serde_json::from_str(schema).unwrap();
    assert_eq!(schema["type"], "object");
    assert!(schema["properties"]["max_dimension"].is_object());
}

#[test]
fn tools_name_json_contains_the_router_input_schema() {
    let result = cli(&["tools", "click", "--json"]);
    assert!(result.status.success(), "{:?}", result);
    let tool: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(tool["name"], "click");
    assert!(tool["description"].as_str().unwrap().contains("Click"));
    assert_eq!(tool["input_schema"]["type"], "object");
    let required = tool["input_schema"]["required"].as_array().unwrap();
    assert!(required.contains(&serde_json::json!("x")));
    assert!(required.contains(&serde_json::json!("y")));
}

#[test]
fn tools_unknown_name_exits_one_with_json_error_and_suggestions() {
    for (name, suggestion) in [("nope", None), ("screensho", Some("screenshot"))] {
        let result = cli(&["tools", name]);
        assert_eq!(result.status.code(), Some(1), "{:?}", result);
        assert!(result.stdout.is_empty());
        let error: serde_json::Value = serde_json::from_slice(&result.stderr).unwrap();
        assert_eq!(error["code"], -32602);
        assert!(error["message"].as_str().unwrap().contains(&format!("unknown tool '{name}'")));
        if let Some(suggestion) = suggestion {
            assert!(error["data"]["suggestions"].as_array().unwrap().contains(&serde_json::json!(suggestion)));
        }
    }
}

#[test]
fn tools_filters_apply_to_schema_discovery_and_existing_names_listing() {
    let result = cli(&["--tools", "doctor,click", "tools", "--names"]);
    assert!(result.status.success(), "{:?}", result);
    assert_eq!(String::from_utf8(result.stdout).unwrap(), "click\ndoctor\n");
    let result = cli(&["--exclude-tools", "screenshot", "tools", "screenshot", "--json"]);
    assert_eq!(result.status.code(), Some(1));
    assert!(result.stdout.is_empty());
    let error: serde_json::Value = serde_json::from_slice(&result.stderr).unwrap();
    assert!(!error["data"]["suggestions"].as_array().unwrap().contains(&serde_json::json!("screenshot")));
}

#[test]
fn tools_rejects_conflicting_output_modes() {
    for args in [vec!["tools", "click", "--names"], vec!["tools", "--json"], vec!["tools", "--names", "--json"]] {
        assert_eq!(cli(&args).status.code(), Some(2), "{args:?}");
    }
}
