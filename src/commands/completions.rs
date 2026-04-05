use anyhow::Result;
use clap::CommandFactory;
use clap_complete::{Shell, generate};

use crate::Cli;

pub fn run(shell: Shell) -> Result<()> {
    let mut cmd = Cli::command();
    let name = cmd.get_name().to_string();

    if shell == Shell::Zsh {
        // Generate to string so we can patch it
        let mut buf = Vec::new();
        generate(shell, &mut cmd, &name, &mut buf);
        let mut script = String::from_utf8(buf)?;

        // Patch the use and remove argument completions to list profiles dynamically
        let profile_completer = r#"local profiles_dir="$HOME/.codexctl/profiles"
    if [[ -d "$profiles_dir" ]]; then
        local -a profiles
        profiles=("${(@f)$(ls "$profiles_dir" 2>/dev/null)}")
        _describe -t profiles 'profile' profiles
    fi"#;

        // Replace empty use commands function
        script = script.replace(
            "_codexctl__use_commands() {\n    local commands; commands=()\n    _describe -t commands 'codexctl use commands' commands \"$@\"\n}",
            &format!("_codexctl__use_commands() {{\n    {profile_completer}\n}}"),
        );

        // Replace empty remove commands function
        script = script.replace(
            "_codexctl__remove_commands() {\n    local commands; commands=()\n    _describe -t commands 'codexctl remove commands' commands \"$@\"\n}",
            &format!("_codexctl__remove_commands() {{\n    {profile_completer}\n}}"),
        );

        print!("{script}");
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
