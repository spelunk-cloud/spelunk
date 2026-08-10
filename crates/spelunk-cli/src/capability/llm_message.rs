//! User-facing text for "no LLM is available", shared by every command that
//! needs one so they read as one product rather than three dialects.

/// Why LLM routing found nothing to run against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoLlmReason {
    /// Offline mode is explicitly in force: no probe was made and none will be.
    Offline,
    /// `llm_url` is configured, but the reachable local server does not serve
    /// an LLM. Deliberately terminal: falling through to a remote LLM after the
    /// user asked for a local one would ship their code somewhere they did not
    /// choose.
    LocalConfiguredButNotServed,
    /// Neither the local server nor an explicitly configured `server_url`
    /// offers an LLM.
    NoLlmAnywhere,
}

/// The command asking for an LLM. Selects the subject of the message and
/// whether the opt-out flag is worth mentioning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmFeature {
    Summaries,
    Explore,
    MemoryHarvest,
}

impl LlmFeature {
    fn subject(self) -> &'static str {
        match self {
            LlmFeature::Summaries => "Skipping chunk summaries",
            LlmFeature::Explore => "'spelunk explore' cannot run",
            LlmFeature::MemoryHarvest => "'spelunk memory harvest' cannot run",
        }
    }
}

/// Render the no-LLM notice for `feature`.
///
/// Every branch names the cause and the next step, and none of them names a
/// type, module or internal field: `llm_url` and `server_url` appear only
/// because they are config keys the reader can actually edit.
pub fn no_llm_message(reason: NoLlmReason, feature: LlmFeature) -> String {
    let subject = feature.subject();
    let body = match reason {
        NoLlmReason::Offline => "offline mode is on, so no inference will run.\n\
             Turn offline mode off to enable it: unset SPELUNK_NO_SERVER, or remove \
             `mode = \"offline\"` from your spelunk config."
            .to_string(),
        NoLlmReason::LocalConfiguredButNotServed => {
            "your local spelunk server is running without the LLM endpoint you set in \
             `llm_url`, so it cannot answer LLM requests.\n\
             A running server keeps the settings it started with, so restart it to pick \
             yours up:\n  \
             spelunk server stop\n  \
             spelunk server start"
                .to_string()
        }
        NoLlmReason::NoLlmAnywhere => {
            let mut msg = "no LLM is available.\n\
                 There are two ways to get one:\n  \
                 set `llm_url` in ~/.config/spelunk/config.toml to your own \
                 chat-completions endpoint, then run `spelunk server stop` and \
                 `spelunk server start`;\n  \
                 or set `server_url` to a spelunk server that already provides one."
                .to_string();
            if feature == LlmFeature::Summaries {
                msg.push_str(
                    "\nPass `--no-summaries` to `spelunk index` to skip this step without \
                     the notice.",
                );
            }
            msg
        }
    };
    format!("{subject}: {body}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const REASONS: [NoLlmReason; 3] = [
        NoLlmReason::Offline,
        NoLlmReason::LocalConfiguredButNotServed,
        NoLlmReason::NoLlmAnywhere,
    ];
    const FEATURES: [LlmFeature; 3] = [
        LlmFeature::Summaries,
        LlmFeature::Explore,
        LlmFeature::MemoryHarvest,
    ];

    // The jargon in the message this task replaces is what created the task.
    // No message may name an internal type, adapter or field; a reader can
    // only act on things they can edit.
    #[test]
    fn no_message_leaks_an_internal_type_or_field() {
        for reason in REASONS {
            for feature in FEATURES {
                let msg = no_llm_message(reason, feature);
                for jargon in [
                    "ServerInferenceClient",
                    "ServerLlmClient",
                    "ServerLlmAdapter",
                    "ServerEmbedAdapter",
                    "Capabilities",
                    "inference_url",
                    "llm.complete",
                    "Tier",
                ] {
                    assert!(
                        !msg.contains(jargon),
                        "{reason:?}/{feature:?} message leaks {jargon:?}: {msg}"
                    );
                }
            }
        }
    }

    #[test]
    fn every_message_names_the_command_and_a_next_step() {
        for reason in REASONS {
            for feature in FEATURES {
                let msg = no_llm_message(reason, feature);
                assert!(
                    msg.starts_with(feature.subject()),
                    "{reason:?}/{feature:?} must lead with the command it concerns: {msg}"
                );
                assert!(
                    msg.contains("spelunk ") || msg.contains("SPELUNK_"),
                    "{reason:?}/{feature:?} must give a command or setting to act on: {msg}"
                );
            }
        }
    }

    #[test]
    fn offline_message_names_offline_mode_and_how_to_leave_it() {
        let msg = no_llm_message(NoLlmReason::Offline, LlmFeature::Summaries);
        assert!(msg.contains("offline mode is on"), "{msg}");
        assert!(msg.contains("SPELUNK_NO_SERVER"), "{msg}");
        assert!(msg.contains("mode = \"offline\""), "{msg}");
    }

    // The stale-daemon case: the setting is right, the running process is
    // older than it. The only useful instruction is the restart.
    #[test]
    fn local_configured_but_not_served_message_names_llm_url_and_the_restart() {
        let msg = no_llm_message(
            NoLlmReason::LocalConfiguredButNotServed,
            LlmFeature::Explore,
        );
        assert!(msg.contains("llm_url"), "{msg}");
        assert!(msg.contains("spelunk server stop"), "{msg}");
        assert!(msg.contains("spelunk server start"), "{msg}");
        assert!(
            !msg.contains("server_url"),
            "must not send the user to a remote LLM they deliberately did not choose: {msg}"
        );
    }

    #[test]
    fn no_llm_anywhere_message_offers_both_routes_to_an_llm() {
        let msg = no_llm_message(NoLlmReason::NoLlmAnywhere, LlmFeature::MemoryHarvest);
        assert!(msg.contains("llm_url"), "local route missing: {msg}");
        assert!(msg.contains("server_url"), "remote route missing: {msg}");
    }

    // `--no-summaries` is only an instruction for `index`; offering it to
    // `explore` or `harvest` would be noise pointing at a flag they lack.
    #[test]
    fn no_summaries_flag_is_offered_to_summaries_only() {
        assert!(
            no_llm_message(NoLlmReason::NoLlmAnywhere, LlmFeature::Summaries)
                .contains("--no-summaries")
        );
        for feature in [LlmFeature::Explore, LlmFeature::MemoryHarvest] {
            assert!(
                !no_llm_message(NoLlmReason::NoLlmAnywhere, feature).contains("--no-summaries"),
                "{feature:?} has no such flag"
            );
        }
    }

    #[test]
    fn each_reason_renders_a_distinct_message() {
        let rendered: Vec<String> = REASONS
            .iter()
            .map(|r| no_llm_message(*r, LlmFeature::Summaries))
            .collect();
        for (i, a) in rendered.iter().enumerate() {
            for b in rendered.iter().skip(i + 1) {
                assert_ne!(a, b, "two reasons render identically, so one is unusable");
            }
        }
    }
}
