//! `subswap add-api`：交互式登记 Claude Code 兼容 API，保存后不自动激活。

use std::io::{self, IsTerminal};

use anyhow::{bail, Context, Result};
use dialoguer::{Confirm, Input, Select};
use subswap_core::{AuditEvent, BillingKind};
use subswap_provider_claude::ClaudeApiConfig;

use crate::app::AppContext;

pub struct AddApiOptions {
    pub preset: Option<String>,
    pub id: Option<String>,
    pub name: Option<String>,
    pub endpoint: Option<String>,
    pub api_key: Option<String>,
    pub auth: Option<String>,
    pub model: Option<String>,
    pub opus_model: Option<String>,
    pub sonnet_model: Option<String>,
    pub haiku_model: Option<String>,
    pub subagent_model: Option<String>,
    pub effort: Option<String>,
    /// 计费方式：flat（订阅固定费率）| metered（按量）| unlimited（不限量）。
    /// 决定 OpenConductor 等下游消费者按权重自动切换时的优先级。
    pub billing: Option<String>,
    pub yes: bool,
}

struct Draft {
    id: String,
    name: String,
    endpoint: String,
    api_key: String,
    auth_field: String,
    model: String,
    opus_model: String,
    sonnet_model: String,
    haiku_model: String,
    subagent_model: String,
    effort: String,
    billing: BillingKind,
    skip_confirmation: bool,
}

pub fn run(ctx: &AppContext, options: AddApiOptions) -> Result<()> {
    let interactive = io::stdin().is_terminal() && io::stdout().is_terminal();
    let draft = build_draft(options, interactive)?;

    if !draft.endpoint.starts_with("https://") && !draft.endpoint.starts_with("http://") {
        bail!("endpoint must start with http:// or https://");
    }

    if interactive
        && !draft.skip_confirmation
        && !Confirm::new()
            .with_prompt(format!("Add provider {}?", draft.name))
            .default(true)
            .interact()?
    {
        println!("Cancelled.");
        return Ok(());
    }

    let account = ctx
        .claude
        .add_api(
            draft.id,
            draft.name,
            draft.api_key,
            ClaudeApiConfig {
                base_url: draft.endpoint.trim_end_matches('/').to_string(),
                auth_field: draft.auth_field,
                model: draft.model,
                opus_model: draft.opus_model,
                sonnet_model: draft.sonnet_model,
                haiku_model: draft.haiku_model,
                subagent_model: draft.subagent_model,
                effort_level: draft.effort,
            },
            draft.billing,
        )
        .context("add Claude API provider")?;
    ctx.audit.append(AuditEvent::ok(
        "add_api",
        "claude",
        Some(account.id.0.as_str()),
    ));
    println!("added → claude/{}", account.id);
    println!("Run `subswap swap {}` to activate it.", account.id);
    Ok(())
}

fn build_draft(options: AddApiOptions, interactive: bool) -> Result<Draft> {
    let preset = match options.preset {
        Some(value) => normalize_preset(&value)?,
        None if interactive => {
            let choices = ["DeepSeek", "Custom"];
            let selected = Select::new()
                .with_prompt("Provider preset")
                .items(&choices)
                .default(0)
                .interact()?;
            choices[selected].to_ascii_lowercase()
        }
        None => bail!("--preset is required without an interactive terminal"),
    };

    let defaults = preset_defaults(&preset);
    let name = value_or_prompt(options.name, interactive, "Name", defaults.name)?;
    let id_default = if preset == "deepseek" {
        "deepseek".to_string()
    } else {
        slugify(&name)
    };
    let id = value_or_prompt(
        options.id,
        interactive && preset == "custom",
        "Id",
        &id_default,
    )?;
    let endpoint = value_or_prompt(
        options.endpoint,
        interactive && preset == "custom",
        "Endpoint",
        if preset == "custom" && !interactive {
            ""
        } else {
            defaults.endpoint
        },
    )?;
    let api_key = match options.api_key {
        Some(value) if !value.trim().is_empty() => value,
        Some(_) => bail!("API key cannot be empty"),
        None if interactive => Input::new()
            .with_prompt("API key")
            .validate_with(|value: &String| -> Result<(), &str> {
                if value.trim().is_empty() {
                    Err("API key cannot be empty")
                } else {
                    Ok(())
                }
            })
            .interact_text()?,
        None => bail!("--api-key is required without an interactive terminal"),
    };
    let auth = match options.auth {
        Some(value) => normalize_auth(&value)?,
        None if preset == "deepseek" => "bearer".into(),
        None if interactive => {
            let choices = ["Authorization Bearer", "X-Api-Key"];
            match Select::new()
                .with_prompt("Authentication")
                .items(&choices)
                .default(0)
                .interact()?
            {
                0 => "bearer".into(),
                _ => "api-key".into(),
            }
        }
        None => bail!("--auth is required for custom preset without an interactive terminal"),
    };
    let model = value_or_prompt(
        options.model,
        interactive && preset == "custom",
        "Primary model",
        if preset == "custom" && !interactive {
            ""
        } else {
            defaults.model
        },
    )?;
    let opus_model = value_or_prompt(
        options.opus_model,
        interactive && preset == "custom",
        "Opus model",
        if defaults.opus_model.is_empty() {
            &model
        } else {
            defaults.opus_model
        },
    )?;
    let sonnet_model = value_or_prompt(
        options.sonnet_model,
        interactive && preset == "custom",
        "Sonnet model",
        if defaults.sonnet_model.is_empty() {
            &model
        } else {
            defaults.sonnet_model
        },
    )?;
    let haiku_model = value_or_prompt(
        options.haiku_model,
        interactive && preset == "custom",
        "Haiku model",
        if defaults.haiku_model.is_empty() {
            &model
        } else {
            defaults.haiku_model
        },
    )?;
    let subagent_model = value_or_prompt(
        options.subagent_model,
        interactive && preset == "custom",
        "Subagent model",
        if defaults.subagent_model.is_empty() {
            &haiku_model
        } else {
            defaults.subagent_model
        },
    )?;
    let effort = value_or_prompt(
        options.effort,
        interactive && preset == "custom",
        "Effort",
        "max",
    )?;
    let billing = match options.billing {
        Some(value) => normalize_billing(&value)?,
        None if interactive => {
            let choices = ["Metered (按量计费)", "Unlimited (不限量)", "Flat (固定费率)"];
            match Select::new()
                .with_prompt("Billing")
                .items(&choices)
                .default(0)
                .interact()?
            {
                0 => BillingKind::Metered,
                1 => BillingKind::Unlimited,
                _ => BillingKind::Flat,
            }
        }
        // 非交互且未显式指定：自定义 API 端点默认按量计费，这是最常见也最保守的假设
        // （宁可被当作"会花钱"而提前预警，也不要默认不限量而被无脑自动切过去）。
        None => BillingKind::Metered,
    };

    Ok(Draft {
        id,
        name,
        endpoint,
        api_key,
        auth_field: if auth == "api-key" {
            "ANTHROPIC_API_KEY".into()
        } else {
            "ANTHROPIC_AUTH_TOKEN".into()
        },
        model,
        opus_model,
        sonnet_model,
        haiku_model,
        subagent_model,
        effort,
        billing,
        skip_confirmation: options.yes,
    })
}

struct PresetDefaults {
    name: &'static str,
    endpoint: &'static str,
    model: &'static str,
    opus_model: &'static str,
    sonnet_model: &'static str,
    haiku_model: &'static str,
    subagent_model: &'static str,
}

fn preset_defaults(preset: &str) -> PresetDefaults {
    match preset {
        "deepseek" => PresetDefaults {
            name: "DeepSeek",
            endpoint: "https://api.deepseek.com/anthropic",
            model: "deepseek-v4-pro[1m]",
            opus_model: "deepseek-v4-pro[1m]",
            sonnet_model: "deepseek-v4-pro[1m]",
            haiku_model: "deepseek-v4-flash",
            subagent_model: "deepseek-v4-flash",
        },
        _ => PresetDefaults {
            name: "Custom API",
            endpoint: "https://api.example.com",
            model: "model-id",
            opus_model: "",
            sonnet_model: "",
            haiku_model: "",
            subagent_model: "",
        },
    }
}

fn value_or_prompt(
    value: Option<String>,
    interactive: bool,
    prompt: &str,
    default: &str,
) -> Result<String> {
    if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
        return Ok(value);
    }
    if interactive {
        return Input::new()
            .with_prompt(prompt)
            .default(default.to_string())
            .interact_text()
            .map_err(Into::into);
    }
    if default.is_empty() {
        bail!("{prompt} is required without an interactive terminal");
    }
    Ok(default.to_string())
}

fn normalize_preset(value: &str) -> Result<String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "deepseek" => Ok("deepseek".into()),
        "custom" => Ok("custom".into()),
        other => bail!("unknown preset: {other} (expected deepseek or custom)"),
    }
}

fn normalize_auth(value: &str) -> Result<String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "bearer" | "auth-token" | "anthropic_auth_token" => Ok("bearer".into()),
        "api-key" | "api_key" | "anthropic_api_key" => Ok("api-key".into()),
        other => bail!("unknown auth mode: {other} (expected bearer or api-key)"),
    }
}

fn normalize_billing(value: &str) -> Result<BillingKind> {
    value
        .trim()
        .to_ascii_lowercase()
        .parse::<BillingKind>()
        .map_err(|_| anyhow::anyhow!("unknown billing mode: {} (expected flat, metered or unlimited)", value))
}

fn slugify(value: &str) -> String {
    let slug = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if slug.is_empty() {
        "custom-api".into()
    } else {
        slug
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_creates_stable_account_id() {
        assert_eq!(slugify("My API Endpoint"), "my-api-endpoint");
        assert_eq!(slugify("  "), "custom-api");
    }
}
