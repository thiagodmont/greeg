//! Ownership checks for generated skills; publication uses the config snapshot protocol.
use crate::hook::Agent;
use crate::hook_config::Config;
use anyhow::{Context, Result};
use std::path::Path;

const MARKER: &str = "\n<!-- greeg-managed-skill:v1 ";

fn managed_text(body: &str, agent: Agent) -> String {
    let name = match agent {
        Agent::Claude => "claude",
        Agent::Codex => "codex",
    };
    let digest = blake3::hash(body.as_bytes());
    format!(
        "{body}{MARKER}agent={name} blake3={} -->\n",
        digest.to_hex()
    )
}

fn owned(text: &str, legacy: &str, agent: Agent) -> bool {
    text == legacy
        || text
            .rsplit_once(MARKER)
            .is_some_and(|(body, _)| text == managed_text(body, agent))
}

enum Action {
    Write,
    Remove,
    Current,
    Missing,
    Preserve,
}

pub struct Skill {
    snapshot: Config,
    generated: String,
    action: Action,
}

impl Skill {
    pub fn read(path: &Path, body: &str, agent: Agent, uninstall: bool) -> Result<Self> {
        let snapshot =
            Config::read(path).context("inspect generated skill before editing hooks")?;
        let generated = managed_text(body, agent);
        let action = match snapshot.text() {
            None if uninstall => Action::Missing,
            None => Action::Write,
            Some(text) if !owned(text, body, agent) => Action::Preserve,
            Some(_) if uninstall => Action::Remove,
            Some(text) if text == generated => Action::Current,
            Some(_) => Action::Write,
        };
        Ok(Self {
            snapshot,
            generated,
            action,
        })
    }

    pub fn description(&self, dry_run: bool) -> &'static str {
        match (&self.action, dry_run) {
            (Action::Write, true) => "would write the managed greeg skill",
            (Action::Write, false) => "managed skill written",
            (Action::Remove, true) => "would remove the unmodified greeg skill",
            (Action::Remove, false) => "unmodified skill removed",
            (Action::Current, _) => "skill already current",
            (Action::Missing, _) => "no greeg skill present",
            (Action::Preserve, _) => {
                "skill preserved (modified or unrecognized; move it aside to regenerate)"
            }
        }
    }

    pub fn apply(&self) -> Result<()> {
        match self.action {
            Action::Write => self.snapshot.write(&self.generated),
            Action::Remove => self.snapshot.remove(),
            Action::Current | Action::Missing => self.snapshot.check_current(),
            Action::Preserve => Ok(()),
        }
        .context("hook configuration was applied, but the skill operation failed; inspect both paths before retrying")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ownership_requires_an_exact_legacy_body_or_intact_agent_marker() {
        let legacy = "legacy instructions\n";
        for agent in [Agent::Claude, Agent::Codex] {
            assert!(owned(legacy, legacy, agent));
            let generated = managed_text("previous generated instructions\n", agent);
            assert!(owned(&generated, legacy, agent));
            let other = if agent == Agent::Claude {
                Agent::Codex
            } else {
                Agent::Claude
            };
            for changed in [
                generated.replace("previous", "edited"),
                generated.replace(":v1 ", ":v2 "),
                generated.replace("blake3=", "sha256="),
                generated.replace(" -->\n", " -->"),
                format!("{generated}user addition\n"),
                managed_text("previous generated instructions\n", other),
                "custom instructions\n".to_owned(),
            ] {
                assert!(!owned(&changed, legacy, agent), "{changed}");
            }
        }
    }
}
