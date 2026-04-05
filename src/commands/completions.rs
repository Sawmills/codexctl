use anyhow::Result;
use clap::CommandFactory;
use clap_complete::{Shell, generate};

use crate::Cli;

pub fn run(shell: Shell) -> Result<()> {
    let mut cmd = Cli::command();
    let name = cmd.get_name().to_string();

    if shell == Shell::Zsh {
        let mut buf = Vec::new();
        generate(shell, &mut cmd, &name, &mut buf);
        let mut script = String::from_utf8(buf)?;

        // Remove #compdef — not needed when sourcing directly
        script = script.replace("#compdef codexctl\n", "");

        // Define a profile completer function
        let profile_fn = r#"
_codexctl_profiles() {
    local profiles_dir="$HOME/.codexctl/profiles"
    if [[ -d "$profiles_dir" ]]; then
        local -a profiles
        profiles=("${(@f)$(ls "$profiles_dir" 2>/dev/null)}")
        compadd -a profiles
    fi
}
"#;

        // Replace _default with _codexctl_profiles for use and remove alias arguments
        script = script.replace(
            "':alias -- Profile alias to switch to:_default'",
            "':alias -- Profile alias to switch to:_codexctl_profiles'",
        );
        script = script.replace(
            "':alias -- Profile alias to remove:_default'",
            "':alias -- Profile alias to remove:_codexctl_profiles'",
        );

        // Insert profile function before the main _codexctl function
        print!("{profile_fn}{script}");
        println!("compdef _codexctl codexctl");
    } else if shell == Shell::Bash {
        generate(shell, &mut cmd, name, &mut std::io::stdout());
        println!();
        println!(r#"_codexctl_profiles() {{"#);
        println!(r#"  local profiles_dir="$HOME/.codexctl/profiles""#);
        println!(r#"  if [[ -d "$profiles_dir" ]]; then"#);
        println!(
            r#"    COMPREPLY=($(compgen -W "$(ls "$profiles_dir")" -- "${{COMP_WORDS[COMP_CWORD]}}"))"#
        );
        println!(r#"  fi"#);
        println!(r#"}}"#);
        println!(r#"complete -F _codexctl_profiles codexctl use"#);
        println!(r#"complete -F _codexctl_profiles codexctl remove"#);
    } else if shell == Shell::Fish {
        generate(shell, &mut cmd, name, &mut std::io::stdout());
        println!();
        println!(
            r#"complete -c codexctl -n '__fish_seen_subcommand_from use remove' -xa '(ls ~/.codexctl/profiles/ 2>/dev/null)'"#
        );
    } else {
        generate(shell, &mut cmd, name, &mut std::io::stdout());
    }

    Ok(())
}
