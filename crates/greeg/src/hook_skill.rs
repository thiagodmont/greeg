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

/// blake3 of each skill text a release wrote before skills carried a marker.
const UNMARKED_RELEASES: &[(Agent, &str)] = &[
    // 0.2.0 to 0.3.0
    (
        Agent::Claude,
        "c27b679a81e4ddaa0ae5083534edee66affe3a3e8761929e877065214072e5dc",
    ),
    // 0.4.0 to 0.6.0
    (
        Agent::Claude,
        "aaa0441fd5bb6f49aa216694d0c8cbe93ad800cc2dc00ad1883b621ee4a29968",
    ),
    // 0.5.0 to 0.6.0
    (
        Agent::Codex,
        "578731ba0b168fcd7a9a3d6d40877cbe55f9d4f3d02eddf0643bb419d1283752",
    ),
];

fn unmarked_release(text: &str, agent: Agent) -> bool {
    let digest = blake3::hash(text.as_bytes()).to_hex();
    UNMARKED_RELEASES
        .iter()
        .any(|&(a, d)| a == agent && d == digest.as_str())
}

fn owned(text: &str, legacy: &str, agent: Agent) -> bool {
    text == legacy
        || unmarked_release(text, agent)
        || text
            .rsplit_once(MARKER)
            .is_some_and(|(body, _)| text == managed_text(body, agent))
}

/// An installed skill file against the text this build generates.
#[derive(Debug, PartialEq, Eq)]
pub enum State {
    Missing,
    Current,
    Outdated,
    Edited,
}

pub fn state(text: Option<&str>, body: &str, agent: Agent) -> State {
    match text {
        None => State::Missing,
        Some(text) if !owned(text, body, agent) => State::Edited,
        Some(text) if text == managed_text(body, agent) => State::Current,
        Some(_) => State::Outdated,
    }
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
        let action = match (state(snapshot.text(), body, agent), uninstall) {
            (State::Edited, _) => Action::Preserve,
            (State::Missing, true) => Action::Missing,
            (State::Missing | State::Outdated, false) => Action::Write,
            (State::Current, false) => Action::Current,
            (State::Current | State::Outdated, true) => Action::Remove,
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
    fn a_release_text_without_a_marker_is_owned_by_its_agent_only_while_unedited() {
        let released = [
            (
                Agent::Claude,
                include_str!("../tests/fixtures/skills/claude-0.2.0.md"),
            ),
            (
                Agent::Claude,
                include_str!("../tests/fixtures/skills/claude-0.4.0.md"),
            ),
            (
                Agent::Codex,
                include_str!("../tests/fixtures/skills/codex-0.5.0.md"),
            ),
        ];
        assert_eq!(released.len(), UNMARKED_RELEASES.len());
        for ((agent, text), &(listed, digest)) in released.into_iter().zip(UNMARKED_RELEASES) {
            assert_eq!(agent, listed);
            assert_eq!(blake3::hash(text.as_bytes()).to_hex().as_str(), digest);
            let other = if agent == Agent::Claude {
                Agent::Codex
            } else {
                Agent::Claude
            };
            assert!(owned(text, "current body\n", agent));
            assert!(!owned(text, "current body\n", other));
            assert!(!owned(&format!("{text}\n"), "current body\n", agent));
            assert!(!owned(
                &text.replacen("greeg", "Greeg", 1),
                "current body\n",
                agent
            ));
        }
    }

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
