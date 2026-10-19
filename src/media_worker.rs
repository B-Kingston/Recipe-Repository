//! Persistent Python workers for local speech-to-text and OCR.
//!
//! Importing Whisper/PaddleOCR, building their models, and starting the
//! interpreter cost seconds per import, so `pi/local-media.py serve` stays
//! alive between imports and this module keeps one worker per kind
//! (transcribe, OCR) in a process-wide slot. A kind serialises its own
//! requests; the two kinds run side by side, which lets the pipeline
//! transcribe audio while it reads frames.
//!
//! Protocol (JSON lines on the worker's stdin/stdout): the client writes
//! `{"id": N, "command": ..., ...}` and reads one line back,
//! `{"id": N, "ok": true, "result": {...}}` or
//! `{"id": N, "ok": false, "error": "..."}`. An error response leaves the
//! worker usable. Anything else — a timeout, an unreadable, oversized, or
//! mismatched line, an exit — kills the worker's process group and drops it,
//! because the stream can no longer be trusted. The next request starts a
//! fresh worker; a worker that had already served a request and then died
//! (idle exit, crash) is replaced once inside the same request.

use crate::media::{COMMAND_OUTPUT_MAX_BYTES, env_path, kill_process_group};
use parking_lot::Mutex;
use serde_json::Value;
use std::{
    collections::VecDeque,
    io::{self, BufRead, BufReader, Write},
    os::unix::process::CommandExt,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, LazyLock,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use tracing::{debug, info, warn};

/// Longest stderr line kept for the log and for error messages.
const STDERR_LINE_MAX_BYTES: usize = 2 * 1024;
/// Recent stderr lines remembered to explain a dead worker.
const STDERR_TAIL_LINES: usize = 12;
/// Characters of that tail quoted in an error message.
const STDERR_TAIL_MAX_CHARS: usize = 500;
/// How long the stdout reader waits for an exited worker to be reapable.
const REAP_POLLS: u32 = 50;
const REAP_POLL_INTERVAL: Duration = Duration::from_millis(100);

static TRANSCRIBE: LazyLock<Client> = LazyLock::new(|| {
    Client::new(
        "transcription",
        Box::new(|| worker_command("MEDIA_TRANSCRIBE_SCRIPT")),
        COMMAND_OUTPUT_MAX_BYTES,
    )
});
static OCR: LazyLock<Client> = LazyLock::new(|| {
    Client::new(
        "OCR",
        Box::new(|| worker_command("MEDIA_OCR_SCRIPT")),
        COMMAND_OUTPUT_MAX_BYTES,
    )
});

/// Sends one `transcribe` request (the object must carry `"command"`) to the
/// transcription worker and returns its `result`.
pub(crate) fn transcribe(request: Value, timeout: Duration) -> Result<Value, String> {
    TRANSCRIBE.request(request, timeout)
}

/// Sends one `ocr` request to the OCR worker and returns its `result`.
pub(crate) fn ocr(request: Value, timeout: Duration) -> Result<Value, String> {
    OCR.request(request, timeout)
}

fn worker_command(script_env: &str) -> Command {
    let mut command = Command::new(env_path("MEDIA_PYTHON", "python3"));
    command
        .arg(env_path(script_env, "pi/local-media.py"))
        .arg("serve");
    command
}

/// One worker kind: how to start it, the response size bound, and the live
/// process (if any). The slot's lock is held for a whole request, which is
/// what serialises requests of one kind.
struct Client {
    label: &'static str,
    spawn: Box<dyn Fn() -> Command + Send + Sync>,
    max_response_bytes: usize,
    slot: Mutex<Option<Worker>>,
    next_id: AtomicU64,
}

enum Event {
    Line(Vec<u8>),
    /// The worker wrote a response line longer than the bound.
    TooLarge,
    /// stdout closed: the worker exited or was killed.
    Closed,
}

struct Worker {
    /// Shared with the stdout reader, which reaps a worker that exits on its
    /// own (idle timeout, crash) so it does not stay a zombie.
    child: Arc<Mutex<Child>>,
    stdin: ChildStdin,
    events: mpsc::Receiver<Event>,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
    stderr_thread: Option<JoinHandle<()>>,
    /// Requests this process has answered. A worker with `served > 0` that
    /// dies is presumed to have gone idle or crashed, not to fail
    /// deterministically, so one replacement is tried.
    served: u32,
}

/// How an exchange with a worker failed.
enum Failure {
    /// The worker is gone (write failed or stdout closed before a response).
    Died,
    /// The worker is unusable or too slow; the message says why.
    Broken(String),
}

impl Client {
    fn new(
        label: &'static str,
        spawn: Box<dyn Fn() -> Command + Send + Sync>,
        max_response_bytes: usize,
    ) -> Self {
        Self {
            label,
            spawn,
            max_response_bytes,
            slot: Mutex::new(None),
            next_id: AtomicU64::new(1),
        }
    }

    /// Runs one request within `timeout` (waiting for the previous request of
    /// this kind, starting the worker, and the response all count).
    fn request(&self, request: Value, timeout: Duration) -> Result<Value, String> {
        let started = Instant::now();
        let deadline = started + timeout;
        let Some(mut slot) = self.slot.try_lock_for(timeout) else {
            return Err(format!(
                "the previous {} request did not finish within {} seconds",
                self.label,
                timeout.as_secs()
            ));
        };
        let Value::Object(mut body) = request else {
            return Err(format!("a {} request must be a JSON object", self.label));
        };
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        body.insert("id".into(), id.into());
        let mut line = serde_json::to_vec(&Value::Object(body))
            .map_err(|error| format!("could not encode the {} request: {error}", self.label))?;
        line.push(b'\n');

        let mut replaced = false;
        loop {
            let mut worker = match slot
                .take()
                .and_then(|mut worker| worker.alive().then_some(worker))
            {
                Some(worker) => worker,
                None => self.start()?,
            };
            let warm = worker.served > 0;
            let outcome = worker.exchange(&line, id, deadline, self.max_response_bytes, self.label);
            match outcome {
                Ok(reply) => {
                    worker.served += 1;
                    info!(
                        worker = self.label,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        reused = warm,
                        "media worker request finished"
                    );
                    *slot = Some(worker);
                    return reply;
                }
                Err(Failure::Died) if warm && !replaced => {
                    replaced = true;
                    let detail = worker.finish();
                    warn!(
                        worker = self.label,
                        detail, "media worker had exited; starting a replacement"
                    );
                }
                Err(Failure::Died) => {
                    let detail = worker.finish();
                    return Err(if detail.is_empty() {
                        format!("the {} worker exited before answering", self.label)
                    } else {
                        format!(
                            "the {} worker exited before answering: {detail}",
                            self.label
                        )
                    });
                }
                Err(Failure::Broken(message)) => {
                    worker.finish();
                    return Err(message);
                }
            }
        }
    }

    fn start(&self) -> Result<Worker, String> {
        let mut command = (self.spawn)();
        // A worker must be killable with everything it spawned, and it does
        // not need the AI Gateway secret the app environment carries.
        command.process_group(0);
        command.env_remove("AI_GATEWAY_API_KEY");
        command.env("PYTHONUNBUFFERED", "1");
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| {
                format!(
                    "could not start the {} worker {:?}: {error}",
                    self.label,
                    command.get_program()
                )
            })?;
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            kill_process_group(&mut child);
            let _ = child.wait();
            return Err(format!(
                "the {} worker pipes were not available",
                self.label
            ));
        };
        let pid = child.id();
        let child = Arc::new(Mutex::new(child));
        let (sender, events) = mpsc::channel();
        let max_response_bytes = self.max_response_bytes;
        let reaper = Arc::clone(&child);
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = Vec::new();
            loop {
                line.clear();
                let event = match read_line(&mut reader, max_response_bytes, &mut line, true) {
                    Ok(Some(false)) => Event::Line(std::mem::take(&mut line)),
                    Ok(Some(true)) => Event::TooLarge,
                    Ok(None) | Err(_) => Event::Closed,
                };
                let last = !matches!(event, Event::Line(_));
                if sender.send(event).is_err() || last {
                    break;
                }
            }
            // stdout ends when the worker exits. Reap it here, polling so the
            // lock is never held while a request thread wants to kill it.
            for _ in 0..REAP_POLLS {
                if !matches!(reaper.lock().try_wait(), Ok(None)) {
                    break;
                }
                thread::sleep(REAP_POLL_INTERVAL);
            }
        });
        let stderr_tail = Arc::new(Mutex::new(VecDeque::new()));
        let stderr_thread = {
            let tail = Arc::clone(&stderr_tail);
            let label = self.label;
            thread::spawn(move || forward_stderr(stderr, label, &tail))
        };
        debug!(worker = self.label, pid, "media worker started");
        Ok(Worker {
            child,
            stdin,
            events,
            stderr_tail,
            stderr_thread: Some(stderr_thread),
            served: 0,
        })
    }
}

impl Worker {
    /// False once the worker process has exited (idle timeout, crash).
    fn alive(&mut self) -> bool {
        matches!(self.child.lock().try_wait(), Ok(None))
    }

    /// Writes the request and waits for its response line until `deadline`.
    /// The outer result is the worker's health, the inner one the answer.
    fn exchange(
        &mut self,
        line: &[u8],
        id: u64,
        deadline: Instant,
        max_response_bytes: usize,
        label: &str,
    ) -> Result<Result<Value, String>, Failure> {
        if self
            .stdin
            .write_all(line)
            .and_then(|()| self.stdin.flush())
            .is_err()
        {
            return Err(Failure::Died);
        }
        let wait = deadline.saturating_duration_since(Instant::now());
        let bytes = match self.events.recv_timeout(wait) {
            Ok(Event::Line(bytes)) => bytes,
            Ok(Event::Closed) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(Failure::Died);
            }
            Ok(Event::TooLarge) => {
                return Err(Failure::Broken(format!(
                    "the {label} worker response exceeded {max_response_bytes} bytes"
                )));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err(Failure::Broken(format!(
                    "the {label} worker timed out and was stopped"
                )));
            }
        };
        let unreadable = |reason: String| {
            Failure::Broken(format!(
                "the {label} worker returned an unreadable response: {reason}"
            ))
        };
        let response: Value =
            serde_json::from_slice(&bytes).map_err(|error| unreadable(error.to_string()))?;
        let Some(response) = response.as_object() else {
            return Err(unreadable("not a JSON object".into()));
        };
        if response.get("id").and_then(Value::as_u64) != Some(id) {
            return Err(unreadable(
                "the response id does not match the request".into(),
            ));
        }
        match response.get("ok").and_then(Value::as_bool) {
            Some(true) => match response.get("result") {
                Some(result) => Ok(Ok(result.clone())),
                None => Err(unreadable("an ok response has no result".into())),
            },
            Some(false) => Ok(Err(response
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("the worker reported an unspecified error")
                .to_string())),
            None => Err(unreadable("the response has no ok flag".into())),
        }
    }

    /// Stops the worker's whole process group, reaps it, and returns the tail
    /// of its stderr (the explanation of a crash).
    fn finish(mut self) -> String {
        self.terminate();
        if let Some(thread) = self.stderr_thread.take() {
            let _ = thread.join();
        }
        let tail = self.stderr_tail.lock();
        let joined = tail
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(" | ");
        let mut chars = joined.chars();
        let kept: String = chars.by_ref().take(STDERR_TAIL_MAX_CHARS).collect();
        if chars.next().is_some() {
            format!("{kept}…")
        } else {
            kept
        }
    }

    fn terminate(&mut self) {
        let mut child = self.child.lock();
        // A leader that already exited was reaped by `try_wait` and its pid
        // may be reused, so only a still-running leader gets the group kill.
        if matches!(child.try_wait(), Ok(None)) {
            kill_process_group(&mut child);
        }
        let _ = child.wait();
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.terminate();
    }
}

/// Reads one `\n`-terminated line into `line` (without the newline).
/// `Ok(None)` is end of stream with nothing pending. `Ok(Some(true))` means
/// the line outgrew `cap`: with `strict` the read stops there (the rest of
/// the stream is abandoned), otherwise the excess is discarded up to the next
/// newline and the kept prefix is returned.
fn read_line<R: BufRead>(
    reader: &mut R,
    cap: usize,
    line: &mut Vec<u8>,
    strict: bool,
) -> io::Result<Option<bool>> {
    let mut overflowed = false;
    loop {
        let chunk = reader.fill_buf()?;
        if chunk.is_empty() {
            // A partial final line from a dying worker is not a response, so
            // a strict stream ends here; a log line is kept as far as it got.
            let pending = !strict && (overflowed || !line.is_empty());
            return Ok(pending.then_some(overflowed));
        }
        let (take, complete) = match chunk.iter().position(|byte| *byte == b'\n') {
            Some(position) => (position + 1, true),
            None => (chunk.len(), false),
        };
        let body = &chunk[..if complete { take - 1 } else { take }];
        let room = cap.saturating_sub(line.len());
        if body.len() > room {
            overflowed = true;
            if strict {
                return Ok(Some(true));
            }
            line.extend_from_slice(&body[..room]);
        } else {
            line.extend_from_slice(body);
        }
        reader.consume(take);
        if complete {
            return Ok(Some(overflowed));
        }
    }
}

/// Drains the worker's stderr for as long as it lives: lines go to the log
/// (a worker-reported failure at warn, library chatter at debug) and the most
/// recent few are kept to explain a crash.
fn forward_stderr<R: io::Read>(stderr: R, label: &'static str, tail: &Mutex<VecDeque<String>>) {
    let mut reader = BufReader::new(stderr);
    let mut line = Vec::new();
    loop {
        line.clear();
        match read_line(&mut reader, STDERR_LINE_MAX_BYTES, &mut line, false) {
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
        let text = String::from_utf8_lossy(&line);
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        if text.starts_with("request failed:") || text.starts_with("{\"error\"") {
            warn!(worker = label, "{text}");
        } else {
            debug!(worker = label, "{text}");
        }
        let mut tail = tail.lock();
        if tail.len() == STDERR_TAIL_LINES {
            tail.pop_front();
        }
        tail.push_back(text.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{env, fs, path::PathBuf};
    use uuid::Uuid;

    fn client(script: &str, max_response_bytes: usize) -> Client {
        let script = script.to_string();
        Client::new(
            "test",
            Box::new(move || {
                let mut command = Command::new("sh");
                command.arg("-c").arg(&script);
                command
            }),
            max_response_bytes,
        )
    }

    /// A worker that answers every request with its own pid.
    const ECHO_PID: &str = r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  printf '{"id":%s,"ok":true,"result":{"pid":%s}}\n' "$id" "$$"
done
"#;

    fn pid_of(reply: &Value) -> u64 {
        reply["pid"].as_u64().expect("reply carries a pid")
    }

    fn marker(name: &str) -> PathBuf {
        env::temp_dir().join(format!("kr-worker-{name}-{}", Uuid::new_v4()))
    }

    fn process_exists(pid: u64) -> bool {
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    const LONG: Duration = Duration::from_secs(20);

    #[test]
    fn keeps_one_worker_across_requests() {
        let client = client(ECHO_PID, 1024);
        let first = client.request(json!({"command": "ping"}), LONG).unwrap();
        let second = client.request(json!({"command": "ping"}), LONG).unwrap();
        assert_eq!(pid_of(&first), pid_of(&second));
    }

    #[test]
    fn replaces_a_worker_that_exited_after_serving() {
        // Answers exactly one request, then exits like an idle timeout would.
        let client = client(
            r#"
IFS= read -r line
id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
printf '{"id":%s,"ok":true,"result":{"pid":%s}}\n' "$id" "$$"
"#,
            1024,
        );
        let first = client.request(json!({"command": "ping"}), LONG).unwrap();
        let second = client.request(json!({"command": "ping"}), LONG).unwrap();
        assert_ne!(pid_of(&first), pid_of(&second));
        let third = client.request(json!({"command": "ping"}), LONG).unwrap();
        assert_ne!(pid_of(&second), pid_of(&third));
    }

    #[test]
    fn a_worker_that_exits_on_its_own_is_reaped_without_another_request() {
        // Answers one request, then exits like an idle timeout would. Without
        // reaping it stays a zombie, and `kill(pid, 0)` still finds it.
        let client = client(
            r#"
IFS= read -r line
id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
printf '{"id":%s,"ok":true,"result":{"pid":%s}}\n' "$id" "$$"
"#,
            1024,
        );
        let pid = pid_of(&client.request(json!({"command": "ping"}), LONG).unwrap());
        let deadline = Instant::now() + Duration::from_secs(5);
        while process_exists(pid) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
        }
        assert!(!process_exists(pid), "worker {pid} was left unreaped");
    }

    #[test]
    fn a_worker_that_dies_on_its_first_request_is_an_error_with_its_stderr() {
        let client = client("echo 'import exploded' >&2; exit 3", 1024);
        let error = client
            .request(json!({"command": "ping"}), LONG)
            .unwrap_err();
        assert!(error.contains("exited before answering"), "{error}");
        assert!(error.contains("import exploded"), "{error}");
    }

    #[test]
    fn timeout_kills_the_worker_and_its_children_and_the_next_request_starts_fresh() {
        let pid_file = marker("timeout");
        // `sleep` is a child of the shell; both must die with the group.
        let script = format!(
            r#"
echo $$ > '{path}'
IFS= read -r line
sleep 30 &
wait
"#,
            path = pid_file.display()
        );
        let client = client(&script, 1024);
        let started = Instant::now();
        let error = client
            .request(json!({"command": "ping"}), Duration::from_millis(600))
            .unwrap_err();
        assert!(error.contains("timed out"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(10));
        let pid: u64 = fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let _ = fs::remove_file(&pid_file);
        assert!(!process_exists(pid), "the worker survived its timeout");
        assert!(client.slot.lock().is_none(), "a timed-out worker was kept");
    }

    #[test]
    fn rejects_malformed_responses_and_drops_the_worker() {
        for (name, script) in [
            ("not json", "IFS= read -r line; echo 'hello there'; sleep 5"),
            ("not an object", "IFS= read -r line; echo '[1,2]'; sleep 5"),
            (
                "wrong id",
                r#"IFS= read -r line; echo '{"id":999999,"ok":true,"result":{}}'; sleep 5"#,
            ),
            (
                "no result",
                r#"
IFS= read -r line
id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
printf '{"id":%s,"ok":true}\n' "$id"
sleep 5
"#,
            ),
        ] {
            let client = client(script, 1024);
            let error = client
                .request(json!({"command": "ping"}), LONG)
                .unwrap_err();
            assert!(error.contains("unreadable response"), "{name}: {error}");
            assert!(client.slot.lock().is_none(), "{name}: worker kept");
        }
    }

    #[test]
    fn bounds_the_response_size() {
        let client = client(r#"IFS= read -r line; printf '%0300d\n' 0; sleep 5"#, 128);
        let started = Instant::now();
        let error = client
            .request(json!({"command": "ping"}), LONG)
            .unwrap_err();
        assert!(error.contains("exceeded 128 bytes"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn an_error_response_is_an_error_but_keeps_the_worker() {
        let client = client(
            r#"
n=0
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  n=$((n+1))
  if [ "$n" = 1 ]; then
    printf '{"id":%s,"ok":false,"error":"frame missing"}\n' "$id"
  else
    printf '{"id":%s,"ok":true,"result":{"pid":%s}}\n' "$id" "$$"
  fi
done
"#,
            1024,
        );
        let error = client.request(json!({"command": "ocr"}), LONG).unwrap_err();
        assert_eq!(error, "frame missing");
        assert!(client.slot.lock().is_some());
        let reply = client.request(json!({"command": "ocr"}), LONG).unwrap();
        assert!(pid_of(&reply) > 0);
    }

    #[test]
    fn requests_of_one_kind_are_serialised_and_never_cross_talk() {
        let client = Arc::new(client(ECHO_PID, 1024));
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let client = Arc::clone(&client);
                thread::spawn(move || {
                    (0..3)
                        .map(|_| pid_of(&client.request(json!({"command": "ping"}), LONG).unwrap()))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let pids: Vec<u64> = handles
            .into_iter()
            .flat_map(|handle| handle.join().unwrap())
            .collect();
        assert!(pids.iter().all(|pid| *pid == pids[0]), "{pids:?}");
    }

    #[test]
    fn two_kinds_can_run_a_request_at_the_same_time() {
        // Each stub answers only after the other kind's stub has started its
        // request, so a shared lock between kinds would make both time out.
        let (a, b) = (marker("a"), marker("b"));
        let stub = |mine: &PathBuf, theirs: &PathBuf| {
            format!(
                r#"
IFS= read -r line
id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
touch '{mine}'
i=0
while [ ! -e '{theirs}' ] && [ $i -lt 100 ]; do sleep 0.05; i=$((i+1)); done
if [ -e '{theirs}' ]; then
  printf '{{"id":%s,"ok":true,"result":{{"pid":%s}}}}\n' "$id" "$$"
else
  printf '{{"id":%s,"ok":false,"error":"peer never ran"}}\n' "$id"
fi
"#,
                mine = mine.display(),
                theirs = theirs.display()
            )
        };
        let transcription = Arc::new(client(&stub(&a, &b), 1024));
        let ocr = Arc::new(client(&stub(&b, &a), 1024));
        let first = {
            let client = Arc::clone(&transcription);
            thread::spawn(move || client.request(json!({"command": "transcribe"}), LONG))
        };
        let second = {
            let client = Arc::clone(&ocr);
            thread::spawn(move || client.request(json!({"command": "ocr"}), LONG))
        };
        let results = (first.join().unwrap(), second.join().unwrap());
        let _ = (fs::remove_file(&a), fs::remove_file(&b));
        assert!(results.0.is_ok(), "{results:?}");
        assert!(results.1.is_ok(), "{results:?}");
    }

    #[test]
    fn a_missing_interpreter_is_an_error() {
        let client = Client::new(
            "test",
            Box::new(|| Command::new("/nonexistent/python")),
            1024,
        );
        let error = client
            .request(json!({"command": "ping"}), LONG)
            .unwrap_err();
        assert!(error.contains("could not start"), "{error}");
    }

    #[test]
    fn read_line_truncates_stderr_but_stops_a_strict_stream_at_the_cap() {
        let mut data = io::Cursor::new(b"abcdefghij\nxy\n".to_vec());
        let mut line = Vec::new();
        assert_eq!(
            read_line(&mut data, 4, &mut line, false).unwrap(),
            Some(true)
        );
        assert_eq!(line, b"abcd");
        line.clear();
        assert_eq!(
            read_line(&mut data, 4, &mut line, false).unwrap(),
            Some(false)
        );
        assert_eq!(line, b"xy");
        line.clear();
        assert_eq!(read_line(&mut data, 4, &mut line, false).unwrap(), None);

        let mut data = io::Cursor::new(b"abcdefghij\n".to_vec());
        line.clear();
        assert_eq!(
            read_line(&mut data, 4, &mut line, true).unwrap(),
            Some(true)
        );
    }

    #[test]
    fn read_line_treats_an_unterminated_strict_line_as_end_of_stream() {
        let mut data = io::Cursor::new(b"{\"id\":1".to_vec());
        let mut line = Vec::new();
        assert_eq!(read_line(&mut data, 64, &mut line, true).unwrap(), None);
    }
}
