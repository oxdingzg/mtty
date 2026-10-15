//! Local adapter discovery and copyable ACP configuration templates.
use mtty_config::AcpAgent;
use std::path::Path;

#[derive(Clone, Debug)]
pub struct Adapter {
    pub agent: AcpAgent,
    pub found: bool,
    pub configured: bool,
    pub install: Option<String>,
}

pub fn templates() -> Vec<(AcpAgent, String)> {
    [
        (
            "Claude Agent ACP",
            vec!["claude-agent-acp"],
            "npm install -g @agentclientprotocol/claude-agent-acp",
        ),
        (
            "Codex ACP",
            vec!["codex-acp"],
            "npm install -g @agentclientprotocol/codex-acp",
        ),
        (
            "Gemini ACP",
            vec!["gemini", "--experimental-acp"],
            "npm install -g @google/gemini-cli",
        ),
    ]
    .into_iter()
    .map(|(name, command, install)| {
        (
            AcpAgent {
                name: name.into(),
                command: command.into_iter().map(str::to_owned).collect(),
                ..Default::default()
            },
            install.into(),
        )
    })
    .collect()
}

fn found(agent: &AcpAgent) -> bool {
    let Some(program) = agent.command.first() else {
        return false;
    };
    let path = Path::new(program);
    if path.is_absolute() || program.contains(['/', '\\']) {
        return path.is_file();
    }
    let env = |name: &str| {
        agent
            .env
            .iter()
            .find(|(key, _)| {
                if cfg!(windows) {
                    key.eq_ignore_ascii_case(name)
                } else {
                    key.as_str() == name
                }
            })
            .map(|(_, value)| value.clone())
            .or_else(|| std::env::var(name).ok())
    };
    let extensions: Vec<String> = if cfg!(windows) {
        env("PATHEXT")
            .unwrap_or_else(|| ".EXE;.CMD;.BAT;.COM".into())
            .split(';')
            .map(str::to_owned)
            .collect()
    } else {
        Vec::new()
    };
    env("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| {
            dir.join(program).is_file()
                || extensions
                    .iter()
                    .any(|extension| dir.join(format!("{program}{extension}")).is_file())
        })
    })
}

pub fn discover(configured: &[AcpAgent]) -> Vec<Adapter> {
    let templates = templates();
    let mut result: Vec<_> = configured
        .iter()
        .cloned()
        .map(|agent| {
            let install = templates
                .iter()
                .find(|(template, _)| template.command == agent.command)
                .map(|(_, install)| install.clone());
            Adapter {
                found: found(&agent),
                agent,
                configured: true,
                install,
            }
        })
        .collect();
    let mut additional: Vec<_> = templates
        .into_iter()
        .filter(|(agent, _)| {
            !configured
                .iter()
                .any(|configured| configured.command == agent.command)
        })
        .map(|(agent, install)| Adapter {
            found: found(&agent),
            agent,
            configured: false,
            install: Some(install),
        })
        .collect();
    additional.sort_by_key(|entry| !entry.found);
    result.extend(additional);
    result
}

pub fn parse_command(text: &str) -> Result<Vec<String>, String> {
    let command: Vec<String> = if text.trim_start().starts_with('[') {
        serde_json::from_str(text).map_err(|e| format!("command array: {e}"))?
    } else {
        if text.contains(['\'', '"']) {
            return Err("use a JSON argv array for quoted command arguments".into());
        }
        text.split_whitespace().map(str::to_owned).collect()
    };
    if command.first().map_or(true, |command| command.is_empty())
        || command.iter().any(|argument| argument.contains('\0'))
    {
        return Err("enter a command or JSON argv array".into());
    }
    Ok(command)
}

pub fn config_template(agent: &AcpAgent) -> String {
    format!(
        "\n[[acp.agent]]\nname = {}\ncommand = {}\n",
        serde_json::to_string(&agent.name).unwrap(),
        serde_json::to_string(&agent.command).unwrap()
    )
}

pub fn append_bounded(target: &mut String, text: &str, limit: usize) {
    target.push_str(text);
    if target.len() > limit {
        let mut cut = target.len() - limit;
        while !target.is_char_boundary(cut) {
            cut += 1;
        }
        target.drain(..cut);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn quoted_argv_templates_round_trip_through_config() {
        let command =
            parse_command(r#"["/path with space/adapter","--flag","two words"]"#).unwrap();
        assert_eq!(command[0], "/path with space/adapter");
        let agent = AcpAgent {
            name: "Custom ACP".into(),
            command,
            ..Default::default()
        };
        let config = mtty_config::Config::from_toml(&config_template(&agent)).unwrap();
        assert_eq!(config.acp.agents[0].command, agent.command);
        assert!(parse_command("\"/bad quoted path\"").is_err());
        assert!(parse_command("[]").is_err());
    }
    #[test]
    fn configured_entries_are_preserved_without_duplicate_templates_or_credentials() {
        let mut agent = templates().remove(0).0;
        agent
            .env
            .insert("PRIVATE_VALUE".into(), "fixture-only".into());
        assert!(!config_template(&agent).contains("fixture-only"));
        let result = discover(std::slice::from_ref(&agent));
        assert!(result[0].configured);
        assert_eq!(
            result
                .iter()
                .filter(|entry| entry.agent.command == agent.command)
                .count(),
            1
        );
        let mut text = "first".to_owned();
        append_bounded(&mut text, "你好世界", 7);
        assert!(text.len() <= 7 && text.ends_with("世界"));
    }
}
