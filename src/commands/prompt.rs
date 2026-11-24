// ===========================================================================
// prompt.rs — `session prompt` (send / schedule / list / cancel), `session
// list --server`, and the `daemon` that delivers scheduled prompts.
//
// This absorbs `opencode-prompt` + `opencode-sessions` from the old Nushell
// config, plus the scheduler on top. Delivery goes through the OpenCode
// background service's HTTP API (see `oc::server`); scheduled jobs live in
// the locked JSON queue (see `oc::schedule`).
//
// Daemon loop, per ~20 s tick:
//   1. lock queue → load → mark every due job `delivering` (attempts +1 on
//      retries) → save → unlock;
//   2. POST each outside the lock (switch agent/model first when asked);
//      success → `delivered`, failure → back to `pending` (or `failed` once
//      MAX_ATTEMPTS is reached, keeping the last error);
//   3. lock → save final states → unlock.
// A crash between (1) and (3) is recovered on the next load: `delivering`
// normalizes to `pending`, and the rare redelivered prompt beats a silently
// dropped one.
// ===========================================================================

use crate::commands::util::{Output, prompt_line};
use anyhow::{Context, Result, bail};
use mdrv_oc as oc;
use mdrv_oc::schedule::{self, Job, JobState, ModelRef, Queue};
use mdrv_oc::server::{self, Endpoint, ServerSession};
use std::io::{IsTerminal, Read};
use std::time::Duration;

/// Everything `session prompt` needs for the send-or-schedule path.
pub(crate) struct PromptArgs {
    pub text: Vec<String>,
    pub file: Option<std::path::PathBuf>,
    pub session: Option<String>,
    pub at: Option<String>,
    pub in_: Option<String>,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub url: Option<String>,
}

/// `mdrv-oc session prompt [TEXT] ...` — deliver now, or queue for the daemon.
pub(crate) fn cmd_session_prompt(args: PromptArgs) -> Result<()> {
    let text = resolve_text(&args.text, args.file.as_deref())?;
    let ep = server::resolve_endpoint(args.url.as_deref())
        .context("locating the OpenCode server for prompt delivery")?;

    // Parse `--at`/`--in` before anything else so a typo fails fast.
    let due_ms = if args.at.is_some() || args.in_.is_some() {
        Some(schedule::parse_due(
            args.at.as_deref(),
            args.in_.as_deref(),
            &jiff::Zoned::now(),
        )?)
    } else {
        None
    };

    let target = resolve_target(&ep, args.session.as_deref())?;

    if let Some(due_ms) = due_ms {
        schedule_job(&target, &text, due_ms, args.agent, args.model)?;
    } else {
        deliver_now(&ep, target, &text, args.agent, args.model)?;
    }
    Ok(())
}

/// Prompt text source: joined positional args (single `-` = stdin), else
/// `--file` (`-` = stdin). Requiring one explicit source beats silently
/// reading an empty stdin.
fn resolve_text(words: &[String], file: Option<&std::path::Path>) -> Result<String> {
    let text = if !words.is_empty() {
        if words.len() == 1 && words[0] == "-" {
            read_stdin()?
        } else {
            words.join(" ")
        }
    } else if let Some(path) = file {
        if path.as_os_str() == "-" {
            read_stdin()?
        } else {
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?
        }
    } else {
        bail!("no prompt text given: pass TEXT, --file <PATH>, or \"-\" for stdin")
    };
    if text.trim().is_empty() {
        bail!("prompt text is empty");
    }
    Ok(text)
}

fn read_stdin() -> Result<String> {
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .context("reading prompt text from stdin")?;
    Ok(buf)
}

/// The session to prompt: `--session` (exact id, then unique prefix across
/// all projects), else the interactive picker.
fn resolve_target(ep: &Endpoint, session: Option<&str>) -> Result<ServerSession> {
    match session {
        Some(sel) => {
            let all = server::sessions(ep, 1000)?;
            if let [one] = all.iter().filter(|s| s.id == sel).collect::<Vec<_>>()[..] {
                return Ok(one.clone());
            }
            let prefix: Vec<_> = all.iter().filter(|s| s.id.starts_with(sel)).collect();
            match prefix.len() {
                1 => Ok(prefix[0].clone()),
                0 => bail!(
                    "no server session matches {sel:?} ({} session(s) known to the server)",
                    all.len()
                ),
                n => bail!(
                    "{sel:?} matched {n} sessions; use a longer prefix:\n{}",
                    prefix
                        .iter()
                        .take(8)
                        .map(|s| format!("  • {}", display_id_title(s)))
                        .collect::<Vec<_>>()
                        .join("\n")
                ),
            }
        }
        None => pick_session(ep),
    }
}

fn display_id_title(s: &ServerSession) -> String {
    let title = s.title.as_deref().unwrap_or("(untitled)");
    format!(
        "{}  {}  [{}]",
        s.id,
        title,
        s.directory.as_deref().unwrap_or("?")
    )
}

/// Interactive numbered picker over the server's sessions — same style as the
/// import picker (numbered list + stdin line).
fn pick_session(ep: &Endpoint) -> Result<ServerSession> {
    if !std::io::stdin().is_terminal() {
        bail!("no --session given and stdin is not a terminal: pass --session <ID>")
    }
    let all = server::sessions(ep, 1000)?;
    if all.is_empty() {
        bail!("the OpenCode server reports no sessions");
    }
    println!("sessions (newest first per project):");
    for (i, s) in all.iter().take(50).enumerate() {
        println!("  {:>3})  {}", i + 1, display_id_title(s));
    }
    if all.len() > 50 {
        println!(
            "  ... {} more (pass --session <ID> directly)",
            all.len() - 50
        );
    }
    let Some(line) = prompt_line("session number (or empty to cancel)? ")? else {
        bail!("cancelled");
    };
    let n: usize = line.trim().parse().context("not a number")?;
    if n == 0 || n > all.len().min(50) {
        bail!("cancelled");
    }
    Ok(all[n - 1].clone())
}

/// Immediate delivery: optional agent/model switch, then POST the prompt.
fn deliver_now(
    ep: &Endpoint,
    target: ServerSession,
    text: &str,
    agent: Option<String>,
    model: Option<String>,
) -> Result<()> {
    apply_switches(ep, &target.id, agent, model)?;
    server::prompt(ep, &target.id, text)?;
    println!(
        "✓ sent to {} ({})",
        target.title.as_deref().unwrap_or("(untitled)"),
        target.id
    );
    Ok(())
}

fn apply_switches(
    ep: &Endpoint,
    session_id: &str,
    agent: Option<String>,
    model: Option<String>,
) -> Result<()> {
    if let Some(agent) = agent {
        server::switch_agent(ep, session_id, &agent)
            .with_context(|| format!("switching agent to {agent:?}"))?;
    }
    if let Some(model) = model {
        let (provider, id) = parse_model(&model)?;
        server::switch_model(ep, session_id, &provider, &id)
            .with_context(|| format!("switching model to {model:?}"))?;
    }
    Ok(())
}

fn parse_model(s: &str) -> Result<(String, String)> {
    let (provider, id) = s.split_once('/').ok_or_else(|| {
        anyhow::anyhow!("--model expects PROVIDER/ID, e.g. zai-coding-plan/glm-5.3-flash")
    })?;
    if provider.is_empty() || id.is_empty() {
        anyhow::bail!("--model expects PROVIDER/ID, e.g. zai-coding-plan/glm-5.3-flash");
    }
    Ok((provider.to_owned(), id.to_owned()))
}

/// Queue the job, warn when no daemon is watching, and confirm.
/// A due time already gone is legal: the daemon fires it once, immediately.
fn warn_if_past(due_ms: i64) {
    if due_ms < schedule::now_ms() {
        eprintln!("⚠ that time has passed — the prompt fires on the next daemon tick, late");
    }
}

fn schedule_job(
    target: &ServerSession,
    text: &str,
    due_ms: i64,
    agent: Option<String>,
    model: Option<String>,
) -> Result<()> {
    let model = model
        .map(|m| parse_model(&m))
        .transpose()?
        .map(|(provider, id)| ModelRef { provider, id });

    let mut job = Job {
        id: String::new(),
        created_ms: schedule::now_ms(),
        due_ms,
        session_id: target.id.clone(),
        session_title: target.title.clone(),
        text: text.to_owned(),
        agent,
        model,
        state: JobState::Pending,
        attempts: 0,
        last_attempt_ms: None,
        last_error: None,
        done_ms: None,
    };

    let job_id = Queue::with_locked(|q| {
        job.id = q.new_job_id();
        q.jobs.push(job.clone());
        Ok(job.id.clone())
    })?;

    warn_if_past(due_ms);
    if !schedule::daemon_running() {
        eprintln!(
            "⚠ no mdrv-oc daemon is running — the prompt will wait until you start one:\n  mdrv-oc daemon"
        );
    }
    println!(
        "⏱ scheduled {} → {} ({}) due {}",
        job_id,
        target.title.as_deref().unwrap_or("(untitled)"),
        target.id,
        schedule::format_ms(due_ms)
    );
    Ok(())
}

/// `session prompt list` — pending first (soonest due at top), then finished.
pub(crate) fn cmd_prompt_list(output: Output) -> Result<()> {
    let q = Queue::load()?;
    let mut pending: Vec<&Job> = q
        .jobs
        .iter()
        .filter(|j| matches!(j.state, JobState::Pending | JobState::Delivering))
        .collect();
    pending.sort_by_key(|j| (j.due_ms, j.id.clone()));
    let mut finished: Vec<&Job> = q
        .jobs
        .iter()
        .filter(|j| matches!(j.state, JobState::Delivered | JobState::Failed))
        .collect();
    finished.sort_by_key(|j| (j.done_ms.unwrap_or(0), j.id.clone()));

    let payload = serde_json::json!({
        "pending": pending.iter().map(|j| serde_json::json!({
            "id": j.id,
            "due": schedule::format_ms(j.due_ms),
            "due_ms": j.due_ms,
            "session": j.session_id,
            "title": j.session_title,
            "text": j.text,
            "attempts": j.attempts,
            "state": j.state,
            "last_error": j.last_error,
        })).collect::<Vec<_>>(),
        "recent": finished.iter().rev().take(20).map(|j| serde_json::json!({
            "id": j.id,
            "state": j.state,
            "session": j.session_id,
            "title": j.session_title,
            "text": j.text,
            "last_error": j.last_error,
        })).collect::<Vec<_>>(),
    });
    output.emit(&payload, || {
        if pending.is_empty() && finished.is_empty() {
            println!("no scheduled or recent prompts");
            return;
        }
        if !pending.is_empty() {
            println!("pending:");
            for j in pending {
                let retry = if j.attempts > 0 {
                    format!(
                        ", attempt {}/{}: {}",
                        j.attempts + 1,
                        schedule::MAX_ATTEMPTS,
                        j.last_error.as_deref().unwrap_or("waiting to retry")
                    )
                } else {
                    String::new()
                };
                println!(
                    "  {}  {}  {} [{}]\n        {}{}",
                    j.id,
                    schedule::format_ms(j.due_ms),
                    j.session_title.as_deref().unwrap_or("?"),
                    j.session_id,
                    first_line(&j.text, 90),
                    retry,
                );
            }
        }
        if !finished.is_empty() {
            println!("recent:");
            for j in finished.iter().rev().take(20) {
                let state = match j.state {
                    JobState::Delivered => "✓ delivered",
                    JobState::Failed => "✗ failed",
                    _ => "…",
                };
                let mut line = format!(
                    "  {}  {}  {} [{}]  {}",
                    j.id,
                    state,
                    j.session_title.as_deref().unwrap_or("?"),
                    j.session_id,
                    first_line(&j.text, 60),
                );
                if let Some(err) = &j.last_error {
                    line.push_str(&format!(" — {err}"));
                }
                println!("{line}");
            }
        }
    });
    Ok(())
}

/// `session prompt cancel <id|prefix>`
pub(crate) fn cmd_prompt_cancel(id: &str, output: Output) -> Result<()> {
    // The closure must speak the library's error type (`oc::Error`); the
    // `?` at the call site converts it into anyhow for main.
    let removed = Queue::with_locked(|q| {
        let idx = q.find_job(id)?;
        q.cancel_job(idx, None)
    })?;
    let removed = format!(
        "✗ cancelled {} → {} at {}",
        removed.id,
        removed.session_title.as_deref().unwrap_or("?"),
        schedule::format_ms(removed.due_ms)
    );
    output.emit(&serde_json::json!({ "cancelled": removed }), || {
        println!("{removed}")
    });
    Ok(())
}

/// `session prompt edit <id|prefix>` — change text, target session, or due
/// time of a pending job. Every field is optional; at least one is required.
pub(crate) struct EditArgs {
    pub id: String,
    pub text: Option<String>,
    pub file: Option<std::path::PathBuf>,
    pub session: Option<String>,
    pub at: Option<String>,
    pub in_: Option<String>,
    pub url: Option<String>,
}

pub(crate) fn cmd_prompt_edit(args: EditArgs, output: Output) -> Result<()> {
    use mdrv_oc::schedule::{JobExpect, JobPatch};

    if args.text.is_none()
        && args.file.is_none()
        && args.session.is_none()
        && args.at.is_none()
        && args.in_.is_none()
    {
        bail!("nothing to edit: pass --text/--file, --session, or --at/--in");
    }

    // Parse the time before taking the queue lock so a typo fails fast.
    let due_ms = if args.at.is_some() || args.in_.is_some() {
        Some(schedule::parse_due(
            args.at.as_deref(),
            args.in_.as_deref(),
            &jiff::Zoned::now(),
        )?)
    } else {
        None
    };

    let text = match (&args.text, &args.file) {
        (Some(text), _) => {
            if text.trim().is_empty() {
                bail!("prompt text is empty");
            }
            Some(text.clone())
        }
        (None, Some(path)) if path.as_os_str() == "-" => Some(read_stdin()?),
        (None, Some(path)) => Some(
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?,
        ),
        (None, None) => None,
    };

    let session = args
        .session
        .as_deref()
        .map(|sel| resolve_session_swap(args.url.as_deref(), sel))
        .transpose()?;

    let mut changed: Vec<&str> = Vec::new();
    if text.is_some() {
        changed.push("text");
    }
    if session.is_some() {
        changed.push("session");
    }
    if due_ms.is_some() {
        changed.push("due");
    }

    let patch = JobPatch {
        text,
        session,
        due_ms,
    };
    let updated = Queue::with_locked(|q| {
        let idx = q.find_job(&args.id)?;
        // The CLI read the job under the same lock, so this cannot be stale;
        // `update_job` still enforces the in-flight and finished rules.
        let expect = JobExpect::of(&q.jobs[idx]);
        q.update_job(idx, &expect, &patch)
    })?;

    warn_if_past(updated.due_ms);
    let summary = format!(
        "✓ updated {} ({}): {} · due {}",
        updated.id,
        updated
            .session_title
            .as_deref()
            .unwrap_or(&updated.session_id),
        changed.join(", "),
        schedule::format_ms(updated.due_ms)
    );
    output.emit(&serde_json::json!({ "updated": updated }), || {
        println!("{summary}")
    });
    Ok(())
}

/// Resolve a new `--session` value for an edit: a running server makes it
/// strict (exact id or unique prefix, as at creation); with no server the id
/// is accepted unvalidated, because delivery retries and reports the failure.
fn resolve_session_swap(url: Option<&str>, sel: &str) -> Result<oc::schedule::SessionSwap> {
    let swap = |title| oc::schedule::SessionSwap {
        id: sel.to_owned(),
        title,
    };
    let ep = match server::resolve_endpoint(url) {
        Ok(ep) => ep,
        Err(e) => {
            eprintln!("⚠ no OpenCode server ({e:#}); accepting {sel:?} unvalidated");
            return Ok(swap(None));
        }
    };
    let all = server::sessions(&ep, 1000)?;
    if let [one] = all.iter().filter(|s| s.id == sel).collect::<Vec<_>>()[..] {
        return Ok(swap(one.title.clone()));
    }
    let prefix: Vec<_> = all.iter().filter(|s| s.id.starts_with(sel)).collect();
    match prefix.len() {
        1 => Ok(swap(prefix[0].title.clone())),
        0 => {
            eprintln!(
                "⚠ no server session matches {sel:?} ({} known); accepted unvalidated",
                all.len()
            );
            Ok(swap(None))
        }
        n => bail!(
            "{sel:?} matched {n} sessions; use a longer prefix:\n{}",
            prefix
                .iter()
                .take(8)
                .map(|s| format!("  • {}", display_id_title(s)))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    }
}

fn first_line(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or("");
    match s.lines().count() {
        1 => truncate_str(line, max),
        n => format!("{} …(+{} lines)", truncate_str(line, max), n - 1),
    }
}

fn truncate_str(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_owned(),
    }
}

/// `session list --server` — the absorbed `opencode-sessions`.
pub(crate) fn cmd_session_list_server(
    directory: Option<&str>,
    limit: u32,
    output: Output,
) -> Result<()> {
    let ep = server::resolve_endpoint(None).context("locating the OpenCode server")?;
    let list = match directory {
        Some(dir) => server::sessions_in(&ep, dir, limit)?,
        None => server::sessions(&ep, limit)?,
    };
    let payload = serde_json::json!(
        list.iter()
            .map(|s| serde_json::json!({
                "id": s.id,
                "title": s.title,
                "directory": s.directory,
                "updated_ms": s.updated_ms,
            }))
            .collect::<Vec<_>>()
    );
    output.emit(&payload, || {
        if list.is_empty() {
            println!("(no sessions)");
            return;
        }
        for s in &list {
            println!(
                "{}  {}  {}  {}",
                schedule::format_ms(s.updated_ms),
                s.id,
                s.title.as_deref().unwrap_or("-"),
                s.directory.as_deref().unwrap_or("-"),
            );
        }
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// daemon
// ---------------------------------------------------------------------------

/// `mdrv-oc daemon` — single-instance, foreground delivery loop.
pub(crate) fn cmd_daemon(once: bool) -> Result<()> {
    let lock_path = schedule::daemon_lock_path()?;
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .read(true)
        .open(&lock_path)?;
    lock.try_lock().map_err(|_| {
        anyhow::anyhow!(
            "another mdrv-oc daemon is already running (lock: {})",
            lock_path.display()
        )
    })?;

    if once {
        return tick_once();
    }

    eprintln!(
        "mdrv-oc daemon watching {} (ctrl-c to stop)",
        Queue::path()?.display()
    );
    loop {
        if let Err(e) = tick_once() {
            // A dead server must not kill the daemon — the queue keeps the
            // jobs and the next tick retries.
            eprintln!("⚠ tick failed: {e:#}");
        }
        std::thread::sleep(Duration::from_secs(20));
    }
}

/// One pass: fire everything due, then persist the outcomes.
fn tick_once() -> Result<()> {
    let now = schedule::now_ms();

    // Phase 1: claim due jobs under the lock.
    let claimed: Vec<Job> = Queue::with_locked(|q| {
        let mut claimed = Vec::new();
        for job in q.jobs.iter_mut() {
            if job.due_for_delivery(now) {
                job.state = JobState::Delivering;
                job.attempts += 1;
                job.last_attempt_ms = Some(now);
                claimed.push(job.clone());
            }
        }
        Ok(claimed)
    })?;
    if claimed.is_empty() {
        return Ok(());
    }

    // Phase 2: deliver outside the lock. Endpoint resolution is per-tick so a
    // server that starts *later* is picked up automatically.
    let mut outcomes: Vec<(String, Option<String>)> = Vec::new(); // (id, error?)
    let ep = match server::resolve_endpoint(None) {
        Ok(ep) => Some(ep),
        Err(e) => {
            eprintln!("⚠ no server reachable yet: {e:#}");
            None
        }
    };
    for job in &claimed {
        let err = match ep.as_ref() {
            None => Some("no OpenCode server reachable".to_owned()),
            Some(ep) => deliver_job(ep, job).err().map(|e| format!("{e:#}")),
        };
        match &err {
            None => eprintln!(
                "✓ delivered {} → {} ({})",
                job.id,
                job.session_title.as_deref().unwrap_or("?"),
                job.session_id
            ),
            Some(e) => eprintln!("✗ {} failed: {e}", job.id),
        }
        outcomes.push((job.id.clone(), err));
    }

    // Phase 3: record outcomes under the lock.
    Queue::with_locked(|q| {
        for (id, err) in &outcomes {
            let Some(job) = q.jobs.iter_mut().find(|j| &j.id == id) else {
                continue;
            };
            match err {
                None => {
                    job.state = JobState::Delivered;
                    job.done_ms = Some(schedule::now_ms());
                    job.last_error = None;
                }
                Some(e) => {
                    job.last_error = Some(e.clone());
                    if job.attempts >= schedule::MAX_ATTEMPTS {
                        job.state = JobState::Failed;
                        job.done_ms = Some(schedule::now_ms());
                    } else {
                        job.state = JobState::Pending; // retried after RETRY_INTERVAL_MS
                    }
                }
            }
        }
        Ok(())
    })?;
    Ok(())
}

/// Deliver one job: optional agent/model switch, then the prompt POST.
fn deliver_job(ep: &Endpoint, job: &Job) -> anyhow::Result<()> {
    if let Some(agent) = &job.agent {
        server::switch_agent(ep, &job.session_id, agent)
            .with_context(|| format!("switching agent to {agent:?}"))?;
    }
    if let Some(m) = &job.model {
        server::switch_model(ep, &job.session_id, &m.provider, &m.id)
            .with_context(|| format!("switching model to {}/{}", m.provider, m.id))?;
    }
    server::prompt(ep, &job.session_id, &job.text).context("prompt delivery failed")
}
