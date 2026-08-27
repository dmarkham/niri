//! Talking to the whisper.cpp server.
//!
//! This is the one part of dictation that blocks for a meaningful length of
//! time, so it is the one part that gets its own thread. A hung server or a
//! vanished network stalls the worker and nothing else; the compositor learns
//! about it through a channel and carries on drawing frames either way.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use calloop::channel::Sender as CalloopSender;
use niri_config::Dictation;

use super::chunker::{wav_header, Chunk};

/// Something the transcriber has to say, delivered back to the event loop.
#[derive(Debug, Clone)]
pub enum Transcript {
    /// Text for an utterance still being spoken.
    Preview { text: String, generation: u64 },
    /// Text for a settled utterance.
    Final { text: String, generation: u64 },
    /// The server could not be reached. Dictation stops.
    Failed(String),
}

/// Handles to the transcription threads. Dropping this stops them.
#[derive(Debug)]
pub struct Transcriber {
    finals: Sender<Chunk>,
    previews: Sender<Chunk>,
    /// The utterance currently being filled. The preview lane reads this to
    /// skip previews of an utterance that has already been cut.
    filling: Arc<AtomicU64>,
}

impl Transcriber {
    /// Start transcription, reporting back over `to_niri`.
    ///
    /// Finals and previews get a thread each, as in the Go tool this was
    /// ported from. Sharing one thread looks tidier but serialises them: a
    /// preview cannot even be *sent* while a settled utterance is in flight,
    /// so every preview arriving in that window is discarded as stale and the
    /// text visibly stops keeping up with the speaker.
    pub fn spawn(config: Dictation, to_niri: CalloopSender<Transcript>) -> Self {
        let (finals, finals_rx) = mpsc::channel::<Chunk>();
        let (previews, previews_rx) = mpsc::channel::<Chunk>();
        let filling = Arc::new(AtomicU64::new(0));

        {
            let config = config.clone();
            let to_niri = to_niri.clone();
            thread::Builder::new()
                .name("dictation-finals".to_owned())
                .spawn(move || {
                    let agent = agent(&config);
                    // Settled text is never dropped or reordered: it is the
                    // transcript, and it has to arrive in spoken order.
                    while let Ok(chunk) = finals_rx.recv() {
                        if !transcribe_and_report(&agent, &config, &chunk, &to_niri) {
                            return;
                        }
                    }
                })
                .expect("failed to spawn dictation finals thread");
        }

        {
            let filling = filling.clone();
            thread::Builder::new()
                .name("dictation-previews".to_owned())
                .spawn(move || {
                    let agent = agent(&config);
                    while let Ok(chunk) = previews_rx.recv() {
                        // Latest wins: anything queued behind this describes
                        // more of the sentence than it does.
                        let mut chunk = chunk;
                        while let Ok(next) = previews_rx.try_recv() {
                            chunk = next;
                        }
                        if chunk.generation < filling.load(Ordering::Relaxed) {
                            continue; // its final is already on the way
                        }
                        if !transcribe_and_report(&agent, &config, &chunk, &to_niri) {
                            return;
                        }
                    }
                })
                .expect("failed to spawn dictation previews thread");
        }

        Self {
            finals,
            previews,
            filling,
        }
    }

    /// Queue a chunk. Returns false once the lane that handles it is gone.
    pub fn send(&self, chunk: Chunk) -> bool {
        if chunk.final_ {
            // This utterance is over, so previews of it are worthless now.
            self.filling
                .fetch_max(chunk.generation + 1, Ordering::Relaxed);
            self.finals.send(chunk).is_ok()
        } else {
            self.filling.fetch_max(chunk.generation, Ordering::Relaxed);
            self.previews.send(chunk).is_ok()
        }
    }
}

fn agent(config: &Dictation) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_millis(u64::from(
            config.request_timeout_ms,
        ))))
        .build()
        .new_agent()
}

/// Returns false if the caller should give up (channel closed or fatal error).
fn transcribe_and_report(
    agent: &ureq::Agent,
    config: &Dictation,
    chunk: &Chunk,
    to_niri: &CalloopSender<Transcript>,
) -> bool {
    let kind = if chunk.final_ { "final" } else { "preview" };
    let audio_ms = chunk.pcm.len() / super::chunker::ms_to_bytes(1).max(1);
    let started = std::time::Instant::now();

    match transcribe(agent, config, &chunk.pcm) {
        Ok(text) => {
            debug!(
                "dictation {kind} gen={} {}ms audio -> {:?} in {:?}",
                chunk.generation,
                audio_ms,
                text.trim(),
                started.elapsed(),
            );
            let msg = if chunk.final_ {
                Transcript::Final {
                    text,
                    generation: chunk.generation,
                }
            } else {
                Transcript::Preview {
                    text,
                    generation: chunk.generation,
                }
            };
            to_niri.send(msg).is_ok()
        }
        Err(err) => {
            // A failed preview is not worth stopping over; the next one will
            // very likely succeed and nothing was shown to the user. A failed
            // final means the transcript would silently lose words.
            if !chunk.final_ {
                debug!(
                    "dictation preview failed after {:?}: {err}",
                    started.elapsed()
                );
                return true;
            }
            warn!("dictation request failed: {err}");
            // Report it, then stop: losing words from the transcript without
            // saying so is worse than ending the session.
            let _ = to_niri.send(Transcript::Failed(err));
            false
        }
    }
}

/// POST one chunk as a multipart WAV upload and read back `{"text": "..."}`.
fn transcribe(agent: &ureq::Agent, config: &Dictation, pcm: &[u8]) -> Result<String, String> {
    const BOUNDARY: &str = "----niri-dictation-boundary";

    let mut body = Vec::with_capacity(pcm.len() + 512);
    let mut part = |name: &str, value: &str| {
        body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(value.as_bytes());
        body.extend_from_slice(b"\r\n");
    };
    part("temperature", "0.0");
    part("response_format", "json");
    part("language", &config.language);

    body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
    body.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"chunk.wav\"\r\n",
    );
    body.extend_from_slice(b"Content-Type: audio/wav\r\n\r\n");
    body.extend_from_slice(&wav_header(pcm.len()));
    body.extend_from_slice(pcm);
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());

    let response = agent
        .post(&config.whisper_url)
        .header(
            "Content-Type",
            &format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .send(&body[..])
        .map_err(|err| format!("whisper server: {err}"))?;

    let status = response.status();
    let text = response
        .into_body()
        .read_to_string()
        .map_err(|err| format!("whisper server: {err}"))?;

    if !status.is_success() {
        return Err(format!("whisper server: {status}: {}", text.trim()));
    }

    parse_response(&text)
}

/// Pull the transcript out of the server's JSON.
///
/// Hand-rolled rather than pulling in a deserialiser: the response is one flat
/// object and this avoids a struct definition travelling with it.
fn parse_response(body: &str) -> Result<String, String> {
    let value: serde_json::Value = serde_json::from_str(body)
        .map_err(|_| format!("whisper server: bad response: {}", body.trim()))?;

    if let Some(err) = value.get("error").and_then(|e| e.as_str()) {
        return Err(format!("whisper server: {err}"));
    }

    value
        .get("text")
        .and_then(|t| t.as_str())
        .map(str::to_owned)
        .ok_or_else(|| format!("whisper server: no text in response: {}", body.trim()))
}

/// Text the model emits when handed near-silence, which would otherwise land
/// in the transcript as if it had been spoken.
const HALLUCINATIONS: &[&str] = &[
    "thank you.",
    "thank you",
    "thanks for watching.",
    "thanks for watching",
    "okay.",
    "you",
    "[blank_audio]",
    "(blank_audio)",
    ".",
];

/// Clean one server response into text worth showing, or nothing.
///
/// Whisper answers near-silence with stray punctuation, bare digits, or a
/// stock pleasantry. None of that was said, so none of it should be typed.
pub fn clean(text: &str) -> String {
    let mut out: Vec<&str> = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if HALLUCINATIONS.contains(&line.to_lowercase().as_str()) {
            continue;
        }
        // No letters means no words: "0.", "...", and similar.
        if !line.chars().any(char::is_alphabetic) {
            continue;
        }
        out.push(line);
    }

    out.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_text_field() {
        assert_eq!(
            parse_response(r#"{"text": " Hello there.\n"}"#).unwrap(),
            " Hello there.\n"
        );
    }

    #[test]
    fn surfaces_a_server_error() {
        let err = parse_response(r#"{"error": "model not loaded"}"#).unwrap_err();
        assert!(err.contains("model not loaded"), "{err}");
    }

    #[test]
    fn rejects_junk() {
        assert!(parse_response("<html>502</html>").is_err());
    }

    #[test]
    fn drops_what_was_never_said() {
        assert_eq!(clean(" Thank you.\n"), "");
        assert_eq!(clean("[BLANK_AUDIO]"), "");
        assert_eq!(clean("0."), "");
        assert_eq!(clean("...\n"), "");
        assert_eq!(clean("   "), "");
    }

    #[test]
    fn keeps_what_was() {
        assert_eq!(clean(" Hello, this is a test.\n"), "Hello, this is a test.");
        // Several lines become one utterance.
        assert_eq!(clean("first line\nsecond line"), "first line second line");
        // A real sentence containing a stock phrase is not a hallucination.
        assert_eq!(
            clean("thank you for the review"),
            "thank you for the review"
        );
    }
}
