use anyhow::Result;
use clap::CommandFactory;
use clap_complete::{Shell, generate};

use crate::Cli;

pub fn run(shell: Shell) -> Result<()> {
    let mut cmd = Cli::command();
    let name = cmd.get_name().to_string();
    generate(shell, &mut cmd, name, &mut std::io::stdout());

    // For zsh, append a dynamic completion function for profile aliases
    if shell == Shell::Zsh {
        println!();
        println!(r#"# Dynamic profile completion"#);
        println!(r#"_codexctl_profiles() {{"#);
        println!(r#"  local profiles_dir="$HOME/.codexctl/profiles""#);
        println!(r#"  if [[ -d "$profiles_dir" ]]; then"#);
        println!(r#"    compadd -- "$profiles_dir"/*(/:t)"#);
        println!(r#"  fi"#);
        println!(r#"}}"#);
        println!(r#"compdef '_codexctl_profiles' 'codexctl use'"#);
        println!(r#"compdef '_codexctl_profiles' 'codexctl remove'"#);
    }

    if shell == Shell::Bash {
        println!();
        println!(r#"# Dynamic profile completion"#);
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
    }

    if shell == Shell::Fish {
        println!();
        println!(r#"# Dynamic profile completion"#);
        println!(
            r#"complete -c codexctl -n '__fish_seen_subcommand_from use remove' -xa '(ls ~/.codexctl/profiles/ 2>/dev/null)'"#
        );
    }

    Ok(())
}
