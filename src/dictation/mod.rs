//! Dictation: speech captured, transcribed, and typed into the focused app.
//!
//! ```text
//! microphone ──> audio ──> chunker ──> whisper thread ──> injector ──> app
//!              (calloop)  (calloop)      (its own)        (calloop)
//! ```
//!
//! Capture and chunking are cheap enough to sit on the compositor's event
//! loop — a memcpy and some arithmetic over 20 ms frames. The request to the
//! whisper server is the only part that blocks for a meaningful length of
//! time, so it is the only part that gets a thread. That keeps a hung server
//! from ever costing a frame.

pub mod audio;
pub mod chunker;
pub mod injector;
pub mod whisper;

use audio::Capture;
use chunker::Chunker;
use injector::{Delivery, Injection};
use whisper::{Transcriber, Transcript};

use crate::niri::State;

/// A dictation session: the microphone is open and words are being typed.
pub struct Dictation {
    chunker: Chunker,
    transcriber: Transcriber,
    /// Dropping this releases the microphone, so it is kept alive here for as
    /// long as the session lasts.
    _capture: Capture,
    /// The most recent provisional text, so the indicator can show what is
    /// coming before it settles.
    preview: String,
    /// Generation of the utterance whose text is still to come. Anything older
    /// arriving late has been superseded and is dropped.
    generation: u64,
}

impl Dictation {
    /// What is currently being heard but has not settled.
    pub fn preview(&self) -> &str {
        &self.preview
    }
}

impl State {
    /// Start or stop dictation.
    pub fn toggle_dictation(&mut self) {
        if self.niri.dictation.is_some() {
            self.stop_dictation();
        } else if let Err(err) = self.start_dictation() {
            warn!("could not start dictation: {err:?}");
        }
    }

    fn start_dictation(&mut self) -> anyhow::Result<()> {
        let config = self.niri.config.borrow().dictation.clone();

        // Retire the previous session's source. It is not removed at stop time
        // because the last utterance is still being transcribed then.
        if let Some(token) = self.niri.dictation_transcripts.take() {
            self.niri.event_loop.remove(token);
        }

        let (to_niri, from_worker) = calloop::channel::channel();
        let transcripts = self
            .niri
            .event_loop
            .insert_source(from_worker, |event, _, state| {
                if let calloop::channel::Event::Msg(transcript) = event {
                    state.on_transcript(transcript);
                }
            })
            .map_err(|err| anyhow::anyhow!("error inserting transcript source: {err}"))?;

        let transcriber = Transcriber::spawn(config.clone(), to_niri);

        // The capture callback owns nothing: it hands audio to the compositor,
        // which owns the chunker and decides what to do with the cuts.
        let capture = Capture::start(self.niri.event_loop.clone(), |state, audio| {
            state.on_dictation_audio(audio);
        })?;

        self.niri.dictation = Some(Dictation {
            chunker: Chunker::new(config),
            transcriber,
            _capture: capture,
            preview: String::new(),
            generation: 0,
        });

        self.niri.dictation_transcripts = Some(transcripts);
        self.niri.dictation_indicator.show();
        info!("dictation started");
        // Only the indicator appearing needs a frame. Transcript updates
        // deliberately do not redraw: the badge looks the same either way, and
        // audio capture shares this event loop, so a needless repaint of every
        // output delays the very audio it would be reporting on.
        self.niri.queue_redraw_all();

        Ok(())
    }

    pub fn stop_dictation(&mut self) {
        let Some(mut dictation) = self.niri.dictation.take() else {
            return;
        };

        // Send whatever was mid-sentence, or the last few words are lost
        // simply because the speaker stopped before the chunker cut.
        let mut cuts = Vec::new();
        dictation.chunker.flush(&mut cuts);
        for chunk in cuts.into_iter().filter(|c| c.final_) {
            dictation.transcriber.send(chunk);
        }

        // Any preedit belongs to text that will never be committed now.
        self.clear_preedit();
        self.niri.dictation_indicator.set_preview("");

        self.niri.dictation_indicator.hide();
        info!("dictation stopped");
        self.niri.queue_redraw_all();
    }

    /// Feed captured audio to the chunker and dispatch whatever it cuts.
    fn on_dictation_audio(&mut self, audio: &[u8]) {
        let Some(dictation) = &mut self.niri.dictation else {
            return;
        };

        let mut cuts = Vec::new();
        dictation.chunker.push(audio, &mut cuts);

        for chunk in cuts {
            debug!(
                "dictation cut {} gen={} {}ms",
                if chunk.final_ { "final" } else { "preview" },
                chunk.generation,
                chunk.pcm.len() / crate::dictation::chunker::ms_to_bytes(1).max(1),
            );
            if !dictation.transcriber.send(chunk) {
                warn!("dictation transcriber died, stopping");
                self.stop_dictation();
                return;
            }
        }
    }

    /// Text has come back from the server.
    fn on_transcript(&mut self, transcript: Transcript) {
        match transcript {
            Transcript::Failed(err) => {
                warn!("dictation failed: {err}");
                self.stop_dictation();
            }

            Transcript::Preview { text, generation } => {
                // Provisional text is only meaningful while still listening.
                let Some(dictation) = &mut self.niri.dictation else {
                    return;
                };
                // A preview whose utterance has already been committed would
                // put stale words on top of real ones.
                if generation < dictation.generation {
                    return;
                }
                let text = whisper::clean(&text);
                if text == dictation.preview {
                    return;
                }
                dictation.preview = text.clone();

                // Applications that speak text-input show this themselves as
                // preedit. The rest cannot, so the indicator carries it —
                // otherwise nothing appears until the utterance settles, which
                // is up to max_chunk_ms away.
                let delivery = self.inject_text(Injection::Preedit(text.clone()));
                debug!("dictation preview shown via {delivery:?}: {text:?}");
                if delivery != Delivery::TextInput
                    && self.niri.dictation_indicator.set_preview(&text)
                {
                    self.niri.queue_redraw_all();
                }
            }

            Transcript::Final { text, generation } => {
                // Committed even when the session has already ended: stopping
                // flushes whatever was mid-sentence, and that is exactly the
                // last thing the speaker said. Dropping it loses words.
                if let Some(dictation) = &mut self.niri.dictation {
                    if generation < dictation.generation {
                        return;
                    }
                    dictation.generation = generation + 1;
                    dictation.preview.clear();
                }
                // The words are about to be real; stop showing a guess at them.
                if self.niri.dictation_indicator.set_preview("") {
                    self.niri.queue_redraw_all();
                }

                let text = whisper::clean(&text);
                if text.is_empty() {
                    // Nothing was said, but a preedit may be showing a guess
                    // at it, and that guess is now known to be wrong.
                    self.clear_preedit();
                } else {
                    // A space between utterances, since each arrives trimmed.
                    let delivery = self.inject_text(Injection::Commit(format!("{text} ")));
                    debug!("dictation commit via {delivery:?}: {text:?}");
                }
            }
        }
    }

    /// Drop any provisional text showing in the focused application.
    fn clear_preedit(&mut self) {
        self.inject_text(Injection::Preedit(String::new()));
    }
}
