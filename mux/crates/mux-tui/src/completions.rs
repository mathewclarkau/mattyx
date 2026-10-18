//! Shell completion scripts for `mtyx`.
//!
//! The verb table (`crate::cli::VERBS`) is the single source of truth:
//! the emitters below walk it at run time, so adding a verb or a flag to
//! `cli.rs` updates every shell's completions with no extra bookkeeping.
//!
//! `mtyx completions <bash|zsh|fish>` is a `main.rs` special-case (like
//! `theme`) rather than a verb-table entry, so the verb count stays at 64
//! and the subcommand never reaches the control socket.

use crate::cli::VERBS;

/// Flags accepted by every verb, before the verb's own `allowed` list.
const GLOBAL_FLAGS: &str = "--json --socket --session --machine --help";

/// Emit the completion script for `shell`. Unknown shells return an empty
/// string; `main.rs` rejects them with exit 2 before calling in.
pub(crate) fn generate(shell: &str) -> String {
    match shell {
        "bash" => bash(),
        "zsh" => zsh(),
        "fish" => fish(),
        _ => String::new(),
    }
}

fn verb_names() -> Vec<&'static str> {
    VERBS.iter().map(|verb| verb.name).collect()
}

/// `--flag1 --flag2` for a verb, globals included.
fn verb_flags(verb: &crate::cli::VerbSpec) -> String {
    let mut flags = String::from(GLOBAL_FLAGS);
    for flag in verb.allowed {
        flags.push_str(" --");
        flags.push_str(flag);
    }
    flags
}

fn bash() -> String {
    let mut out = String::new();
    out.push_str("# bash completion for mtyx — install with:\n");
    out.push_str("#   mtyx completions bash > /etc/bash_completion.d/mtyx\n");
    out.push_str("_mtyx() {\n");
    out.push_str("    local cur verb\n");
    out.push_str("    COMPREPLY=()\n");
    out.push_str("    cur=\"${COMP_WORDS[COMP_CWORD]}\"\n");
    out.push_str("    if [ \"$COMP_CWORD\" -eq 1 ]; then\n");
    out.push_str(&format!(
        "        COMPREPLY=( $(compgen -W \"{}\" -- \"$cur\") )\n",
        verb_names().join(" ")
    ));
    out.push_str("        return 0\n");
    out.push_str("    fi\n");
    out.push_str("    verb=\"${COMP_WORDS[1]}\"\n");
    out.push_str("    case \"$verb\" in\n");
    for verb in VERBS {
        out.push_str(&format!("        {})\n", verb.name));
        out.push_str(&format!(
            "            COMPREPLY=( $(compgen -W \"{}\" -- \"$cur\") )\n",
            verb_flags(verb)
        ));
        out.push_str("            ;;\n");
    }
    out.push_str("        *)\n");
    out.push_str(&format!(
        "            COMPREPLY=( $(compgen -W \"{GLOBAL_FLAGS}\" -- \"$cur\") )\n"
    ));
    out.push_str("            ;;\n");
    out.push_str("    esac\n");
    out.push_str("    return 0\n");
    out.push_str("}\n");
    out.push_str("complete -F _mtyx mtyx\n");
    out
}

fn zsh() -> String {
    let mut out = String::new();
    out.push_str("#compdef mtyx\n");
    out.push_str("# zsh completion for mtyx — install with:\n");
    out.push_str("#   mtyx completions zsh > \"${fpath[1]}/_mtyx\"\n");
    out.push_str("\n");
    out.push_str("local -a _mtyx_verbs\n");
    out.push_str(&format!("_mtyx_verbs=({})\n", verb_names().join(" ")));
    out.push_str("if (( CURRENT == 2 )); then\n");
    out.push_str("    _describe -t verbs 'mtyx verb' _mtyx_verbs\n");
    out.push_str("    return\n");
    out.push_str("fi\n");
    out.push_str("\n");
    out.push_str("local -a _mtyx_flags\n");
    out.push_str("case \"${words[2]}\" in\n");
    for verb in VERBS {
        out.push_str(&format!("    {})\n", verb.name));
        out.push_str(&format!("        _mtyx_flags=({})\n", verb_flags(verb)));
        out.push_str("        ;;\n");
    }
    out.push_str("    *)\n");
    out.push_str(&format!("        _mtyx_flags=({GLOBAL_FLAGS})\n"));
    out.push_str("        ;;\n");
    out.push_str("esac\n");
    out.push_str("_describe -t flags 'mtyx flag' _mtyx_flags\n");
    out
}

fn fish() -> String {
    let mut out = String::new();
    out.push_str("# fish completion for mtyx — install with:\n");
    out.push_str("#   mtyx completions fish > ~/.config/fish/completions/mtyx.fish\n");
    out.push_str("complete -c mtyx -f\n");
    out.push_str(&format!(
        "complete -c mtyx -n '__fish_use_subcommand' -a '{}'\n",
        verb_names().join(" ")
    ));
    for verb in VERBS {
        for flag in verb.allowed {
            out.push_str(&format!(
                "complete -c mtyx -n '__fish_seen_subcommand_from {}' -l {}\n",
                verb.name, flag
            ));
        }
    }
    for flag in ["json", "socket", "session", "machine", "help"] {
        out.push_str(&format!("complete -c mtyx -l {flag}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_covers_every_verb(script: &str, shell: &str) {
        assert!(!script.is_empty(), "{shell} script must not be empty");
        for verb in VERBS {
            assert!(script.contains(verb.name), "{shell} script is missing verb {:?}", verb.name);
        }
        assert_eq!(VERBS.len(), 64, "verb table drifted from the pinned count");
    }

    #[test]
    fn bash_covers_every_verb() {
        assert_covers_every_verb(&generate("bash"), "bash");
    }

    #[test]
    fn zsh_covers_every_verb() {
        assert_covers_every_verb(&generate("zsh"), "zsh");
    }

    #[test]
    fn fish_covers_every_verb() {
        assert_covers_every_verb(&generate("fish"), "fish");
    }

    #[test]
    fn unknown_shell_is_rejected_by_the_emitter() {
        // `main.rs` exits 2 before calling `generate` for anything other
        // than bash/zsh/fish; the emitter must still not invent a script.
        assert!(generate("nushell").is_empty());
    }

    #[test]
    fn verb_flags_include_allowed_and_globals() {
        let verb = VERBS.iter().find(|v| v.name == "send").expect("send verb");
        let flags = verb_flags(verb);
        assert!(flags.contains("--text"), "missing verb flag: {flags}");
        assert!(flags.contains("--json"), "missing global flag: {flags}");
        assert!(flags.contains("--machine"), "missing global flag: {flags}");
    }
}
