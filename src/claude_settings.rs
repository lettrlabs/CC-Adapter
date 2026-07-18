use serde_json::{Map, Value, json};

const BASE_URL_ENV: &str = "ANTHROPIC_BASE_URL";
const API_KEY_ENV: &str = "ANTHROPIC_API_KEY";
const TOOL_SEARCH_ENV: &str = "ENABLE_TOOL_SEARCH";
const STREAM_TIMEOUT_ENV: &str = "CLAUDE_STREAM_IDLE_TIMEOUT_MS";

pub const LOCAL_API_KEY: &str = "cc-adapter-local";
pub const TOOL_SEARCH_ENABLED: &str = "true";

const NON_OBJECT_ROOT_ERROR: &str = "Claude settings root must be a JSON object";
const NON_OBJECT_ENV_ERROR: &str = "Claude settings `env` must be a JSON object";

fn settings_object(settings: &Value) -> Result<&Map<String, Value>, &'static str> {
    settings.as_object().ok_or(NON_OBJECT_ROOT_ERROR)
}

fn settings_object_mut(settings: &mut Value) -> Result<&mut Map<String, Value>, &'static str> {
    settings.as_object_mut().ok_or(NON_OBJECT_ROOT_ERROR)
}

fn env_object(settings: &Value) -> Result<Option<&Map<String, Value>>, &'static str> {
    match settings_object(settings)?.get("env") {
        None => Ok(None),
        Some(Value::Object(env)) => Ok(Some(env)),
        Some(_) => Err(NON_OBJECT_ENV_ERROR),
    }
}

fn backup_entry(env: Option<&Map<String, Value>>, key: &str) -> Value {
    let old_value = env.and_then(|env| env.get(key)).cloned();
    json!({
        "had_value": old_value.is_some(),
        "old_value": old_value,
    })
}

pub fn apply_managed_env(
    settings: &mut Value,
    proxy_url: &str,
    stream_idle_timeout_ms: u64,
) -> Result<Value, &'static str> {
    let existing_env = env_object(settings)?;
    let env_was_present = existing_env.is_some();
    let mut backup = Map::new();
    backup.insert("env_was_present".to_string(), Value::Bool(env_was_present));
    backup.insert(
        "anthropic_base_url".to_string(),
        backup_entry(existing_env, BASE_URL_ENV),
    );
    backup.insert(
        "enable_tool_search".to_string(),
        backup_entry(existing_env, TOOL_SEARCH_ENV),
    );
    if stream_idle_timeout_ms > 0 {
        backup.insert(
            "claude_stream_idle_timeout_ms".to_string(),
            backup_entry(existing_env, STREAM_TIMEOUT_ENV),
        );
    }

    let settings = settings_object_mut(settings)?;
    let env = settings
        .entry("env".to_string())
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or(NON_OBJECT_ENV_ERROR)?;
    env.insert(
        BASE_URL_ENV.to_string(),
        Value::String(proxy_url.to_string()),
    );
    env.insert(
        TOOL_SEARCH_ENV.to_string(),
        Value::String(TOOL_SEARCH_ENABLED.to_string()),
    );
    if stream_idle_timeout_ms > 0 {
        env.insert(
            STREAM_TIMEOUT_ENV.to_string(),
            Value::String(stream_idle_timeout_ms.to_string()),
        );
    }

    Ok(Value::Object(backup))
}

fn restore_section(env: &mut Map<String, Value>, key: &str, section: Option<&Value>) {
    let Some(section) = section else { return };
    match section.get("had_value").and_then(Value::as_bool) {
        Some(true) => {
            if let Some(old_value) = section.get("old_value") {
                env.insert(key.to_string(), old_value.clone());
            }
        }
        Some(false) => {
            env.remove(key);
        }
        None => {}
    }
}

fn restore_api_key(env: &mut Map<String, Value>, section: Option<&Value>) {
    let Some(section) = section else { return };
    match section.get("injected").and_then(Value::as_bool) {
        Some(true) => {
            if env.get(API_KEY_ENV).and_then(Value::as_str) == Some(LOCAL_API_KEY) {
                env.remove(API_KEY_ENV);
            }
        }
        Some(false) => {}
        None => restore_section(env, API_KEY_ENV, Some(section)),
    }
}

pub fn restore_managed_env(
    settings: &mut Value,
    backup: Option<&Value>,
) -> Result<(), &'static str> {
    env_object(settings)?;

    if settings_object(settings)?.get("env").is_none() && backup.is_none() {
        return Ok(());
    }

    let env = settings_object_mut(settings)?
        .entry("env".to_string())
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or(NON_OBJECT_ENV_ERROR)?;

    match backup {
        None => {
            if env.get(TOOL_SEARCH_ENV).and_then(Value::as_str) == Some(TOOL_SEARCH_ENABLED) {
                env.remove(TOOL_SEARCH_ENV);
            }
        }
        Some(backup) => {
            let base = backup
                .get("anthropic_base_url")
                .or_else(|| backup.get("had_value").is_some().then_some(backup));
            restore_section(env, BASE_URL_ENV, base);
            restore_api_key(env, backup.get("anthropic_api_key"));
            restore_section(env, TOOL_SEARCH_ENV, backup.get("enable_tool_search"));
            restore_section(
                env,
                STREAM_TIMEOUT_ENV,
                backup.get("claude_stream_idle_timeout_ms"),
            );
        }
    }

    let env_is_empty = settings
        .get("env")
        .and_then(Value::as_object)
        .is_some_and(Map::is_empty);
    let remove_env = env_is_empty
        && backup.is_some_and(|backup| {
            backup.get("env_was_present").and_then(Value::as_bool) == Some(false)
                || backup.get("env_was_present").is_none()
        });
    if remove_env {
        settings_object_mut(settings)?.remove("env");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn apply_preserves_absent_api_key_and_omits_it_from_new_backup() {
        let mut settings = json!({"permissions": {"allow": ["Read"]}});

        let backup = apply_managed_env(&mut settings, "http://127.0.0.1:8080", 300_000).unwrap();
        let serialized_backup = serde_json::to_string(&backup).unwrap();

        assert!(settings["env"].get("ANTHROPIC_API_KEY").is_none());
        assert!(backup.get("anthropic_api_key").is_none());
        assert!(!serialized_backup.contains("anthropic_api_key"));
    }

    #[test]
    fn applies_base_url_tool_search_and_timeout_with_exact_backup() {
        let mut settings = json!({
            "env": {
                "ANTHROPIC_API_KEY": "existing-key",
                "ENABLE_TOOL_SEARCH": "auto:5",
                "UNRELATED": "keep"
            },
            "permissions": {"allow": ["Read"]}
        });

        let backup = apply_managed_env(&mut settings, "http://127.0.0.1:8080", 300_000).unwrap();

        assert_eq!(
            settings["env"]["ANTHROPIC_BASE_URL"],
            "http://127.0.0.1:8080"
        );
        assert_eq!(settings["env"]["ANTHROPIC_API_KEY"], "existing-key");
        assert_eq!(settings["env"]["ENABLE_TOOL_SEARCH"], "true");
        assert_eq!(settings["env"]["CLAUDE_STREAM_IDLE_TIMEOUT_MS"], "300000");
        assert_eq!(settings["env"]["UNRELATED"], "keep");
        assert_eq!(settings["permissions"], json!({"allow": ["Read"]}));
        assert!(backup.get("anthropic_api_key").is_none());
        assert_eq!(backup["enable_tool_search"]["old_value"], "auto:5");
    }

    #[test]
    fn restore_reinstates_existing_values_and_removes_new_values() {
        let original = json!({
            "env": {
                "ANTHROPIC_API_KEY": 7,
                "ENABLE_TOOL_SEARCH": {"mode": "auto"},
                "UNRELATED": "keep"
            },
            "permissions": {"allow": ["Read"]}
        });
        let mut settings = original.clone();
        let backup = apply_managed_env(&mut settings, "http://127.0.0.1:8080", 300_000).unwrap();

        restore_managed_env(&mut settings, Some(&backup)).unwrap();

        assert_eq!(settings, original);
    }

    #[test]
    fn existing_api_key_is_never_changed_or_serialized_into_backup() {
        const SENTINEL_SECRET: &str = "sentinel-anthropic-secret-do-not-persist";
        let original = json!({
            "env": {
                "ANTHROPIC_API_KEY": SENTINEL_SECRET,
                "UNRELATED": "keep"
            }
        });
        let mut settings = original.clone();

        let backup = apply_managed_env(&mut settings, "http://127.0.0.1:8080", 300_000).unwrap();
        let serialized_backup = serde_json::to_string(&backup).unwrap();

        assert_eq!(settings["env"]["ANTHROPIC_API_KEY"], SENTINEL_SECRET);
        assert!(backup.get("anthropic_api_key").is_none());
        assert!(!serialized_backup.contains(SENTINEL_SECRET));
        restore_managed_env(&mut settings, Some(&backup)).unwrap();
        assert_eq!(settings, original);
    }

    #[test]
    fn apply_and_restore_round_trip_preserves_absent_and_existing_api_key_states() {
        let originals = [
            json!({"permissions": {"allow": ["Read"]}}),
            json!({
                "env": {
                    "ANTHROPIC_API_KEY": "  user-supplied-key  ",
                    "UNRELATED": "keep"
                }
            }),
        ];

        for original in originals {
            let mut settings = original.clone();
            let backup =
                apply_managed_env(&mut settings, "http://127.0.0.1:8080", 300_000).unwrap();

            assert_eq!(
                settings["env"].get("ANTHROPIC_API_KEY"),
                original
                    .get("env")
                    .and_then(|env| env.get("ANTHROPIC_API_KEY"))
            );
            assert!(backup.get("anthropic_api_key").is_none());

            restore_managed_env(&mut settings, Some(&backup)).unwrap();
            assert_eq!(settings, original);
        }
    }

    #[test]
    fn zero_timeout_leaves_existing_timeout_unmanaged() {
        let mut settings = json!({
            "env": {"CLAUDE_STREAM_IDLE_TIMEOUT_MS": {"custom": true}}
        });

        let backup = apply_managed_env(&mut settings, "http://127.0.0.1:8080", 0).unwrap();

        assert_eq!(
            settings["env"]["CLAUDE_STREAM_IDLE_TIMEOUT_MS"],
            json!({"custom": true})
        );
        assert!(backup.get("claude_stream_idle_timeout_ms").is_none());
    }

    #[test]
    fn restores_legacy_base_url_backup() {
        let mut settings = json!({"env": {"ANTHROPIC_BASE_URL": "http://adapter"}});
        let legacy = json!({"had_value": true, "old_value": "https://old.example"});

        restore_managed_env(&mut settings, Some(&legacy)).unwrap();

        assert_eq!(settings["env"]["ANTHROPIC_BASE_URL"], "https://old.example");
    }

    #[test]
    fn absent_env_is_removed_after_round_trip() {
        let original = json!({"permissions": {"allow": ["Read"]}});
        let mut settings = original.clone();
        let backup = apply_managed_env(&mut settings, "http://127.0.0.1:8080", 300_000).unwrap();

        restore_managed_env(&mut settings, Some(&backup)).unwrap();

        assert_eq!(settings, original);
    }

    #[test]
    fn restore_preserves_unrelated_env_values_added_after_apply() {
        let mut settings = json!({"permissions": {"allow": ["Read"]}});
        let backup = apply_managed_env(&mut settings, "http://127.0.0.1:8080", 300_000).unwrap();
        settings["env"]["UNRELATED"] = json!({"added": "later"});

        restore_managed_env(&mut settings, Some(&backup)).unwrap();

        assert_eq!(
            settings,
            json!({
                "env": {"UNRELATED": {"added": "later"}},
                "permissions": {"allow": ["Read"]}
            })
        );
    }

    #[test]
    fn empty_env_object_is_preserved_after_round_trip() {
        let original = json!({"env": {}, "permissions": {"allow": ["Read"]}});
        let mut settings = original.clone();
        let backup = apply_managed_env(&mut settings, "http://127.0.0.1:8080", 300_000).unwrap();

        restore_managed_env(&mut settings, Some(&backup)).unwrap();

        assert_eq!(settings, original);
    }

    #[test]
    fn no_backup_preserves_manually_supplied_local_api_key() {
        let mut adapter_values = json!({
            "env": {
                "ANTHROPIC_BASE_URL": "https://user.example",
                "ANTHROPIC_API_KEY": "cc-adapter-local",
                "ENABLE_TOOL_SEARCH": "true",
                "CLAUDE_STREAM_IDLE_TIMEOUT_MS": "300000",
                "UNRELATED": "keep"
            }
        });

        restore_managed_env(&mut adapter_values, None).unwrap();

        assert_eq!(
            adapter_values,
            json!({
                "env": {
                    "ANTHROPIC_BASE_URL": "https://user.example",
                    "ANTHROPIC_API_KEY": "cc-adapter-local",
                    "CLAUDE_STREAM_IDLE_TIMEOUT_MS": "300000",
                    "UNRELATED": "keep"
                }
            })
        );

        let original_user_values = json!({
            "env": {
                "ANTHROPIC_API_KEY": "user-key",
                "ENABLE_TOOL_SEARCH": "auto:5"
            }
        });
        let mut user_values = original_user_values.clone();

        restore_managed_env(&mut user_values, None).unwrap();

        assert_eq!(user_values, original_user_values);
    }

    #[test]
    fn legacy_injected_api_key_backup_removes_only_unchanged_adapter_dummy() {
        let legacy = json!({
            "env_was_present": true,
            "anthropic_api_key": {"injected": true}
        });
        let mut unchanged_dummy = json!({
            "env": {"ANTHROPIC_API_KEY": "cc-adapter-local", "UNRELATED": "keep"}
        });
        let mut replaced_dummy = json!({
            "env": {"ANTHROPIC_API_KEY": "manually-replaced", "UNRELATED": "keep"}
        });

        restore_managed_env(&mut unchanged_dummy, Some(&legacy)).unwrap();
        restore_managed_env(&mut replaced_dummy, Some(&legacy)).unwrap();

        assert_eq!(unchanged_dummy, json!({"env": {"UNRELATED": "keep"}}));
        assert_eq!(
            replaced_dummy,
            json!({
                "env": {"ANTHROPIC_API_KEY": "manually-replaced", "UNRELATED": "keep"}
            })
        );
    }

    #[test]
    fn legacy_saved_api_key_backup_restores_exact_value() {
        const LEGACY_VALUE: &str = " legacy-secret-value ";
        let legacy = json!({
            "env_was_present": true,
            "anthropic_api_key": {"had_value": true, "old_value": LEGACY_VALUE}
        });
        let mut settings = json!({
            "env": {"ANTHROPIC_API_KEY": "cc-adapter-local", "UNRELATED": "keep"}
        });

        restore_managed_env(&mut settings, Some(&legacy)).unwrap();

        assert_eq!(settings["env"]["ANTHROPIC_API_KEY"], LEGACY_VALUE);
    }

    #[test]
    fn runtime_and_documentation_preserve_claude_login_guidance() {
        const MAIN_SOURCE: &str = include_str!("main.rs");
        const README: &str = include_str!("../README.md");
        const AGENT_INSTALL: &str = include_str!("../docs/agent-install.md");
        const CONFIG_EXAMPLE: &str = include_str!("../config-example.toml");
        const AUTOMATIC_NON_OWNERSHIP: &str = "Automatic settings behavior does not own `ANTHROPIC_API_KEY`: with the current backup shape it does not add, overwrite, remove, back up, or restore the key, and new backups do not contain an `anthropic_api_key` section.";
        const FALLBACK_WARNING: &str = "Optional fallback for a Claude Code client that is not signed in: `ANTHROPIC_API_KEY=cc-adapter-local`. Setting any API key takes precedence over your Claude.ai login and disables Claude.ai-hosted connectors; local/configured MCP servers still work.";
        const QUOTA_GUIDANCE: &str = "Signed-in Claude Code can be out of Claude model credits because routed inference uses ChatGPT/Codex quota.";

        assert!(!MAIN_SOURCE.contains(
            "Auto-configured ~/.claude/settings.json: ANTHROPIC_BASE_URL, ANTHROPIC_API_KEY"
        ));
        assert!(!MAIN_SOURCE.contains("println!(\"    ANTHROPIC_API_KEY="));

        for document in [README, AGENT_INSTALL, CONFIG_EXAMPLE] {
            assert!(document.contains(AUTOMATIC_NON_OWNERSHIP));
            assert!(document.contains(FALLBACK_WARNING));
            assert!(document.contains(QUOTA_GUIDANCE));
            assert!(!document.contains("$env:ANTHROPIC_API_KEY"));
            assert!(!document.contains("export ANTHROPIC_API_KEY"));
        }
    }

    #[test]
    fn apply_rejects_non_object_env_without_mutating_settings() {
        let original = json!({"env": ["user", "data"], "permissions": {"allow": []}});
        let mut settings = original.clone();

        let error = apply_managed_env(&mut settings, "http://127.0.0.1:8080", 300_000).unwrap_err();

        assert_eq!(error, "Claude settings `env` must be a JSON object");
        assert_eq!(settings, original);
    }

    #[test]
    fn restore_rejects_non_object_env_without_mutating_settings() {
        let original = json!({"env": "user-data", "permissions": {"allow": []}});
        let mut settings = original.clone();
        let backup = json!({
            "anthropic_base_url": {"had_value": false, "old_value": null}
        });

        let error = restore_managed_env(&mut settings, Some(&backup)).unwrap_err();

        assert_eq!(error, "Claude settings `env` must be a JSON object");
        assert_eq!(settings, original);
    }
}
