use anyhow::Result;
use clap::CommandFactory;
use clap_complete::{Shell, generate};

use crate::Cli;

pub fn run(shell: Shell) -> Result<()> {
    let mut cmd = Cli::command();
    let name = cmd.get_name().to_string();

    if shell == Shell::Zsh {
        // Generate the native zsh completion to string, then wrap it so it
        // works when sourced directly (not just via fpath+compinit).
        let mut buf = Vec::new();
        generate(shell, &mut cmd, &name, &mut buf);
        let mut script = String::from_utf8(buf)?;

        // Remove the #compdef line — not needed when sourcing directly
        script = script.replace("#compdef codexctl\n", "");

        // Patch use/remove to dynamically list profiles
        let profile_completer = r#"local profiles_dir="$HOME/.codexctl/profiles"
    if [[ -d "$profiles_dir" ]]; then
        local -a profiles
        profiles=("${(@f)$(ls "$profiles_dir" 2>/dev/null)}")
        _describe -t profiles 'profile' profiles
    fi"#;

        script = script.replace(
            "_codexctl__use_commands() {\n    local commands; commands=()\n    _describe -t commands 'codexctl use commands' commands \"$@\"\n}",
            &format!("_codexctl__use_commands() {{\n    {profile_completer}\n}}"),
        );

        script = script.replace(
            "_codexctl__remove_commands() {\n    local commands; commands=()\n    _describe -t commands 'codexctl remove commands' commands \"$@\"\n}",
            &format!("_codexctl__remove_commands() {{\n    {profile_completer}\n}}"),
        );

        print!("{script}");
        // Register the completion function
        println!("compdef _codexctl codexctl");
    } else if shell == Shell::Bash {
        generate(shell, &mut cmd, name, &mut std::io::stdout());
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
    } else if shell == Shell::Fish {
        generate(shell, &mut cmd, name, &mut std::io::stdout());
        println!();
        println!(r#"# Dynamic profile completion"#);
        println!(
            r#"complete -c codexctl -n '__fish_seen_subcommand_from use remove' -xa '(ls ~/.codexctl/profiles/ 2>/dev/null)'"#
        );
    } else {
        generate(shell, &mut cmd, name, &mut std::io::stdout());
    }

    Ok(())
}
