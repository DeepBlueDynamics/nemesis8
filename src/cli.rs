use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "nemesis8",
    version,
    about = "Run AI agents in Docker. Oodles of providers, tons of agentic tools, one binary. Also available as `n8`.",
    long_about = "Run AI agents in Docker — one binary, many providers \
(codex, claude, antigravity, grok, … and any you install).\n\n\
START          run (one-shot) · interactive (TTY) · shell (bare container)\n\
GET BACK IN    resume / attach — unified picker of running containers + past sessions; resume lands in the session's original workspace (Ctrl+Enter or . = current dir)\n\
SEARCH         sessions <query> — full-text BM25 search across transcript content, not just ids/paths\n\
CONTROL PLANE  serve (resident gateway + scheduler; daemon via --background/--status/--stop) + agents (cross-host fleet: list / kill / spawn)\n\
EXTEND         mcp — add / list / remove agent tools (local .py or remote MCP URLs)\n\
INTEGRATE      auto-detects Hyperia (terminal) and Ferricula (storage) when running\n\n\
Global flags (--provider/--model/--danger/--workspace/…) apply to run, interactive, and resume."
)]
pub struct Cli {
    /// Subcommand. Omit (bare `n8` / `n8 --danger`) to open the home screen.
    #[command(subcommand)]
    pub command: Option<Command>,

    /// AI provider: codex, claude, antigravity, grok — or any installed provider
    #[arg(long, global = true)]
    pub provider: Option<String>,

    /// Skip all approvals and sandboxing
    #[arg(long, global = true)]
    pub danger: bool,

    /// Docker privileged mode
    #[arg(long, global = true)]
    pub privileged: bool,

    /// GPU. On `build`: bake NVIDIA support (CUDA runtime + capabilities) into the
    /// image (~1.2 GB). On `run`/`interactive`: expose host GPUs (docker --gpus all)
    /// — needs an image built with `--gpu`, else n8 warns and runs CPU-only.
    #[arg(long, global = true)]
    pub gpu: bool,

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

    /// Publish a container port to the host (repeatable): "3000", "8080:80",
    /// or "0.0.0.0:8080:80". Binds 127.0.0.1 unless an ip is given. Lets you
    /// reach servers the agent starts inside its container. Also configurable
    /// via `ports = [...]` in .nemesis8.toml.
    #[arg(long, global = true)]
    pub publish: Vec<String>,

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
    /// Build the agent image (nemesis8:latest) on top of the published base.
    ///
    /// Bare `n8 build` makes the lean CPU image (~6.9 GB) and, on a terminal,
    /// asks about the optional heavyweight layers below. Pass the flags to skip
    /// the prompt (scripts/CI):
    ///   --gpu      NVIDIA GPU support — CUDA runtime + cuDNN, ~+3.6 GB. Then run
    ///              agents with `n8 --gpu`. (--gpu is a GLOBAL flag.)
    ///   --ffmpeg   latest ffmpeg static build, ~+80 MB.
    ///   --native   C/C++ build toolchain so agents can COMPILE native code
    ///              (cargo build, C, node-gyp, Python C extensions), ~+300 MB.
    ///
    /// The base image (nemesis8-base) is PULLED, never built here — it changes
    /// only via CI when Dockerfile.base / requirements.txt change.
    Build {
        /// Output JSON progress lines instead of TUI (for Hyperia integration)
        #[arg(long)]
        json_progress: bool,

        /// Include the latest ffmpeg static build in the image (adds ~80 MB)
        #[arg(long)]
        ffmpeg: bool,

        /// Include a C/C++ build toolchain (gcc, make, headers, pkg-config,
        /// libssl-dev) so agents can COMPILE native code — cargo build/test,
        /// C, node-gyp, Python C extensions (adds ~300 MB). Without it
        /// `cargo check` works but `cargo build` fails with "cc not found".
        #[arg(long)]
        native: bool,

        /// Install the `glint` terminal-dashboard app into the image, runnable
        /// from the home screen's New → Type: App (adds ~15 MB).
        #[arg(long)]
        glint: bool,
    },

    /// One-shot exec: run a prompt and exit (non-interactive)
    Run {
        /// Prompt to execute
        prompt: String,
    },

    /// Start an interactive agent session (TTY)
    Interactive,

    /// Serve the trainer API standalone (tool-run training data for Sailfish,
    /// localhost-only on :18042 — the wired port Sailfish expects). Also
    /// starts automatically with `serve`.
    Trainer,

    /// Start the control-plane gateway + scheduler (daemon: --background / --status / --stop)
    Serve {
        /// Detach and run in the background (writes a PID + log file)
        #[arg(long)]
        background: bool,

        /// Show whether the background gateway is running, then exit
        #[arg(long)]
        status: bool,

        /// Stop the background gateway, then exit
        #[arg(long)]
        stop: bool,
    },

    /// Drop into a container bash shell
    Shell,

    /// Attach to a running nemesis8 container. With no arg, opens the unified
    /// resume/attach picker (running containers + past sessions in one list).
    Attach {
        /// Container name or ID (from nemesis8 ps). Omit to open the picker.
        container: Option<String>,
    },

    /// Stop a running container (name, ID, or "all")
    Stop {
        /// Container name, ID, or "all" to stop all
        container: String,
    },

    /// Store API credentials for the current provider
    Login,

    /// List recent sessions, or full-text search transcript content with a query
    Sessions {
        /// Query: BM25 full-text search across transcript content, plus
        /// id/workspace substring match. Omit to list recent sessions.
        query: Option<String>,
    },

    /// Resume a previous session. With no id, opens an interactive picker
    /// listing every session (codex / antigravity / ...). Provider
    /// is auto-detected from the session path so you never need --provider.
    Resume {
        /// Optional session ID — full UUID, or its first 5 or last 5 chars.
        /// Omit to open the picker.
        id: Option<String>,
    },

    /// Fleet control: list / kill / spawn agents across the control plane
    Agents {
        #[command(subcommand)]
        action: Option<AgentsAction>,
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

    /// Update nemesis8 to the latest release
    Update,

    /// Manage MCP tools (add / list / remove — local .py files or remote MCP URLs)
    Mcp {
        #[command(subcommand)]
        action: McpAction,
    },

    /// Dependency services: bring up / status / down / logs the containers agents
    /// depend on (Ferricula, …) from declarative services/*.toml templates.
    Services {
        #[command(subcommand)]
        action: ServicesAction,
    },

    /// Interactive home screen (bare `n8`): new session + resume/attach control room.
    #[command(hide = true)]
    Home,
}

#[derive(Subcommand, Debug)]
pub enum McpAction {
    /// Copy a .py file into the MCP tool directory, install any deps, and register it
    Add {
        /// Path to the MCP tool .py file
        file: std::path::PathBuf,
        /// Extra pip packages to install (in addition to any # requires: header in the file)
        #[arg(long, value_delimiter = ',')]
        requires: Vec<String>,
    },
    /// List installed MCP tools
    List,
    /// Remove an MCP tool and deregister it
    Remove {
        /// Tool filename (e.g. gads.py)
        name: String,
    },
    /// Generate + validate every provider's MCP config from this workspace's
    /// tools — the same code path the container runs — and report PASS/FAIL
    /// with the exact schema problems. Run after changing tools or providers.
    Test {
        /// Only test one provider (e.g. antigravity, opencode)
        #[arg(long)]
        provider: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum ServicesAction {
    /// Bring a service up (pull/build → start → wait healthy). Omit NAME for all
    /// templates marked `enabled = true`.
    Up {
        /// Service name (from a services/*.toml template). Omit for all enabled.
        name: Option<String>,
    },
    /// Show managed services and their state/health.
    Status,
    /// Stop + remove a service. Omit NAME to take down every managed service.
    Down {
        name: Option<String>,
    },
    /// Tail a service container's logs.
    Logs {
        name: String,
    },
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
pub enum AgentsAction {
    /// List all agents the control plane knows about (default)
    List,
    /// Kill an agent by id (local_id, global host/local, or prefix)
    Kill {
        id: String,
    },
    /// Spawn a new agent with a one-shot prompt
    Spawn {
        prompt: String,
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
        let cli = parse(&["nemesis8", "build"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Build { .. })));
    }

    #[test]
    fn test_run_command_with_prompt() {
        let cli = parse(&["nemesis8", "run", "list files"]).unwrap();
        match cli.command {
            Some(Command::Run { prompt }) => assert_eq!(prompt, "list files"),
            _ => panic!("expected Run command"),
        }
    }

    #[test]
    fn test_run_requires_prompt() {
        let result = parse(&["nemesis8", "run"]);
        assert!(result.is_err());
    }

    #[test]
    fn test_resume_with_short_id() {
        let cli = parse(&["nemesis8", "resume", "8d44d"]).unwrap();
        match cli.command {
            Some(Command::Resume { id }) => assert_eq!(id.as_deref(), Some("8d44d")),
            _ => panic!("expected Resume command"),
        }
    }

    #[test]
    fn test_resume_without_id_opens_picker() {
        let cli = parse(&["nemesis8", "resume"]).unwrap();
        match cli.command {
            Some(Command::Resume { id }) => assert!(id.is_none()),
            _ => panic!("expected Resume command"),
        }
    }

    #[test]
    fn test_bare_n8_has_no_command() {
        // Bare `n8` (and `n8 --danger`) parse with no subcommand → home screen.
        assert!(parse(&["nemesis8"]).unwrap().command.is_none());
        assert!(parse(&["nemesis8", "--danger"]).unwrap().command.is_none());
    }

    #[test]
    fn test_global_flags() {
        let cli = parse(&[
            "nemesis8",
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
        let cli = parse(&["nemesis8", "serve"]).unwrap();
        assert_eq!(cli.port, 4000);
    }

    #[test]
    fn test_custom_port() {
        let cli = parse(&["nemesis8", "--port", "8080", "serve"]).unwrap();
        assert_eq!(cli.port, 8080);
    }

    #[test]
    fn test_all_subcommands_parse() {
        for cmd in &[
            vec!["nemesis8", "build"],
            vec!["nemesis8", "run", "hello"],
            vec!["nemesis8", "interactive"],
            vec!["nemesis8", "serve"],
            vec!["nemesis8", "shell"],
            vec!["nemesis8", "login"],
            vec!["nemesis8", "sessions"],
            vec!["nemesis8", "resume", "abc12"],
            vec!["nemesis8", "doctor"],
            vec!["nemesis8", "update"],
        ] {
            assert!(parse(cmd).is_ok(), "failed to parse: {cmd:?}");
        }
    }

    #[test]
    fn test_unknown_subcommand_fails() {
        assert!(parse(&["nemesis8", "deploy"]).is_err());
    }

    #[test]
    fn test_mcp_subcommands() {
        assert!(parse(&["nemesis8", "mcp", "list"]).is_ok());
        assert!(parse(&["nemesis8", "mcp", "add", "/tmp/tool.py"]).is_ok());
        assert!(parse(&["nemesis8", "mcp", "remove", "tool.py"]).is_ok());
    }
}
