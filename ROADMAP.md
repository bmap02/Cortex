# Cortex roadmap

This roadmap covers where Cortex goes after 0.1.x: a desktop GUI alongside the CLI, hardware-aware
model recommendations, and long-term memory you can browse as a map. The order shows dependencies, not
deadlines. Each version can be released on its own.

## Principles

- **Two front-ends, one engine.** The CLI is for people who live in the terminal, and the GUI is for
  people getting into local AI. Both have the same capabilities; only the way they show them differs.
- **Local by default.** Everything runs on the user's machine. Every internet-facing capability the
  agent can use is off until the user turns it on, each with its own switch. Model downloads the user
  starts themselves (catalog, Hugging Face, URL) aren't agent network access and don't need the opt-in.
- **Lightweight means leaving resources for the model.** Cortex itself should use as little RAM, CPU
  and GPU as possible, so the machine's resources go to running the model. The CLI stays a single
  binary of about 5 MB; the GUI is a separate binary, so the CLI never pays for it.
- **The user approves what changes things.** Approvals stay the default everywhere, including for
  unattended runs (through allowlists) and for memory (through batch review). There's never an
  "approve everything" default.
- **Stable over clever.** Plain threads and channels, no async runtime, files people can read.

## Target architecture

```
cortex-core   agent loop, backend (Ollama / OpenAI-compatible), tools, config,
              memory + retrieval, skills, tasks, hardware detection, model catalog
   ├── cortex       CLI (rustyline, terminal Markdown renderer)
   └── cortex-gui   GUI (Iced)
```

- The agent loop runs on a **worker thread** and sends **events** over a channel: text delta, tool
  call started or finished, approval request, usage, done or error. The front-ends render those events;
  the core never prints and never reads stdin.
- Approvals are a request/response pair over the channel: the CLI asks y/n/always, and the GUI shows
  a dialog.
- Stopping uses one cancel flag, set by Ctrl-C in the CLI and by the Stop button in the GUI, which the
  loop already checks.
- HTTP stays blocking (`ureq`). Async was considered and rejected: with one local model, speed depends
  on the model server, and tokio would mean a large rewrite and more dependencies for no gain.

---

## 0.2 Hardware detection and model install

*Depends on: nothing. Can be built alongside 0.3.*

- Detect **RAM, GPU vendor, VRAM and Apple unified memory** on Windows, macOS and Linux.
- A **curated model catalog** bundled with each release (works offline, refreshed by `cortex update`),
  listing tool-capable models with their size, quantization, context length and memory needed.
- `cortex models recommend`: models that fit this machine, ranked. Also used by `cortex setup`, and
  replaces the RAM-only suggestion in `install-windows.ps1`.
- **One-click install through Ollama** from the catalog.
- **Custom models**:
  - Hugging Face GGUF through Ollama's `hf.co/<user>/<repo>:<quant>` pulls.
  - Any GGUF URL: download it, write a Modelfile, run `ollama create`.
  - A **fit check** before installing: file size and quantization compared with the detected VRAM and RAM.
- Other servers (LM Studio, llama.cpp, vLLM, …) get **recommendations only**, no install.

**Decide when 0.2 starts:** which models go in the catalog and how often it's updated; how VRAM is
detected on each platform (vendor tools such as `nvidia-smi` vs. OS APIs).

## 0.3 Core split

*Depends on: nothing. No visible change for users.*

- Turn the repo into a Cargo workspace with `cortex-core` (library) and `cortex` (CLI).
- Move the agent loop from printing directly to the event channel described above; the CLI renders
  the events and behaves exactly as it does today.
- Move approvals and the interrupt behind the core's interfaces.
- Keep the existing tests passing; add tests that drive the core through events, with no terminal.

## 0.4 Memory and recall

*Depends on: 0.3.*

- **Notes as Markdown**: one fact or topic per file under `~/.cortex/memory/`, with frontmatter and
  `[[wikilinks]]`. The files are the source of truth, readable and editable in any text editor, with no
  Obsidian or other app needed.
- **BM25 index** over memory notes and session notes. It's a cache that can be rebuilt from the files
  at any time. **Embeddings are optional**: they switch on when an embedding model (e.g.
  `nomic-embed-text`) is installed.
- **Retrieval that follows links**: BM25 hits **plus notes one link away**, within a fixed token budget
  (around 10% of the context window).
- **Automatic injection** of retrieved notes on each turn, plus a **`recall` tool** for digging deeper.
  This replaces putting all of `MEMORY.md` into the system prompt.
- **Session notes**: when the context is compressed or a session ends, its summary is saved as a note
  linked to the memories it used or created, so past work can still be found once it's out of context.
- **Batch review**: the agent collects candidate memories during a session, and the user approves,
  edits or rejects them all at once at session end or with `/consolidate`. `remember` still works for
  "save this now."
- `/consolidate` also proposes merges and new links.
- **Migration**: an existing `MEMORY.md` is split into notes the first time it runs.
- CLI: `cortex memory list | search | show | links`.

## 0.5 Skills

*Depends on: 0.4.*

- Skills in the open **`SKILL.md`** format (a folder with a frontmatter description) under
  `~/.cortex/skills/`, so existing skills work in Cortex.
- Skills are picked by their description, the same way memory is retrieved.
- The agent can **draft** new skills from work it has done; saving one **always needs approval**.
- CLI: `cortex skills list | show | remove`.

## 0.6 GUI preview

*Depends on: 0.2, 0.4.*

- `cortex-gui` built with **Iced** (MIT license; its Elm-style messages map onto the core's events;
  `Canvas` for the graph). It only repaints on input or while a reply is streaming.
- **Chat**: streaming Markdown and highlighted code (sharing `syntect`), tool steps shown as they run,
  approval dialogs, a Stop button, sessions.
- **First-run wizard**: detect the hardware, recommend models, install one with a click, or import a
  custom model.
- **Memory map**: a graph of notes and links that you can click through to open, edit and see what
  links to a note.
- **Memory review**: the batch-review checklist from 0.4.
- **Network indicator**: always visible while any network capability is on.
- Platforms: Windows stable; macOS and Linux shipped as **preview** until they're tested.

## 0.7 Scheduled tasks and subagents

*Depends on: 0.3 (better with 0.6).*

- **Scheduled tasks** use the OS scheduler (Task Scheduler / launchd / cron or systemd timers) to
  start `cortex run --task <id>` with no front-end. **Nothing runs between tasks.**
- Each task has a **tool allowlist** approved when it's created. Anything outside the list is denied
  and logged.
- Results go to an **inbox**, shown the next time the CLI or GUI opens.
- **Subagents** each get their own fresh context. They run **one after another on the loaded model**
  by default, and in parallel only when hardware detection says the extra model memory fits.

## 0.8 Opt-in network capabilities

*Depends on: 0.7.*

- Separate switches, all **off by default**:
  - web fetch / search (extending the current `fetch_url`)
  - browser and computer use
  - each messaging gateway on its own
- **Messaging gateways** run as a small **background service**, the only feature that does, and only
  once it's turned on.
- The GUI indicator from 0.6 covers all of these.

**Decide when 0.8 starts:** which gateways come first; the browser/computer-use approach and its
safety model.

## 1.0 Stable release

*Depends on: everything above.*

- **Native GUI installers**: `.msi` or `.exe` on Windows, `.dmg` on macOS, AppImage and `.deb` on
  Linux, built by the existing release workflow. **Install commands remain an option**, and the CLI keeps
  its install scripts. The installers offer the GUI with an option to skip it.
- Installers are **unsigned at first**, with documented steps for the SmartScreen and Gatekeeper
  warnings. Code signing comes once there are enough users to justify the cost.
- GUI stable on all three platforms after testing on macOS and Linux.

---

## Dependency graph

```
0.2 hardware ──────────────┐
                           ├──> 0.6 GUI
0.3 core ──> 0.4 memory ───┤
   │                       └──> 0.5 skills
   └──> 0.7 tasks ──> 0.8 network

1.0 needs all of the above.
```

Once a version is ready to start, it becomes a GitHub milestone with issues.
