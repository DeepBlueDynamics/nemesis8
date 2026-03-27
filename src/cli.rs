use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "nemesis8",
    version,
    about = "Run AI agents in Docker. Four providers, 44 MCP tools, one binary."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// AI provider: codex, gemini, claude, or openclaw
    #[arg(long, global = true)]
    pub provider: Option<String>,

    /// Skip all approvals and sandboxing
    #[arg(long, global = true)]
    pub danger: bool,

    /// Docker privileged mode
    #[arg(long, global = true)]
    pub privileged: bool,

    /// Model override
    #[arg(long, global = true)]
    pub model: Option<String>,

    /// Custom workspace mount path
    #[arg(long, global = true)]
    pub workspace: Option<String>,

    /// Start with no workspace mount (scratch container)
    #[arg(long, global = true)]
    pub no_mount: bool,

    /// Gateway port
    #[arg(long, global = true, default_value = "4000")]
    pub port: u16,

    /// Custom Docker image tag
    #[arg(long, global = true)]
    pub tag: Option<String>,

    /// Remote gateway URL (skip local Docker, delegate to remote nemesis8 serve)
    #[arg(long, global = true, env = "NEMESIS8_REMOTE")]
    pub remote: Option<String>,

    /// Auth token for remote gateway
    #[arg(long, global = true, env = "NEMESIS8_TOKEN")]
    pub token: Option<String>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Build the Docker image
    Build,

    /// One-shot exec: run a prompt and exit
    Run {
        /// Prompt to execute
        prompt: String,
    },

    /// Start an interactive session
    Interactive,

    /// Start the HTTP gateway + scheduler
    Serve,

    /// Drop into a container bash shell
    Shell,

    /// Attach to a running nemesis8 container
    Attach {
        /// Container name or ID (from nemesis8 ps)
        container: String,
    },

    /// Stop a running nemesis8 container
    Stop {
        /// Container name, ID, or "all" to stop all
        container: String,
    },

    /// Store API credentials for the current provider
    Login,

    /// List recent sessions
    Sessions,

    /// Resume a previous session (full UUID or last 5 chars)
    Resume {
        /// Session ID (full UUID or last 5 characters)
        id: String,
    },

    /// Sealed containers: capture, build, run, publish
    Pokeball {
        #[command(subcommand)]
        action: PokeballAction,
    },

    /// Manage mount points
    Mount {
        #[command(subcommand)]
        action: MountAction,
    },

    /// List running nemesis8 containers
    Ps,

    /// Create a .nemesis8.toml config in the current directory
    Init,

    /// Check system prerequisites and container runtimes
    Doctor,
}

#[derive(Subcommand)]
pub enum MountAction {
    /// Add a mount point to the config
    Add {
        /// Host path to mount
        host: String,
        /// Container path (optional, defaults to /workspace/<dirname>)
        container: Option<String>,
    },
    /// Remove a mount point from the config
    Remove {
        /// Host path to remove
        host: String,
    },
    /// List current mount points
    List,
}

#[derive(Subcommand)]
pub enum PokeballAction {
    /// Scan a project and generate a pokeball.yaml spec
    Capture {
        /// Path to the project directory or Git URL
        project: String,
    },

    /// Build a sealed Docker image from a pokeball spec
    Build {
        /// Path to pokeball.yaml or project directory
        path: String,
    },

    /// Capture + build in one step
    Seal {
        /// Path to the project directory or Git URL
        project: String,
    },

    /// Start a broker + worker session
    Run {
        /// Pokeball name (as registered in store)
        name: String,
        /// Prompt to execute
        #[arg(long)]
        prompt: Option<String>,
    },

    /// List registered pokeballs
    List,

    /// Show pokeball details
    Inspect {
        /// Pokeball name
        name: String,
    },

    /// Remove a pokeball and its image
    Remove {
        /// Pokeball name
        name: String,
    },

    /// Publish a pokeball to the registry
    Publish {
        /// Pokeball name
        name: String,
        /// Description
        #[arg(long)]
        description: Option<String>,
    },

    /// Pull a pokeball spec from the registry
    Pull {
        /// Pokeball name
        name: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(args)
    }

    #[test]
    fn test_build_command() {
        let cli = parse(&["nemisis8", "build"]).unwrap();
        assert!(matches!(cli.command, Command::Build));
    }

    #[test]
    fn test_run_command_with_prompt() {
        let cli = parse(&["nemisis8", "run", "list files"]).unwrap();
        match cli.command {
            Command::Run { prompt } => assert_eq!(prompt, "list files"),
            _ => panic!("expected Run command"),
        }
    }

    #[test]
    fn test_run_requires_prompt() {
        let result = parse(&["nemisis8", "run"]);
        assert!(result.is_err());
    }

    #[test]
    fn test_resume_with_short_id() {
        let cli = parse(&["nemisis8", "resume", "8d44d"]).unwrap();
        match cli.command {
            Command::Resume { id } => assert_eq!(id, "8d44d"),
            _ => panic!("expected Resume command"),
        }
    }

    #[test]
    fn test_global_flags() {
        let cli = parse(&[
            "nemisis8",
            "--danger",
            "--privileged",
            "--model",
            "gpt-4",
            "--tag",
            "custom:v1",
            "build",
        ])
        .unwrap();
        assert!(cli.danger);
        assert!(cli.privileged);
        assert_eq!(cli.model.as_deref(), Some("gpt-4"));
        assert_eq!(cli.tag.as_deref(), Some("custom:v1"));
    }

    #[test]
    fn test_default_port() {
        let cli = parse(&["nemisis8", "serve"]).unwrap();
        assert_eq!(cli.port, 4000);
    }

    #[test]
    fn test_custom_port() {
        let cli = parse(&["nemisis8", "--port", "8080", "serve"]).unwrap();
        assert_eq!(cli.port, 8080);
    }

    #[test]
    fn test_all_subcommands_parse() {
        for cmd in &[
            vec!["nemisis8", "build"],
            vec!["nemisis8", "run", "hello"],
            vec!["nemisis8", "interactive"],
            vec!["nemisis8", "serve"],
            vec!["nemisis8", "shell"],
            vec!["nemisis8", "login"],
            vec!["nemisis8", "sessions"],
            vec!["nemisis8", "resume", "abc12"],
            vec!["nemisis8", "doctor"],
            vec!["nemisis8", "pokeball", "capture", "/tmp/project"],
            vec!["nemisis8", "pokeball", "build", "/tmp/spec"],
            vec!["nemisis8", "pokeball", "seal", "/tmp/project"],
            vec!["nemisis8", "pokeball", "run", "myapp", "--prompt", "hello"],
            vec!["nemisis8", "pokeball", "list"],
            vec!["nemisis8", "pokeball", "inspect", "myapp"],
            vec!["nemisis8", "pokeball", "remove", "myapp"],
        ] {
            assert!(parse(cmd).is_ok(), "failed to parse: {cmd:?}");
        }
    }

    #[test]
    fn test_unknown_subcommand_fails() {
        assert!(parse(&["nemisis8", "deploy"]).is_err());
    }

    #[test]
    fn test_pokeball_capture() {
        let cli = parse(&["nemisis8", "pokeball", "capture", "/tmp/test"]).unwrap();
        match cli.command {
            Command::Pokeball {
                action: PokeballAction::Capture { project },
            } => assert_eq!(project, "/tmp/test"),
            _ => panic!("expected Pokeball Capture"),
        }
    }

    #[test]
    fn test_pokeball_run_with_prompt() {
        let cli =
            parse(&["nemisis8", "pokeball", "run", "openclaw", "--prompt", "list files"]).unwrap();
        match cli.command {
            Command::Pokeball {
                action: PokeballAction::Run { name, prompt },
            } => {
                assert_eq!(name, "openclaw");
                assert_eq!(prompt.as_deref(), Some("list files"));
            }
            _ => panic!("expected Pokeball Run"),
        }
    }
}
