// ===========================================================================
// schedule.rs — the prompt-scheduler queue and its time parsing.
//
// Pending prompts live in one JSON file, `<data>/mdrv-oc/schedule.json`
// (where `<data>` is `$XDG_DATA_HOME` or `~/.local/share`, with the usual
// `%USERPROFILE%` fallback; override with `$MDRV_OC_DATA_DIR`). Every
// mutation — from the daemon *and* from CLI commands — takes an exclusive
// `flock` on a sibling `schedule.lock` around load → mutate → save, so the
// two never clobber each other. Writes are atomic (temp file + rename).
//
// The daemon's own single-instance lock is `daemon.lock` in the same
// directory: a daemon holds it for its whole lifetime, and
// `daemon_running()` probes it non-blockingly.
//
// Missed-fire policy (agreed design): one-shots fire late once — an overdue
// pending job is delivered as soon as the daemon sees it. Delivery failures
// retry every 60 s, up to 10 attempts, then the job is marked `failed` with
// the last error kept for `session prompt list`.
// ===========================================================================

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;

/// How long a failed delivery waits before the next attempt.
pub const RETRY_INTERVAL_MS: i64 = 60_000;
/// Delivery attempts before a job is marked `failed`.
pub const MAX_ATTEMPTS: u32 = 10;

/// The effective retry interval. `$MDRV_OC_RETRY_INTERVAL_MS` overrides it —
/// an internal knob for smoke tests (realistic pacing compresses from
/// minutes to milliseconds); unset in normal use.
pub fn retry_interval_ms() -> i64 {
    std::env::var("MDRV_OC_RETRY_INTERVAL_MS")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| *v >= 0)
        .unwrap_or(RETRY_INTERVAL_MS)
}

// ---------------------------------------------------------------------------
// Jobs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    /// Waiting for its due time (or its next retry).
    Pending,
    /// A POST is in flight. Normalized back to `pending` on load — the only
    /// writer that sets it is the daemon, so a lingering value means the
    /// daemon died mid-POST. (The prompt may have landed; the rare redelivery
    /// beats silently dropping it.)
    Delivering,
    /// Delivered successfully.
    Delivered,
    /// Exhausted `MAX_ATTEMPTS`; `last_error` says why.
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRef {
    pub provider: String,
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    /// When the job was created (epoch ms).
    pub created_ms: i64,
    /// When the prompt should be delivered (epoch ms).
    pub due_ms: i64,
    pub session_id: String,
    #[serde(default)]
    pub session_title: Option<String>,
    pub text: String,
    /// Switch the session to this agent before prompting (agreed: only when
    /// the user asked for it).
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub model: Option<ModelRef>,
    pub state: JobState,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub last_attempt_ms: Option<i64>,
    #[serde(default)]
    pub last_error: Option<String>,
    /// When the job reached a terminal state (epoch ms).
    #[serde(default)]
    pub done_ms: Option<i64>,
}

impl Job {
    /// A job is eligible for delivery when it is due and, after a failure,
    /// its retry interval has elapsed.
    pub fn due_for_delivery(&self, now_ms: i64) -> bool {
        if self.state != JobState::Pending || self.due_ms > now_ms {
            return false;
        }
        match (self.attempts, self.last_attempt_ms) {
            (0, _) | (_, None) => true,
            (_, Some(last)) => now_ms - last >= retry_interval_ms(),
        }
    }

    /// The state as a short lowercase word (`pending`, `delivered`, ...).
    pub fn state_str(&self) -> &'static str {
        match self.state {
            JobState::Pending => "pending",
            JobState::Delivering => "delivering",
            JobState::Delivered => "delivered",
            JobState::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Queue {
    #[serde(default)]
    pub jobs: Vec<Job>,
}

impl Queue {
    // -- persistence --------------------------------------------------------

    pub fn path() -> Result<PathBuf> {
        Ok(data_dir()?.join("schedule.json"))
    }

    pub fn load() -> Result<Queue> {
        Self::load_from(&Self::path()?)
    }

    pub fn load_from(path: &std::path::Path) -> Result<Queue> {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            // No file yet — an empty queue, not an error.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Queue::default()),
            Err(e) => return Err(e.into()),
        };
        let mut q: Queue = serde_json::from_slice(&bytes).map_err(|e| Error::InvalidInput {
            msg: format!("{} is corrupt: {e}", path.display()),
        })?;
        for job in q.jobs.iter_mut() {
            if job.state == JobState::Delivering {
                job.state = JobState::Pending;
            }
        }
        Ok(q)
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::path()?)
    }

    pub fn save_to(&self, path: &std::path::Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        {
            let f = std::fs::File::create(&tmp)?;
            let mut w = std::io::BufWriter::new(f);
            serde_json::to_writer_pretty(&mut w, self).map_err(|e| Error::InvalidInput {
                msg: format!("serializing queue failed: {e}"),
            })?;
            w.write_all(b"\n")?;
            w.flush()?;
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    // -- mutation under the queue lock --------------------------------------

    /// Run `f` with the queue loaded and locked, saving whatever it leaves.
    /// The lock file lives next to the queue; both daemon and CLI go through
    /// this, so concurrent edits serialize.
    pub fn with_locked<T>(f: impl FnOnce(&mut Queue) -> Result<T>) -> Result<T> {
        let lock_path = data_dir()?.join("schedule.lock");
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .read(true)
            .open(&lock_path)?;
        // std file locking (Rust ≥1.89): a blocking exclusive lock serializes
        // daemon and CLI mutations of the queue file.
        file.lock().map_err(|e| Error::InvalidInput {
            msg: format!("could not lock the schedule queue: {e}"),
        })?;
        let out = (|| {
            let mut q = Queue::load()?;
            let out = f(&mut q)?;
            q.prune();
            q.save()?;
            Ok(out)
        })();
        let _ = file.unlock();
        out
    }

    // -- housekeeping -------------------------------------------------------

    /// Keep at most the newest 100 finished jobs, and drop finished jobs
    /// older than 7 days. Pending jobs are never touched.
    pub fn prune(&mut self) {
        let now_ms = now_ms();
        let cutoff = now_ms - 7 * 24 * 3600 * 1000;
        self.jobs.retain(|j| {
            j.state != JobState::Delivered && j.state != JobState::Failed
                || j.done_ms.unwrap_or(now_ms) >= cutoff
        });
        let mut finished: Vec<usize> = self
            .jobs
            .iter()
            .enumerate()
            .filter(|(_, j)| j.state == JobState::Delivered || j.state == JobState::Failed)
            .map(|(i, _)| i)
            .collect();
        if finished.len() > 100 {
            finished.sort_by_key(|&i| self.jobs[i].done_ms.unwrap_or(0));
            let drop: Vec<usize> = finished[..finished.len() - 100].to_vec();
            let drop_set: std::collections::HashSet<usize> = drop.into_iter().collect();
            let mut i = 0usize;
            self.jobs.retain(|_| {
                let keep = !drop_set.contains(&i);
                i += 1;
                keep
            });
        }
    }

    /// Allocate a fresh job id (`sch_xxxxxxxx`).
    pub fn new_job_id(&self) -> String {
        loop {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            let mut h = nanos ^ ((std::process::id() as u64) << 48);
            h ^= h >> 33;
            h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
            h ^= h >> 33;
            let id = format!("sch_{h:08x}");
            if self.jobs.iter().all(|j| j.id != id) {
                return id;
            }
        }
    }
}

/// `<data>/mdrv-oc`, mirroring how the DB path resolves (`$XDG_DATA_HOME`
/// absolute, else `$HOME/.local/share`, else `%USERPROFILE%`), with a test /
/// sandbox escape hatch via `$MDRV_OC_DATA_DIR`.
pub fn data_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("MDRV_OC_DATA_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .filter(|s| PathBuf::from(s).is_absolute())
        .map(PathBuf::from)
        .or_else(|| {
            crate::pathutil::home_dir()
                .map(PathBuf::from)
                .map(|h| h.join(".local/share"))
        })
        .ok_or(Error::DbNotFound {
            detail: "neither $XDG_DATA_HOME nor $HOME/$USERPROFILE is set".into(),
        })?;
    Ok(base.join("mdrv-oc"))
}

/// Path of the daemon's single-instance lock.
pub fn daemon_lock_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("daemon.lock"))
}

/// True when a daemon currently holds the single-instance lock. A probe that
/// itself fails (missing dir, permissions) counts as "not running" — callers
/// use this only to warn.
pub fn daemon_running() -> bool {
    let path = match daemon_lock_path() {
        Ok(p) => p,
        Err(_) => return false,
    };
    let file = match std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .read(true)
        .open(&path)
    {
        Ok(f) => f,
        Err(_) => return false,
    };
    match file.try_lock() {
        Ok(()) => {
            let _ = file.unlock();
            false
        }
        Err(_) => true,
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Time parsing (`--at` / `--in`), all in the machine's local timezone
// ---------------------------------------------------------------------------

/// Parse the `--at` / `--in` pair (exactly one must be set) into a due time
/// in epoch milliseconds.
///
/// `--at` accepts RFC3339 (`2026-11-03T07:00:00+07:00`), `YYYY-MM-DD HH:MM`
/// (`T` also works as the separator, seconds optional), bare `HH:MM` (today
/// if still in the future, else tomorrow), and `tomorrow HH:MM`.
/// `--in` accepts humantime durations: `90s`, `45m`, `2h`, `1d`, `1h30m`.
pub fn parse_due(at: Option<&str>, in_: Option<&str>, now: &jiff::Zoned) -> Result<i64> {
    match (at, in_) {
        (Some(_), Some(_)) => Err(Error::InvalidInput {
            msg: "--at and --in are mutually exclusive".into(),
        }),
        (None, None) => Err(Error::InvalidInput {
            msg: "nothing to schedule: pass --at or --in to schedule, or neither to send now"
                .into(),
        }),
        (Some(at), None) => parse_at(at, now),
        (None, Some(dur)) => parse_in(dur, now),
    }
}

fn parse_at(at: &str, now: &jiff::Zoned) -> Result<i64> {
    let at = at.trim();
    let tz = now.time_zone().clone();

    // 1. RFC3339 / ISO-8601 timestamp with offset.
    if let Ok(ts) = at.parse::<jiff::Timestamp>() {
        return Ok(ts.to_zoned(tz).timestamp().as_millisecond());
    }

    // 2. `tomorrow HH:MM[:SS]`
    if let Some(rest) = at.strip_prefix("tomorrow ") {
        let time = parse_hms(rest, at)?;
        let z = now
            .tomorrow()
            .map_err(|e| bad_when(at, e))?
            .with()
            .time(time)
            .build()
            .map_err(|e| bad_when(at, e))?;
        return Ok(z.timestamp().as_millisecond());
    }

    // 3. `YYYY-MM-DD HH:MM[:SS]` (space or `T` separator) — ISO-8601 civil
    //    datetime after swapping the separator.
    if at.len() >= 11
        && (at.as_bytes().get(10) == Some(&b' ') || at.as_bytes().get(10) == Some(&b'T'))
    {
        let iso = at.replacen(' ', "T", 1);
        let civil: jiff::civil::DateTime = iso.parse().map_err(|e| bad_when(at, e))?;
        let z = civil.to_zoned(tz).map_err(|e| bad_when(at, e))?;
        return Ok(z.timestamp().as_millisecond());
    }

    // 4. bare `HH:MM[:SS]` — today if still future, else tomorrow.
    let time = parse_hms(at, at)?;
    let mut z = now.with().time(time).build().map_err(|e| bad_when(at, e))?;
    if z <= *now {
        z = z
            .checked_add(jiff::Span::new().days(1))
            .map_err(|e| bad_when(at, e))?;
    }
    Ok(z.timestamp().as_millisecond())
}

/// Parse `HH:MM[:SS]` into a civil time (manual split — three integers don't
/// need `BrokenDownTime`'s roundabout builder).
fn parse_hms(s: &str, original: &str) -> Result<jiff::civil::Time> {
    let bad = || bad_when(original, format!("expected HH:MM, got {s:?}"));
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() < 2 || parts.len() > 3 {
        return Err(bad());
    }
    let h: i8 = parts[0].trim().parse().map_err(|_| bad())?;
    let m: i8 = parts[1].trim().parse().map_err(|_| bad())?;
    let sec: i8 = if parts.len() == 3 {
        parts[2].trim().parse().map_err(|_| bad())?
    } else {
        0
    };
    // `civil::time` panics on out-of-range values, so validate explicitly.
    if !(0..=23).contains(&h) || !(0..=59).contains(&m) || !(0..=60).contains(&sec) {
        return Err(bad());
    }
    Ok(jiff::civil::time(h, m, sec, 0))
}

fn parse_in(dur: &str, now: &jiff::Zoned) -> Result<i64> {
    let dur = dur.trim();
    let mut span = jiff::Span::new();
    let mut total_secs: i64 = 0;
    let mut rest = dur;
    let mut matched_any = false;

    while !rest.is_empty() {
        rest = rest.trim_start();
        let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
        if digits == 0 {
            return Err(bad_in(dur));
        }
        let (num, tail) = rest.split_at(digits);
        let value: i32 = num.parse().map_err(|_| bad_in(dur))?;
        let unit_len = tail.chars().take_while(|c| c.is_ascii_alphabetic()).count();
        let (unit, tail) = tail.split_at(unit_len);
        let unit = match unit {
            "s" | "sec" | "secs" | "second" | "seconds" => "seconds",
            "m" | "min" | "mins" | "minute" | "minutes" => "minutes",
            "h" | "hr" | "hrs" | "hour" | "hours" => "hours",
            "d" | "day" | "days" => "days",
            _ => return Err(bad_in(dur)),
        };
        match unit {
            "seconds" => {
                span = span.seconds(span.get_seconds() + i64::from(value));
                total_secs += i64::from(value);
            }
            "minutes" => {
                span = span.minutes(span.get_minutes() + i64::from(value));
                total_secs += i64::from(value) * 60;
            }
            "hours" => {
                span = span.hours(span.get_hours() + value);
                total_secs += i64::from(value) * 3600;
            }
            "days" => {
                span = span.days(span.get_days() + value);
                total_secs += i64::from(value) * 86400;
            }
            _ => unreachable!(),
        }
        rest = tail;
        matched_any = true;
    }
    if !matched_any || total_secs == 0 {
        return Err(bad_in(dur));
    }
    if total_secs > 366 * 86400 {
        return Err(Error::InvalidInput {
            msg: format!("--in {dur}: durations longer than a year make little sense"),
        });
    }
    let due = now.checked_add(span).map_err(|e| Error::InvalidInput {
        msg: format!("--in {dur}: {e}"),
    })?;
    Ok(due.timestamp().as_millisecond())
}

fn bad_when(input: &str, e: impl std::fmt::Display) -> Error {
    Error::InvalidInput {
        msg: format!(
            "cannot parse --at {input:?} ({e}). Accepted: RFC3339 (2026-11-03T07:00:00+07:00), \
             \"YYYY-MM-DD HH:MM\", \"HH:MM\" (today or tomorrow), \"tomorrow HH:MM\""
        ),
    }
}

fn bad_in(input: &str) -> Error {
    Error::InvalidInput {
        msg: format!(
            "cannot parse --in {input:?}. Accepted: 90s, 45m, 2h, 1d, or compounds like 1h30m"
        ),
    }
}

/// Render an epoch-ms timestamp as a local `YYYY-MM-DD HH:MM` string.
pub fn format_ms(ms: i64) -> String {
    match jiff::Timestamp::from_millisecond(ms) {
        Ok(ts) => ts
            .to_zoned(jiff::Zoned::now().time_zone().clone())
            .strftime("%Y-%m-%d %H:%M")
            .to_string(),
        Err(_) => ms.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed reference point: 2026-11-03 10:00 local time.
    fn fixed_now() -> jiff::Zoned {
        let iso = "2026-11-03T10:00";
        iso.parse::<jiff::civil::DateTime>()
            .unwrap()
            .to_zoned(jiff::Zoned::now().time_zone().clone())
            .unwrap()
    }

    #[test]
    fn in_parses_units_and_compounds() {
        let now = fixed_now();
        let due = parse_in("45m", &now).unwrap();
        assert_eq!(due - now.timestamp().as_millisecond(), 45 * 60 * 1000);
        let due = parse_in("1h30m", &now).unwrap();
        assert_eq!(due - now.timestamp().as_millisecond(), 90 * 60 * 1000);
        let due = parse_in("90s", &now).unwrap();
        assert_eq!(due - now.timestamp().as_millisecond(), 90 * 1000);
        let due = parse_in("2d", &now).unwrap();
        assert_eq!(due - now.timestamp().as_millisecond(), 2 * 86400 * 1000);
    }

    #[test]
    fn in_rejects_garbage() {
        let now = fixed_now();
        for bad in ["", "m", "1x", "h3", "0m", "-5m"] {
            assert!(parse_in(bad, &now).is_err(), "expected {bad:?} to fail");
        }
    }

    #[test]
    fn at_parses_absolute_datetime() {
        let now = fixed_now();
        let due = parse_at("2026-11-03 12:30", &now).unwrap();
        let parsed: jiff::Zoned = "2026-11-03T12:30"
            .parse::<jiff::civil::DateTime>()
            .unwrap()
            .to_zoned(now.time_zone().clone())
            .unwrap();
        assert_eq!(due, parsed.timestamp().as_millisecond());

        // RFC3339 with an explicit offset wins over local time.
        let due = parse_at("2026-11-03T12:30:00+07:00", &now).unwrap();
        let ts: jiff::Timestamp = "2026-11-03T12:30:00+07:00".parse().unwrap();
        assert_eq!(due, ts.as_millisecond());
    }

    #[test]
    fn at_bare_time_rolls_to_tomorrow_when_past() {
        let now = fixed_now(); // 10:00
        let due_today = parse_at("11:00", &now).unwrap();
        let due_tomorrow = parse_at("09:00", &now).unwrap();
        assert!(due_today > now.timestamp().as_millisecond());
        assert!(due_today - now.timestamp().as_millisecond() < 86400 * 1000);
        let diff = due_tomorrow - now.timestamp().as_millisecond();
        assert!((23 * 3600 * 1000..=24 * 3600 * 1000).contains(&diff));
    }

    #[test]
    fn at_tomorrow_prefix() {
        let now = fixed_now();
        let due = parse_at("tomorrow 07:00", &now).unwrap();
        // tomorrow 07:00 minus today 10:00 is 21h, within (20h, 22h].
        let diff = due - now.timestamp().as_millisecond();
        assert!(diff > 20 * 3600 * 1000 && diff <= 22 * 3600 * 1000);
    }

    #[test]
    fn parse_due_mutual_exclusion() {
        let now = fixed_now();
        assert!(parse_due(Some("11:00"), Some("5m"), &now).is_err());
        assert!(parse_due(None, None, &now).is_err());
        assert!(parse_due(None, Some("5m"), &now).is_ok());
    }

    fn job(id: &str, state: JobState, due: i64, attempts: u32, last: Option<i64>) -> Job {
        Job {
            id: id.into(),
            created_ms: 0,
            due_ms: due,
            session_id: "ses_x".into(),
            session_title: None,
            text: "hi".into(),
            agent: None,
            model: None,
            state,
            attempts,
            last_attempt_ms: last,
            last_error: None,
            done_ms: None,
        }
    }

    #[test]
    fn eligibility_respects_retry_pacing() {
        let now = 1_000_000;
        assert!(job("a", JobState::Pending, now - 1, 0, None).due_for_delivery(now));
        assert!(!job("b", JobState::Pending, now + 1, 0, None).due_for_delivery(now));
        // Recent failure → wait for the retry interval.
        assert!(!job("c", JobState::Pending, now - 1, 1, Some(now - 1000)).due_for_delivery(now));
        assert!(
            job(
                "d",
                JobState::Pending,
                now - 1,
                1,
                Some(now - RETRY_INTERVAL_MS)
            )
            .due_for_delivery(now)
        );
        // Terminal states are never delivered.
        assert!(!job("e", JobState::Failed, now - 1, 10, None).due_for_delivery(now));
        assert!(!job("f", JobState::Delivered, now - 1, 0, None).due_for_delivery(now));
    }

    #[test]
    fn queue_roundtrips_and_normalizes_stale_delivering() {
        let dir = std::env::temp_dir().join("mdrv-oc-test-schedule");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("schedule.json");
        let _ = std::fs::remove_file(&path);

        let mut q = Queue::load_from(&path).unwrap();
        assert!(q.jobs.is_empty());
        q.jobs
            .push(job("sch_1", JobState::Delivering, 1, 2, Some(3)));
        q.jobs[0].session_title = Some("t".into());
        q.save_to(&path).unwrap();

        let q = Queue::load_from(&path).unwrap();
        assert_eq!(q.jobs[0].state, JobState::Pending); // stale → recovered
        assert_eq!(q.jobs[0].attempts, 2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn prune_keeps_recent_finished_and_all_pending() {
        let mut q = Queue::default();
        let now = now_ms();
        // 120 old failures → should collapse to 100 newest; plus one pending ancient job kept.
        for i in 0..120 {
            let mut j = job(&format!("f{i}"), JobState::Failed, now, 10, None);
            j.done_ms = Some(now - (i as i64) * 1000);
            q.jobs.push(j);
        }
        let mut p = job("p", JobState::Pending, 0, 0, None);
        p.done_ms = Some(now - 30 * 24 * 3600 * 1000);
        q.jobs.push(p);
        q.prune();
        assert_eq!(
            q.jobs
                .iter()
                .filter(|j| j.state == JobState::Failed)
                .count(),
            100
        );
        assert!(q.jobs.iter().any(|j| j.state == JobState::Pending));

        // Finished jobs older than 7 days are dropped entirely.
        let mut q2 = Queue::default();
        let mut old = job("old", JobState::Delivered, now, 0, None);
        old.done_ms = Some(now - 8 * 24 * 3600 * 1000);
        q2.jobs.push(old);
        q2.prune();
        assert!(q2.jobs.is_empty());
    }

    #[test]
    fn job_ids_are_unique() {
        let q = Queue::default();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            let id = q.new_job_id();
            assert!(id.starts_with("sch_"));
            assert!(seen.insert(id), "id collision");
        }
    }
}
