//! Cortex: a lightweight, fully local AI agent for the terminal.

mod markdown;
mod turn;
mod ui;

use cortex_core::{agent, backend, config, tools};
use agent::Agent;
use backend::Backend;
use cortex_core::cancel::CancelHandle;
use clap::{Parser, Subcommand};
use config::{BackendKind, Config};
use rustyline::completion::{Completer, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::{Context, Editor, Helper};
use std::io::{IsTerminal, Read, Write};
use std::sync::LazyLock;

/// What Ctrl-C stops: the turn in progress (every agent this process makes shares it).
static CTRL_C: LazyLock<CancelHandle> = LazyLock::new(CancelHandle::new);

fn new_agent(cfg: Config) -> Agent {
    let mut a = Agent::new(cfg);
    a.cancel = CTRL_C.clone();
    a
}

#[derive(Parser)]
#[command(name = "cortex", version, about = "A lightweight, fully local AI agent for your terminal")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    /// Override the model for this run
    #[arg(short, long, global = true)]
    model: Option<String>,
    /// Context window in tokens for this run, e.g. --ctx 32k
    #[arg(long, global = true, value_parser = parse_ctx)]
    ctx: Option<usize>,
    /// Resume a saved session (id or prefix; omit the value for the latest)
    #[arg(short, long, num_args = 0..=1, default_missing_value = "")]
    resume: Option<String>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Interactive chat with tools (the default)
    Chat,
    /// One-shot: answer a prompt and exit. Reads stdin too: `cat log | cortex run "explain"`
    Run {
        prompt: Vec<String>,
        /// Print only the answer (no tool chatter or status line)
        #[arg(short, long)]
        quiet: bool,
        /// Disable tools for this run
        #[arg(long)]
        no_tools: bool,
    },
    /// Pick your backend (Ollama or OpenAI-compatible), server URL and model
    Setup,
    /// List models on the server, or pick one: `cortex model llama3.1:8b`
    Model { name: Option<String> },
    /// Model management
    Models {
        #[command(subcommand)]
        action: Option<ModelsCmd>,
    },
    /// Show or toggle tools: `cortex tools`, `cortex tools disable run_shell`
    Tools {
        action: Option<String>,
        name: Option<String>,
    },
    /// Read or change settings: `cortex config`, `cortex config set num_ctx 16384`
    Config {
        action: Option<String>,
        key: Option<String>,
        value: Option<String>,
    },
    /// List saved sessions, or remove them: `cortex sessions clear`, `cortex sessions delete <id>`
    Sessions {
        /// list | clear | delete
        action: Option<String>,
        id: Option<String>,
        /// Don't ask for confirmation
        #[arg(short, long)]
        yes: bool,
    },
    /// Check that the server, model and tool calling work
    Doctor,
    /// Update Cortex to the latest release
    Update,
    /// Remove Cortex from this computer (asks before deleting your settings and conversations)
    Uninstall,
}

#[derive(Subcommand)]
enum ModelsCmd {
    /// List models on the server
    List,
    /// Download a model (Ollama only)
    Pull { name: String },
}

fn main() {
    let cli = Cli::parse();
    let _ = ctrlc::set_handler(|| CTRL_C.cancel());
    let mut cfg = Config::load();
    if let Some(m) = &cli.model { cfg.model = m.clone(); }
    if let Some(n) = cli.ctx { cfg.num_ctx = n; }

    let code = match cli.cmd.unwrap_or(Cmd::Chat) {
        Cmd::Chat => { chat(cfg, cli.resume); 0 }
        Cmd::Run { prompt, quiet, no_tools } => run_once(cfg, prompt.join(" "), quiet, no_tools),
        Cmd::Setup => { setup(); 0 }
        Cmd::Model { name } => pick_model(&cfg, name),
        Cmd::Models { action } => match action.unwrap_or(ModelsCmd::List) {
            ModelsCmd::List => print_models(&cfg),
            ModelsCmd::Pull { name } => match Backend::new(&cfg).pull(&name) {
                Ok(()) => { println!("pulled {name}"); 0 }
                Err(e) => { eprintln!("{}", ui::error(&e.to_string())); 1 }
            },
        },
        Cmd::Tools { action, name } => tools_cmd(&cfg, action, name),
        Cmd::Config { action, key, value } => config_cmd(action, key, value),
        Cmd::Sessions { action, id, yes } => sessions_cmd(action.as_deref().unwrap_or("list"), id.as_deref().unwrap_or(""), yes),
        Cmd::Doctor => doctor(&cfg),
        Cmd::Update => update(),
        Cmd::Uninstall => uninstall(),
    };
    std::process::exit(code);
}

// ---------------------------------------------------------------- interactive

const SLASH: &[(&str, &str)] = &[
    ("/help", "show commands"),
    ("/new", "start a fresh conversation and clear the screen"),
    ("/model", "show or switch model: /model qwen2.5:7b"),
    ("/models", "list models on the server"),
    ("/tools", "list tools"),
    ("/retry", "regenerate the last reply"),
    ("/undo", "remove the last exchange"),
    ("/compress", "summarize the conversation to free context"),
    ("/usage", "token usage this session"),
    ("/context", "show or set the context window: /context 32k"),
    ("/memory", "show long-term memory (~/.cortex/MEMORY.md)"),
    ("/forget", "clear long-term memory"),
    ("/sessions", "list saved sessions · /sessions clear · /sessions delete <id>"),
    ("/resume", "load a session: /resume last, or /resume <id>"),
    ("/yolo", "toggle auto-approve for shell commands and file writes"),
    ("/system", "show the system prompt"),
    ("/exit", "quit (also Ctrl+D)"),
];

struct SlashHelper;
impl Completer for SlashHelper {
    type Candidate = Pair;
    fn complete(&self, line: &str, pos: usize, _: &Context<'_>) -> rustyline::Result<(usize, Vec<Pair>)> {
        if !line.starts_with('/') || line[..pos].contains(' ') { return Ok((0, vec![])); }
        Ok((0, SLASH.iter().filter(|(c, _)| c.starts_with(&line[..pos]))
            .map(|(c, _)| Pair { display: c.to_string(), replacement: format!("{c} ") }).collect()))
    }
}
impl Hinter for SlashHelper {
    type Hint = String;
    fn hint(&self, line: &str, pos: usize, _: &Context<'_>) -> Option<String> {
        if !line.starts_with('/') || pos < line.len() || line.contains(' ') || line.len() < 2 { return None; }
        SLASH.iter().find(|(c, _)| c.starts_with(line)).map(|(c, _)| c[line.len()..].to_string())
    }
}
impl Highlighter for SlashHelper {
    fn highlight_hint<'h>(&self, hint: &'h str) -> std::borrow::Cow<'h, str> { ui::faint(hint).into() }
    // The prompt is passed to readline as plain text so the cursor position is measured
    // correctly; colour is added here. (Colour codes inside the prompt string itself are
    // counted as visible characters on Windows, which pushes the input to the right.)
    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(&'s self, prompt: &'p str, _default: bool) -> std::borrow::Cow<'b, str> {
        if prompt.starts_with('❯') { ui::paint(prompt, ui::WHITE, true).into() } else { ui::faint(prompt).into() }
    }
}
impl Validator for SlashHelper {}
impl Helper for SlashHelper {}

fn banner_info(agent: &Agent) -> ui::BannerInfo {
    let cfg = &agent.cfg;
    let mut sections = vec![];
    match agent.backend.list_models() {
        Ok(models) => {
            let names: Vec<String> = models.iter().map(|m| m.name.clone()).collect();
            sections.push(ui::Section { title: "Models".into(), rows: vec![(cfg.base_url.trim_start_matches("http://").trim_start_matches("https://").into(),
                if names.is_empty() { "none yet: cortex models pull <name>".into() } else { names.join(", ") })] });
        }
        Err(e) => sections.push(ui::Section { title: "Models".into(), rows: vec![("server".into(), format!("unreachable at {} ({})", cfg.base_url, short(&e.to_string())))] }),
    }
    let enabled: Vec<&tools::Tool> = tools::all().iter().filter(|t| t.enabled(cfg)).collect();
    let mut groups: Vec<(String, Vec<String>)> = vec![];
    for t in &enabled {
        match groups.iter_mut().find(|g| g.0 == t.group) {
            Some(g) => g.1.push(t.name.into()),
            None => groups.push((t.group.into(), vec![t.name.into()])),
        }
    }
    if !cfg.allow_network { groups.push(("web".into(), vec!["fetch_url (off · offline mode)".into()])); }
    sections.push(ui::Section { title: "Tools".into(), rows: groups.into_iter().map(|(g, v)| (g, v.join(", "))).collect() });

    let ntools = enabled.len();
    let mem = std::fs::read_to_string(config::memory_path()).map(|s| s.lines().filter(|l| l.starts_with("- ")).count()).unwrap_or(0);
    ui::BannerInfo {
        version: env!("CARGO_PKG_VERSION").into(),
        model: if agent.model.is_empty() { "no model selected".into() } else { agent.model.clone() },
        backend: cfg.backend.label().into(),
        cwd: std::env::current_dir().map(|p| tilde(&p.display().to_string())).unwrap_or_default(),
        session: agent.session_id.clone(),
        sections,
        gauges: gauges(),
        footer: format!("{ntools} tools · {mem} memories · /help for commands"),
    }
}

fn short(s: &str) -> String { ui::truncate(s.split(':').next_back().unwrap_or(s).trim(), 40) }

fn parse_ctx(s: &str) -> Result<usize, String> { config::parse_tokens(s) }

fn fmt_tokens(n: usize) -> String {
    if n >= 1024 && n.is_multiple_of(1024) { format!("{}k", n / 1024) } else { n.to_string() }
}

/// `/context` shows the window; `/context 32k` changes it and saves it as the default.
fn context_cmd(agent: &mut Agent, arg: &str) {
    let max = agent.backend.model_max_context(&agent.model);
    if arg.is_empty() {
        println!("  {} {}", ui::bold(&format!("context window: {} tokens", fmt_tokens(agent.cfg.num_ctx))),
            ui::muted(&format!("(~{} used)", agent.ctx_used())));
        if let Some(m) = max { println!("  {}", ui::muted(&format!("{} supports up to {} tokens", agent.model, fmt_tokens(m)))); }
        println!("  {}", ui::faint("change it with /context 16k, /context 32k, ... (saved as your default)"));
        return;
    }
    let n = match config::parse_tokens(arg) {
        Ok(n) => n,
        Err(e) => { println!("{}", ui::error(&e)); return; }
    };
    agent.cfg.num_ctx = n;
    match Config::update(|c| { c.num_ctx = n; Ok(()) }) {
        Ok(()) => println!("{}", ui::muted(&format!("context window → {} tokens (saved as default)", fmt_tokens(n)))),
        Err(e) => println!("{}", ui::error(&format!("context window → {} tokens for this session; {e}", fmt_tokens(n)))),
    }
    if let Some(m) = max.filter(|m| n > *m) {
        println!("{}", ui::error(&format!("  {} was trained for {} tokens; going past that usually degrades answers.", agent.model, fmt_tokens(m))));
    }
    match agent.cfg.backend {
        BackendKind::Ollama => println!("{}", ui::faint("  Bigger windows use more VRAM. If replies get much slower, the model has spilled into system RAM; step back down.")),
        BackendKind::Openai => println!("{}", ui::faint("  Your server's own limit still applies (llama.cpp -c, LM Studio's context slider). This sets how much Cortex sends before compressing.")),
    }
}

fn tilde(p: &str) -> String {
    match config::user_home().map(|h| h.display().to_string()) {
        Some(h) if !h.is_empty() && p.starts_with(&h) => format!("~{}", &p[h.len()..]),
        _ => p.into(),
    }
}

fn gauges() -> Vec<ui::Gauge> {
    let mut g = vec![];
    // NVIDIA GPU via nvidia-smi when available (no extra dependencies)
    if let Ok(o) = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=name,memory.used,memory.total", "--format=csv,noheader,nounits"]).output()
        && let Some(line) = String::from_utf8_lossy(&o.stdout).lines().next() {
            let p: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
            if let (Some(name), Some(Ok(u)), Some(Ok(t))) = (p.first(), p.get(1).map(|s| s.parse::<f64>()), p.get(2).map(|s| s.parse::<f64>())) {
                g.push(ui::Gauge { label: "gpu".into(), frac: u / t, detail: format!("{:.1}/{:.1} GB  {name}", u / 1024.0, t / 1024.0) });
            }
        }
    let Ok(mut sys) = sysinfo::System::new() else { return g };
    sys.refresh_memory();
    let (used, total) = (sys.used_memory() as f64 / 1e9, sys.total_memory() as f64 / 1e9);
    if total > 0.0 { g.push(ui::Gauge { label: "ram".into(), frac: used / total, detail: format!("{used:.1}/{total:.0} GB") }); }
    sys.refresh_cpu_usage();
    std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
    sys.refresh_cpu_usage();
    let cpu = sys.global_cpu_usage() as f64;
    g.push(ui::Gauge { label: "cpu".into(), frac: cpu / 100.0, detail: format!("{cpu:.0}%  {} cores", sys.cpus().len()) });
    g
}

/// The current folder, if it's inside the Windows folder (e.g. C:\WINDOWS\System32, where
/// Win+R, admin terminals and shortcuts without "Start in" open).
fn in_windows_folder() -> Option<String> {
    if !cfg!(windows) { return None; }
    let sys = std::env::var("SystemRoot").ok()?.to_lowercase();
    let cwd = std::env::current_dir().ok()?.display().to_string();
    let lower = cwd.to_lowercase();
    (lower == sys || lower.starts_with(&format!("{sys}\\"))).then_some(cwd)
}

fn chat(cfg: Config, resume: Option<String>) {
    if !Config::exists() && std::io::stdin().is_terminal() {
        println!("{}", ui::bold("Welcome to Cortex. Let's connect it to your model server first.\n"));
        setup();
        return chat(Config::load(), resume);
    }
    let mut agent = new_agent(cfg);
    if let Some(id) = resume {
        match agent::load_session(&id) {
            Some(s) => { println!("{}", ui::muted(&format!("resumed session {} ({} messages)", s.id, s.messages.len()))); agent.resume(s) }
            None => println!("{}", ui::error("no matching session; starting fresh")),
        }
    }
    if agent.cfg.show_banner { print!("{}", ui::banner(&banner_info(&agent))); }
    if agent.model.is_empty() { println!("{}", ui::error("No model selected. Use /model <name> (see /models).")); }
    if let Some(dir) = in_windows_folder() {
        println!("{}", ui::error(&format!("Cortex is running in {dir}: its file tools start from this Windows system folder, and much of it is locked.")));
        println!("{}", ui::faint("  Quit (/exit), cd to your project folder and run cortex again."));
    }

    let mut rl: Editor<SlashHelper, rustyline::history::DefaultHistory> = match Editor::new() {
        Ok(r) => r,
        Err(e) => { eprintln!("terminal error: {e}"); return; }
    };
    rl.set_helper(Some(SlashHelper));
    let _ = std::fs::create_dir_all(config::home());
    let _ = rl.load_history(&config::history_path());

    loop {
        println!();
        let mut input = match rl.readline("❯ ") {
            Ok(l) => l,
            Err(ReadlineError::Interrupted) => continue,
            Err(ReadlineError::Eof) => break,
            Err(e) => { eprintln!("{e}"); break; }
        };
        // multi-line: end a line with a backslash to keep typing
        while input.ends_with('\\') {
            input.pop();
            input.push('\n');
            match rl.readline("… ") { Ok(l) => input.push_str(&l), Err(_) => break }
        }
        let input = input.trim().to_string();
        if input.is_empty() { continue; }
        let _ = rl.add_history_entry(input.as_str());

        if input.starts_with('/') {
            if !slash(&mut agent, &input) { break; }
            continue;
        }
        if let Err(e) = turn::send(&mut agent, &input, false) { println!("{}", ui::error(&format!("✗ {e}"))); }
    }
    let _ = rl.save_history(&config::history_path());
    agent.save();
    println!("{}", ui::faint(&format!("session saved: {}  (resume with: cortex -r {})", agent.session_id, agent.session_id)));
}

/// Handle a slash command. Returns false to quit.
fn slash(agent: &mut Agent, line: &str) -> bool {
    let (cmd, arg) = line.split_once(' ').map(|(a, b)| (a, b.trim())).unwrap_or((line, ""));
    match cmd {
        "/exit" | "/quit" | "/q" => return false,
        "/help" | "/?" => {
            for (c, d) in SLASH { println!("  {}  {}", ui::bold(&format!("{c:<10}")), ui::muted(d)); }
            println!("  {}", ui::faint("Ctrl+C stops a reply · end a line with \\ for multi-line · Tab completes commands"));
        }
        "/new" | "/reset" | "/clear" => {
            agent.save();
            agent.reset();
            // clear the screen and its scrollback, then start over from the banner
            if ui::color_on() { print!("\x1b[2J\x1b[3J\x1b[H"); }
            if agent.cfg.show_banner { print!("{}", ui::banner(&banner_info(agent))); }
            println!("{}", ui::muted("new conversation"));
        }
        "/model" if arg.is_empty() => println!("  {} {}", ui::bold(&agent.model), ui::muted(&format!("via {} at {}", agent.cfg.backend.label(), agent.cfg.base_url))),
        "/model" => {
            agent.model = arg.to_string();
            agent.tools_supported = true;
            agent.cfg.model = arg.to_string();
            match Config::update(|c| { c.model = arg.to_string(); Ok(()) }) {
                Ok(()) => println!("{}", ui::muted(&format!("model → {arg} (saved as default)"))),
                Err(e) => println!("{}", ui::error(&format!("model → {arg} for this session; {e}"))),
            }
        }
        "/models" => { print_models(&agent.cfg); }
        "/tools" => print_tools(&agent.cfg),
        "/retry" => match agent.undo() {
            Some(t) => if let Err(e) = turn::send(agent, &t, false) { println!("{}", ui::error(&format!("✗ {e}"))) },
            None => println!("{}", ui::muted("nothing to retry")),
        },
        "/undo" => match agent.undo() {
            Some(t) => println!("{}", ui::muted(&format!("removed: {}", ui::truncate(&t.replace('\n', " "), 70)))),
            None => println!("{}", ui::muted("nothing to undo")),
        },
        "/compress" => match agent.compress() {
            Ok(()) => println!("{}", ui::muted(&format!("compressed · ~{} tokens in context now", agent.ctx_estimate()))),
            Err(e) => println!("{}", ui::error(&e)),
        },
        "/usage" => println!("  {}\n  {}",
            ui::muted(&format!("this session: {} prompt + {} completion tokens", agent.total_prompt, agent.total_completion)),
            ui::muted(&format!("context: ~{} / {} tokens · {} messages", agent.ctx_used(), agent.cfg.num_ctx, agent.msgs.len()))),
        "/memory" => match std::fs::read_to_string(config::memory_path()) {
            Ok(m) if !m.trim().is_empty() => println!("{}", m.trim_end()),
            _ => println!("{}", ui::muted("memory is empty. Ask Cortex to remember something.")),
        },
        "/forget" => { let _ = std::fs::remove_file(config::memory_path()); agent.msgs[0].content = agent::system_prompt(&agent.cfg); println!("{}", ui::muted("memory cleared")); }
        "/sessions" => {
            let (action, id) = arg.split_once(' ').map(|(a, b)| (a, b.trim())).unwrap_or((arg, ""));
            let action = if action.is_empty() { "list" } else { action };
            sessions_cmd(action, id, false);
            if action == "clear" {
                println!("{}", ui::faint("  This conversation is still open and will be saved again if you keep chatting. /new starts fresh."));
            }
        }
        "/resume" | "/load" => {
            // "last" (or nothing) means the newest conversation other than this one
            let found = if arg.is_empty() || arg == "last" {
                agent::session_ids().into_iter().filter(|id| *id != agent.session_id).find_map(|id| agent::load_session(&id))
            } else {
                agent::load_session(arg)
            };
            match found {
                Some(s) => {
                    println!("{}", ui::muted(&format!("resumed {} ({} messages)", s.id, s.messages.len())));
                    if !s.model.is_empty() && s.model != agent.model {
                        println!("{}", ui::faint(&format!("  started with {}; continuing with {} (/model {} to switch back)", s.model, agent.model, s.model)));
                    }
                    agent.save();
                    agent.resume(s);
                }
                None => println!("{}", ui::error("no matching session (see /sessions)")),
            }
        }
        "/context" | "/ctx" => context_cmd(agent, arg),
        "/yolo" => {
            agent.approve_all = !agent.approve_all;
            println!("{}", ui::muted(if agent.approve_all { "auto-approve ON: shell commands and file writes run without asking" } else { "auto-approve OFF" }));
        }
        "/system" => println!("{}", ui::faint(&agent.msgs[0].content)),
        _ => println!("{}", ui::muted(&format!("unknown command {cmd}. Try /help"))),
    }
    true
}

// ---------------------------------------------------------------- one-shot

fn run_once(cfg: Config, prompt: String, quiet: bool, no_tools: bool) -> i32 {
    let mut prompt = prompt;
    if !std::io::stdin().is_terminal() {
        let mut piped = String::new();
        let _ = std::io::stdin().read_to_string(&mut piped);
        if !piped.trim().is_empty() { prompt = format!("{prompt}\n\n```\n{}\n```", piped.trim_end()); }
    }
    if prompt.trim().is_empty() { eprintln!("usage: cortex run \"your prompt\""); return 2; }
    let mut agent = new_agent(cfg);
    if no_tools { agent.tools_supported = false; }
    // in one-shot mode nobody can answer approval prompts on a pipe
    if !std::io::stdin().is_terminal() && !agent.approve_all {
        agent.cfg.disabled_tools.extend(tools::all().iter().filter(|t| t.needs_approval).map(|t| t.name.to_string()));
    }
    if quiet {
        return match turn::send(&mut agent, &prompt, true) { Ok(_) => { println!(); 0 } Err(e) => { eprintln!("{e}"); 1 } };
    }
    match turn::send(&mut agent, &prompt, false) { Ok(_) => 0, Err(e) => { eprintln!("{}", ui::error(&format!("✗ {e}"))); 1 } }
}

// ---------------------------------------------------------------- setup & management

fn ask(q: &str, default: &str) -> String {
    if default.is_empty() { print!("{q}: "); } else { print!("{q} {}: ", ui::faint(&format!("[{default}]"))); }
    let _ = std::io::stdout().flush();
    let mut s = String::new();
    let _ = std::io::stdin().read_line(&mut s);
    let s = s.trim();
    if s.is_empty() { default.into() } else { s.into() }
}

fn setup() {
    let mut cfg = Config::load();
    println!("{}", ui::bold("Which server hosts your models?"));
    println!("  1) Ollama              {}", ui::faint("(default http://localhost:11434)"));
    println!("  2) OpenAI-compatible   {}", ui::faint("(llama.cpp server, LM Studio, vLLM, LocalAI, Jan, ...)"));
    let cur = if cfg.backend == BackendKind::Openai { "2" } else { "1" };
    cfg.backend = if ask("choose", cur) == "2" { BackendKind::Openai } else { BackendKind::Ollama };

    let default_url = if Config::exists() && cfg.base_url.contains("://") && (cfg.backend == BackendKind::Openai) == cfg.base_url.ends_with("/v1") {
        cfg.base_url.clone()
    } else {
        cfg.backend.default_url().to_string()
    };
    if cfg.backend == BackendKind::Openai {
        println!("{}", ui::faint("  LM Studio: http://localhost:1234/v1 · llama.cpp: http://localhost:8080/v1 · vLLM: http://localhost:8000/v1"));
    }
    cfg.base_url = ask("server URL", &default_url).trim_end_matches('/').to_string();
    if cfg.backend == BackendKind::Openai {
        cfg.api_key_env = ask("env var holding an API key (blank if none)", &cfg.api_key_env);
    }

    print!("{}", ui::muted("connecting... "));
    let _ = std::io::stdout().flush();
    match Backend::new(&cfg).list_models() {
        Ok(models) if !models.is_empty() => {
            println!("{}", ui::muted(&format!("found {} model(s)", models.len())));
            for (i, m) in models.iter().enumerate() { println!("  {:>2}) {}  {}", i + 1, m.name, ui::faint(&m.detail)); }
            let def = models.iter().position(|m| m.name == cfg.model).map(|i| (i + 1).to_string()).unwrap_or("1".into());
            let pick = ask("model number or name", &def);
            cfg.model = pick.parse::<usize>().ok().and_then(|i| models.get(i.wrapping_sub(1))).map(|m| m.name.clone()).unwrap_or(pick);
        }
        Ok(_) => {
            println!("{}", ui::muted("connected, but the server has no models."));
            if cfg.backend == BackendKind::Ollama {
                let m = ask("model to download now (blank to skip)", "qwen2.5:7b");
                if !m.is_empty() && Backend::new(&cfg).pull(&m).is_ok() { cfg.model = m; }
            } else {
                cfg.model = ask("model name", &cfg.model);
            }
        }
        Err(e) => {
            println!("{}", ui::error(&format!("couldn't connect: {e}")));
            println!("{}", ui::faint("  Saving anyway. Start your server, then run `cortex doctor`."));
            cfg.model = ask("model name", &cfg.model);
        }
    }
    if !cfg.model.is_empty() {
        println!("{}", ui::bold("\nContext window"));
        if let Some(m) = Backend::new(&cfg).model_max_context(&cfg.model) {
            println!("  {}", ui::faint(&format!("{} supports up to {} tokens", cfg.model, fmt_tokens(m))));
        }
        println!("  {}", ui::faint("How much conversation the model can see at once. Bigger remembers more but uses more GPU memory."));
        println!("  {}", ui::faint("8k is light, 16k-32k suits most GPUs with 8 GB+. Change any time with /context."));
        loop {
            match config::parse_tokens(&ask("context window", &fmt_tokens(cfg.num_ctx))) {
                Ok(n) => { cfg.num_ctx = n; break; }
                Err(e) => println!("{}", ui::error(&e)),
            }
        }
    }
    match cfg.save() {
        Ok(()) => println!("{} {}", ui::bold("saved"), ui::muted(&config::config_path().display().to_string())),
        Err(e) => println!("{}", ui::error(&format!("couldn't save config: {e}"))),
    }
}

fn print_models(cfg: &Config) -> i32 {
    match Backend::new(cfg).list_models() {
        Ok(models) => {
            if models.is_empty() { println!("{}", ui::muted("no models on the server")); }
            for m in models {
                let mark = if m.name == cfg.model { ui::bold(ui::DOT) } else { ui::faint("○") };
                println!("  {mark} {:<36} {}", m.name, ui::faint(&m.detail));
            }
            0
        }
        Err(e) => { eprintln!("{}", ui::error(&e.to_string())); 1 }
    }
}

fn pick_model(cfg: &Config, name: Option<String>) -> i32 {
    let Some(name) = name else { return print_models(cfg) };
    match Config::update(|c| { c.model = name.clone(); Ok(()) }) {
        Ok(()) => { println!("default model → {name}"); 0 }
        Err(e) => { eprintln!("{e}"); 1 }
    }
}

fn print_tools(cfg: &Config) {
    for t in tools::all() {
        let state = if t.enabled(cfg) { ui::bold("on ") } else { ui::faint("off") };
        let mut note = String::new();
        if t.needs_approval { note += " · asks first"; }
        if t.network && !cfg.allow_network { note += " · needs allow_network"; }
        println!("  {state} {:<14} {}{}", t.name, ui::muted(t.summary), ui::faint(&note));
    }
}

fn tools_cmd(cfg: &Config, action: Option<String>, name: Option<String>) -> i32 {
    match (action.as_deref(), name) {
        (None | Some("list"), _) => { print_tools(cfg); 0 }
        (Some(a @ ("enable" | "disable")), Some(n)) => {
            if tools::find(&n).is_none() { eprintln!("unknown tool '{n}'"); return 1; }
            let saved = Config::update(|c| {
                c.disabled_tools.retain(|d| d != &n);
                if a == "disable" { c.disabled_tools.push(n.clone()); }
                if n == "fetch_url" && a == "enable" { c.allow_network = true; }
                Ok(())
            });
            match saved { Ok(()) => { println!("{n}: {a}d"); 0 } Err(e) => { eprintln!("{e}"); 1 } }
        }
        _ => { eprintln!("usage: cortex tools [list | enable <name> | disable <name>]"); 2 }
    }
}

/// Shows and changes the saved file, so it ignores this run's --model / --ctx overrides.
fn config_cmd(action: Option<String>, key: Option<String>, value: Option<String>) -> i32 {
    let cfg = Config::load();
    match (action.as_deref(), key, value) {
        (None | Some("show"), _, _) => {
            println!("{}", ui::faint(&format!("# {}", config::config_path().display())));
            print!("{}", toml::to_string_pretty(&cfg).unwrap_or_default());
            0
        }
        (Some("get"), Some(k), _) => match cfg.get(&k) { Some(v) => { println!("{v}"); 0 } None => { eprintln!("unknown key '{k}'"); 1 } },
        (Some("set"), Some(k), Some(v)) => match Config::update(|c| c.set(&k, &v)) {
            Ok(()) => { println!("{k} = {v}"); 0 }
            Err(e) => { eprintln!("{e}"); 1 }
        },
        (Some("path"), _, _) => { println!("{}", config::config_path().display()); 0 }
        _ => { eprintln!("usage: cortex config [show | get <key> | set <key> <value> | path]"); 2 }
    }
}

fn sessions_cmd(action: &str, id: &str, yes: bool) -> i32 {
    match action {
        "list" | "ls" => { print_sessions(); 0 }
        "clear" => {
            let n = agent::session_ids().len();
            if n == 0 { println!("{}", ui::muted("no saved sessions")); return 0; }
            if !yes && !ask(&format!("delete all {}? [y/N]", ui::plural(n, "saved session")), "").eq_ignore_ascii_case("y") {
                println!("{}", ui::muted("kept them"));
                return 0;
            }
            match agent::delete_sessions(None) {
                Ok(d) => { println!("{}", ui::muted(&format!("deleted {}", ui::plural(d, "session")))); 0 }
                Err(e) => { eprintln!("{}", ui::error(&e)); 1 }
            }
        }
        "delete" | "rm" => {
            if id.is_empty() { eprintln!("usage: sessions delete <id>  (see the list for ids)"); return 2; }
            match agent::delete_sessions(Some(id)) {
                Ok(_) => { println!("{}", ui::muted(&format!("deleted session {id}"))); 0 }
                Err(e) => { eprintln!("{}", ui::error(&e)); 1 }
            }
        }
        other => { eprintln!("unknown action '{other}'. Use: list, clear, delete <id>"); 2 }
    }
}

fn print_sessions() {
    let s = agent::list_sessions();
    if s.is_empty() { println!("{}", ui::muted("no saved sessions")); }
    for (id, first, n) in s.iter().take(30) {
        println!("  {}  {}  {}", ui::bold(id), ui::truncate(first, 60), ui::faint(&format!("{n} msgs")));
    }
}

/// "owner/name" of the GitHub repo, from Cargo.toml's `repository`.
fn repo() -> &'static str { env!("CARGO_PKG_REPOSITORY").trim_start_matches("https://github.com/") }

/// "0.1.10" -> [0, 1, 10], so versions compare as numbers ("0.1.10" is newer than "0.1.9").
fn version_parts(v: &str) -> Vec<u64> {
    v.trim().trim_start_matches('v').split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

/// `cortex update`: if a newer release exists, run the official installer (it downloads the
/// release, checks it against SHA256SUMS.txt and swaps the binary, even while Cortex is running).
fn update() -> i32 {
    let current = env!("CARGO_PKG_VERSION");
    let latest = ureq::get(&format!("https://api.github.com/repos/{}/releases/latest", repo()))
        .set("User-Agent", &format!("cortex/{current}")) // GitHub's API requires one
        .timeout(std::time::Duration::from_secs(15))
        .call().ok()
        .and_then(|r| r.into_json::<serde_json::Value>().ok())
        .and_then(|v| v["tag_name"].as_str().map(String::from));
    let Some(latest) = latest else {
        eprintln!("{}", ui::error("couldn't check for updates (no internet connection, or GitHub is unreachable)"));
        return 1;
    };
    if version_parts(&latest) <= version_parts(current) {
        println!("{}", ui::muted(&format!("Cortex v{current} is up to date (latest release: {latest}).")));
        return 0;
    }
    println!("{}", ui::bold(&format!("Updating Cortex v{current} → {latest}")));

    // install into the folder this cortex runs from, so the one on your PATH is the one replaced
    // (e.g. ~/.cargo/bin for `cargo install` builds, not the installer's default folder)
    let dir = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf()));
    let script = format!("https://raw.githubusercontent.com/{}/main/install", repo());
    let mut cmd = if cfg!(windows) {
        let mut c = std::process::Command::new("powershell");
        c.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", &format!("irm {script}.ps1 | iex")]);
        c
    } else {
        let mut c = std::process::Command::new("sh");
        c.args(["-c", &format!("curl -fsSL {script}.sh | sh")]);
        c
    };
    if let Some(d) = &dir { cmd.env("CORTEX_INSTALL_DIR", d); }
    cmd.env("CORTEX_REPO", repo());
    match cmd.status() {
        Ok(s) if s.success() => { println!("{}", ui::bold(&format!("Updated to {latest}."))); 0 }
        Ok(s) => { eprintln!("{}", ui::error(&format!("update failed (installer exit code {})", s.code().unwrap_or(-1)))); 1 }
        Err(e) => { eprintln!("{}", ui::error(&format!("couldn't run the installer: {e}"))); 1 }
    }
}

/// `cortex uninstall`: removes the cortex binary that's running (wherever it was installed) and,
/// only if you say so, the settings folder with your memory and saved conversations.
fn uninstall() -> i32 {
    let Some(exe) = std::env::current_exe().ok() else { eprintln!("{}", ui::error("couldn't find the cortex program")); return 1 };
    println!("This removes Cortex: {}", exe.display());
    if !ask("Continue? [y/N]", "").eq_ignore_ascii_case("y") {
        println!("{}", ui::muted("nothing removed"));
        return 0;
    }
    let home = config::home();
    if home.exists() {
        let q = format!("Also delete your settings, memory and saved conversations in {}? [y/N]", home.display());
        if ask(&q, "").eq_ignore_ascii_case("y") {
            match std::fs::remove_dir_all(&home) {
                Ok(()) => println!("{}", ui::muted(&format!("deleted {}", home.display()))),
                Err(e) => eprintln!("{}", ui::error(&format!("couldn't delete {}: {e}", home.display()))),
            }
        } else {
            println!("{}", ui::muted(&format!("kept {}", home.display())));
        }
    }
    // look before removing: afterwards a copy found on PATH could be this one
    let others = other_copies(&exe);
    if let Err(e) = remove_program(&exe) {
        eprintln!("{}", ui::error(&e));
        return 1;
    }
    // e.g. an old install-script copy hidden behind a `cargo install` build: it would answer to `cortex` next
    if !others.is_empty() {
        println!("{}", ui::bold("Other copies of Cortex are still on your PATH:"));
        for p in &others {
            let version = std::process::Command::new(p).arg("--version").output().ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).filter(|v| !v.is_empty())
                .unwrap_or_else(|| "unknown version".into());
            println!("  {}  {}", p.display(), ui::faint(&format!("({version})")));
        }
        if ask("Remove them too? [y/N]", "").eq_ignore_ascii_case("y") {
            for p in &others {
                if let Err(e) = remove_program(p) { eprintln!("{}", ui::error(&e)); }
            }
        } else {
            println!("{}", ui::muted(&format!("kept them; `cortex` now runs {}", others[0].display())));
        }
    }
    println!("{}", ui::bold("Cortex is uninstalled.")); // on Windows the files disappear a moment after this exits
    println!("{}", ui::faint("  Reinstall any time with the install command from the README."));
    0
}

/// Every other `cortex` program on PATH besides `exe`, in PATH order (the first one is what
/// `cortex` runs next).
fn other_copies(exe: &std::path::Path) -> Vec<std::path::PathBuf> {
    let name = if cfg!(windows) { "cortex.exe" } else { "cortex" };
    // compare resolved paths (the same folder can be on PATH twice, spelled differently),
    // but hand back the normal ones (Windows resolves to \\?\C:\... form)
    let real = |p: &std::path::Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let (mut seen, mut copies) = (vec![real(exe)], vec![]);
    for dir in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        let p = dir.join(name);
        if p.is_file() && !seen.contains(&real(&p)) {
            seen.push(real(&p));
            copies.push(p);
        }
    }
    copies
}

/// Delete a cortex program. On Windows a hidden helper does it once that program isn't running
/// (a running .exe can't be deleted), and also cleans up after the installer's own folder.
fn remove_program(exe: &std::path::Path) -> Result<(), String> {
    if !cfg!(windows) {
        return std::fs::remove_file(exe).map_err(|e| format!("couldn't remove {}: {e}", exe.display()));
    }
    let dir = exe.parent().map(|d| d.to_path_buf()).unwrap_or_default();
    let installer_dir = std::env::var_os("LOCALAPPDATA").map(|d| std::path::PathBuf::from(d).join("Programs").join("cortex"));
    let is_installer_dir = installer_dir.and_then(|d| d.canonicalize().ok()) == dir.canonicalize().ok();
    let script = windows_cleanup_script(exe, &dir, is_installer_dir);
    let mut cmd = std::process::Command::new("powershell");
    cmd.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", &script])
        .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(windows)]
    { use std::os::windows::process::CommandExt; cmd.creation_flags(0x0800_0000); } // CREATE_NO_WINDOW
    cmd.spawn().map(|_| ()).map_err(|e| format!("couldn't remove {}: {e}. Delete it by hand.", exe.display()))
}

/// PowerShell that deletes `exe` once it's no longer running, plus leftovers from `cortex update`.
/// For the installer's own folder it also removes the folder and takes it off the user PATH again.
fn windows_cleanup_script(exe: &std::path::Path, dir: &std::path::Path, installer_dir: bool) -> String {
    let q = |p: &std::path::Path| p.display().to_string().replace('\'', "''"); // PowerShell single-quote escaping
    let mut s = format!(
        "$exe = '{}'; $dir = '{}'\n\
         for ($i = 0; $i -lt 50 -and (Test-Path -LiteralPath $exe); $i++) {{ Start-Sleep -Milliseconds 200; Remove-Item -LiteralPath $exe -Force -ErrorAction SilentlyContinue }}\n\
         Get-ChildItem -LiteralPath $dir -Filter 'cortex.exe*.old' -ErrorAction SilentlyContinue | Remove-Item -Force -ErrorAction SilentlyContinue\n",
        q(exe), q(dir));
    if installer_dir {
        s += "Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue\n\
              $k = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)\n\
              $p = $k.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)\n\
              if ($p) { $kind = $k.GetValueKind('Path'); $keep = @($p -split ';' | Where-Object { $_ -and [Environment]::ExpandEnvironmentVariables($_).TrimEnd('\\') -ne $dir.TrimEnd('\\') }); $k.SetValue('Path', ($keep -join ';'), $kind) }\n\
              $k.Close()\n\
              [Environment]::SetEnvironmentVariable('CORTEX_PATH_REFRESH', '1', 'User'); [Environment]::SetEnvironmentVariable('CORTEX_PATH_REFRESH', $null, 'User')\n";
    }
    s
}

fn doctor(cfg: &Config) -> i32 {
    let ok = |s: &str| println!("  {} {s}", ui::bold("✓"));
    let bad = |s: &str| println!("  {} {s}", ui::error("✗"));
    println!("{}", ui::bold("cortex doctor"));
    if Config::exists() { ok(&format!("config at {}", config::config_path().display())) } else { bad("no config yet: run `cortex setup`") }
    ok(&format!("backend: {} at {}", cfg.backend.label(), cfg.base_url));
    let be = Backend::new(cfg);
    let models = match be.list_models() {
        Ok(m) => { ok(&format!("server reachable, {} model(s)", m.len())); m }
        Err(e) => { bad(&format!("server: {e}")); return 1; }
    };
    if cfg.model.is_empty() { bad("no default model: `cortex model <name>`"); return 1; }
    if models.iter().any(|m| m.name == cfg.model || m.name == format!("{}:latest", cfg.model)) { ok(&format!("model {} is available", cfg.model)) }
    else { bad(&format!("model {} not found on the server", cfg.model)) }

    print!("  … testing a reply and tool calling ");
    let _ = std::io::stdout().flush();
    let msgs = vec![backend::Msg::new("user", "What is 1234 * 5678? Use the calculator tool.")];
    let calc = tools::find("calculator").map(|t| vec![t.schema()]).unwrap_or_default();
    CTRL_C.reset();
    match be.chat(cfg, &cfg.model, &msgs, &calc, &CTRL_C, &mut |_| {}) {
        Ok(r) if !r.tool_calls.is_empty() => { println!(); ok(&format!("tool calling works ({:.0} tok/s)", r.usage.tok_per_s)) }
        Ok(r) => { println!(); ok(&format!("chat works ({:.0} tok/s)", r.usage.tok_per_s)); bad("model answered without calling the tool; agent features may be unreliable with this model") }
        Err(backend::BackendError::ToolsUnsupported(_)) => { println!(); bad("this model doesn't support tool calling (chat still works). Try qwen2.5, llama3.1+, mistral-nemo") }
        Err(e) => { println!(); bad(&format!("chat failed: {e}")); return 1; }
    }
    ok(&format!("shell for run_shell: {}", tools::shell_name()));
    if tools::python_available() { ok("python available for run_python") } else { bad("python not found: run_python won't work") }
    0
}

#[cfg(test)]
mod tests {
    use super::{repo, version_parts, windows_cleanup_script};
    use std::path::Path;

    #[test]
    fn uninstall_script_quotes_paths_and_only_touches_path_for_the_installer_folder() {
        let s = windows_cleanup_script(Path::new(r"C:\Users\o'neil\.cargo\bin\cortex.exe"), Path::new(r"C:\Users\o'neil\.cargo\bin"), false);
        assert!(s.contains(r"$exe = 'C:\Users\o''neil\.cargo\bin\cortex.exe'"), "a ' in a path is escaped: {s}");
        assert!(!s.contains("Registry"), "a cargo install leaves PATH alone");
        let s = windows_cleanup_script(Path::new(r"C:\x\Programs\cortex\cortex.exe"), Path::new(r"C:\x\Programs\cortex"), true);
        assert!(s.contains("Registry") && s.contains("-Recurse"), "the installer folder is removed and taken off PATH");
    }

    #[test]
    fn versions_compare_as_numbers() {
        assert!(version_parts("v0.1.2") > version_parts("0.1.1"));
        assert!(version_parts("v0.1.10") > version_parts("0.1.9")); // not alphabetical
        assert!(version_parts("v0.2.0") > version_parts("0.1.99"));
        assert!(version_parts("v0.1.1") <= version_parts("0.1.1")); // same version: no update
        assert!(version_parts("v0.1.1") <= version_parts("0.2.0")); // a newer dev build isn't downgraded
        assert_eq!(repo(), "bmap02/Cortex");
    }
}
