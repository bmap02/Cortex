//! Cortex: a lightweight, fully local AI agent for the terminal.

mod agent;
mod backend;
mod calc;
mod config;
mod tools;
mod ui;

use agent::Agent;
use backend::{Backend, INTERRUPT};
use clap::{Parser, Subcommand};
use config::{BackendKind, Config};
use rustyline::completion::{Completer, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::{Context, Editor, Helper};
use std::io::{IsTerminal, Read, Write};
use std::sync::atomic::Ordering;

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
    let _ = ctrlc::set_handler(|| INTERRUPT.store(true, Ordering::SeqCst));
    let mut cfg = Config::load();
    if let Some(m) = &cli.model { cfg.model = m.clone(); }
    if let Some(n) = cli.ctx { cfg.num_ctx = n; }

    let code = match cli.cmd.unwrap_or(Cmd::Chat) {
        Cmd::Chat => { chat(cfg, cli.resume); 0 }
        Cmd::Run { prompt, quiet, no_tools } => run_once(cfg, prompt.join(" "), quiet, no_tools),
        Cmd::Setup => { setup(); 0 }
        Cmd::Model { name } => pick_model(cfg, name),
        Cmd::Models { action } => match action.unwrap_or(ModelsCmd::List) {
            ModelsCmd::List => print_models(&cfg),
            ModelsCmd::Pull { name } => match Backend::new(&cfg).pull(&name) {
                Ok(()) => { println!("pulled {name}"); 0 }
                Err(e) => { eprintln!("{}", ui::error(&e.to_string())); 1 }
            },
        },
        Cmd::Tools { action, name } => tools_cmd(cfg, action, name),
        Cmd::Config { action, key, value } => config_cmd(cfg, action, key, value),
        Cmd::Sessions { action, id, yes } => sessions_cmd(action.as_deref().unwrap_or("list"), id.as_deref().unwrap_or(""), yes),
        Cmd::Doctor => doctor(&cfg),
    };
    std::process::exit(code);
}

// ---------------------------------------------------------------- interactive

const SLASH: &[(&str, &str)] = &[
    ("/help", "show commands"),
    ("/new", "start a fresh conversation"),
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
    ("/resume", "load a session: /resume <id>"),
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
    let mut groups: Vec<(String, Vec<String>)> = vec![];
    for t in tools::all().iter().filter(|t| t.enabled(cfg)) {
        match groups.iter_mut().find(|g| g.0 == t.group) {
            Some(g) => g.1.push(t.name.into()),
            None => groups.push((t.group.into(), vec![t.name.into()])),
        }
    }
    if !cfg.allow_network { groups.push(("web".into(), vec!["fetch_url (off · offline mode)".into()])); }
    sections.push(ui::Section { title: "Tools".into(), rows: groups.into_iter().map(|(g, v)| (g, v.join(", "))).collect() });

    let ntools = tools::all().iter().filter(|t| t.enabled(cfg)).count();
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
    if n >= 1024 && n % 1024 == 0 { format!("{}k", n / 1024) } else { n.to_string() }
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
    let mut saved = Config::load();
    saved.num_ctx = n;
    let _ = saved.save();
    println!("{}", ui::muted(&format!("context window → {} tokens (saved as default)", fmt_tokens(n))));
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
        .args(["--query-gpu=name,memory.used,memory.total", "--format=csv,noheader,nounits"]).output() {
        if let Some(line) = String::from_utf8_lossy(&o.stdout).lines().next() {
            let p: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
            if let (Some(name), Some(Ok(u)), Some(Ok(t))) = (p.first(), p.get(1).map(|s| s.parse::<f64>()), p.get(2).map(|s| s.parse::<f64>())) {
                g.push(ui::Gauge { label: "gpu".into(), frac: u / t, detail: format!("{:.1}/{:.1} GB  {name}", u / 1024.0, t / 1024.0) });
            }
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

fn chat(cfg: Config, resume: Option<String>) {
    if !Config::exists() && std::io::stdin().is_terminal() {
        println!("{}", ui::bold("Welcome to Cortex. Let's connect it to your model server first.\n"));
        setup();
        return chat(Config::load(), resume);
    }
    let mut agent = Agent::new(cfg);
    if let Some(id) = resume {
        match agent::load_session(&id) {
            Some(s) => { println!("{}", ui::muted(&format!("resumed session {} ({} messages)", s.id, s.messages.len()))); agent.resume(s) }
            None => println!("{}", ui::error("no matching session; starting fresh")),
        }
    }
    if agent.cfg.show_banner { print!("{}", ui::banner(&banner_info(&agent))); }
    if agent.model.is_empty() { println!("{}", ui::error("No model selected. Use /model <name> (see /models).")); }

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
        if let Err(e) = agent.send(&input, false) { println!("{}", ui::error(&format!("✗ {e}"))); }
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
        "/new" | "/reset" | "/clear" => { agent.save(); agent.reset(); println!("{}", ui::muted("new conversation")); }
        "/model" if arg.is_empty() => println!("  {} {}", ui::bold(&agent.model), ui::muted(&format!("via {} at {}", agent.cfg.backend.label(), agent.cfg.base_url))),
        "/model" => {
            agent.model = arg.to_string();
            agent.tools_supported = true;
            agent.cfg.model = arg.to_string();
            let _ = agent.cfg.save();
            println!("{}", ui::muted(&format!("model → {arg} (saved as default)")));
        }
        "/models" => { print_models(&agent.cfg); }
        "/tools" => print_tools(&agent.cfg),
        "/retry" => match agent.undo() {
            Some(t) => if let Err(e) = agent.send(&t, false) { println!("{}", ui::error(&format!("✗ {e}"))) },
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
        "/resume" | "/load" => match agent::load_session(arg) {
            Some(s) => {
                println!("{}", ui::muted(&format!("resumed {} ({} messages)", s.id, s.messages.len())));
                agent.save();
                agent.resume(s);
            }
            None => println!("{}", ui::error("no matching session (see /sessions)")),
        },
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
    let mut agent = Agent::new(cfg);
    if no_tools { agent.tools_supported = false; }
    // in one-shot mode nobody can answer approval prompts on a pipe
    if !std::io::stdin().is_terminal() && !agent.approve_all {
        agent.cfg.disabled_tools.extend(tools::all().iter().filter(|t| t.needs_approval).map(|t| t.name.to_string()));
    }
    if quiet {
        return match agent.send(&prompt, true) { Ok(_) => { println!(); 0 } Err(e) => { eprintln!("{e}"); 1 } };
    }
    match agent.send(&prompt, false) { Ok(_) => 0, Err(e) => { eprintln!("{}", ui::error(&format!("✗ {e}"))); 1 } }
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

fn pick_model(mut cfg: Config, name: Option<String>) -> i32 {
    let Some(name) = name else { return print_models(&cfg) };
    cfg.model = name.clone();
    match cfg.save() { Ok(()) => { println!("default model → {name}"); 0 } Err(e) => { eprintln!("{e}"); 1 } }
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

fn tools_cmd(mut cfg: Config, action: Option<String>, name: Option<String>) -> i32 {
    match (action.as_deref(), name) {
        (None, _) | (Some("list"), _) => { print_tools(&cfg); 0 }
        (Some(a @ ("enable" | "disable")), Some(n)) => {
            if tools::find(&n).is_none() { eprintln!("unknown tool '{n}'"); return 1; }
            cfg.disabled_tools.retain(|d| d != &n);
            if a == "disable" { cfg.disabled_tools.push(n.clone()); }
            if n == "fetch_url" && a == "enable" { cfg.allow_network = true; }
            match cfg.save() { Ok(()) => { println!("{n}: {a}d"); 0 } Err(e) => { eprintln!("{e}"); 1 } }
        }
        _ => { eprintln!("usage: cortex tools [list | enable <name> | disable <name>]"); 2 }
    }
}

fn config_cmd(mut cfg: Config, action: Option<String>, key: Option<String>, value: Option<String>) -> i32 {
    match (action.as_deref(), key, value) {
        (None, _, _) | (Some("show"), _, _) => {
            println!("{}", ui::faint(&format!("# {}", config::config_path().display())));
            print!("{}", toml::to_string_pretty(&cfg).unwrap_or_default());
            0
        }
        (Some("get"), Some(k), _) => match cfg.get(&k) { Some(v) => { println!("{v}"); 0 } None => { eprintln!("unknown key '{k}'"); 1 } },
        (Some("set"), Some(k), Some(v)) => match cfg.set(&k, &v).and_then(|_| cfg.save().map_err(|e| e.to_string())) {
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
            let n = agent::list_sessions().len();
            if n == 0 { println!("{}", ui::muted("no saved sessions")); return 0; }
            if !yes && !ask(&format!("delete all {n} saved session{}? [y/N]", if n == 1 { "" } else { "s" }), "").eq_ignore_ascii_case("y") {
                println!("{}", ui::muted("kept them"));
                return 0;
            }
            match agent::delete_sessions(None) {
                Ok(d) => { println!("{}", ui::muted(&format!("deleted {d} session{}", if d == 1 { "" } else { "s" }))); 0 }
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
    match be.chat(cfg, &cfg.model, &msgs, &calc, &mut |_| {}) {
        Ok(r) if !r.tool_calls.is_empty() => { println!(); ok(&format!("tool calling works ({:.0} tok/s)", r.usage.tok_per_s)) }
        Ok(r) => { println!(); ok(&format!("chat works ({:.0} tok/s)", r.usage.tok_per_s)); bad("model answered without calling the tool; agent features may be unreliable with this model") }
        Err(backend::BackendError::ToolsUnsupported(_)) => { println!(); bad("this model doesn't support tool calling (chat still works). Try qwen2.5, llama3.1+, mistral-nemo") }
        Err(e) => { println!(); bad(&format!("chat failed: {e}")); return 1; }
    }
    ok(&format!("shell for run_shell: {}", tools::shell_name()));
    if tools::python_available() { ok("python available for run_python") } else { bad("python not found: run_python won't work") }
    0
}
