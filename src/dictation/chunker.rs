//! Cutting a live microphone stream into utterances worth transcribing.
//!
//! Ported from the Go `npu-whisper-type` tool, where the thresholds were tuned
//! against a real microphone in a real room.
//!
//! Two things come out of here. A *final* chunk is a cut utterance whose text
//! is committed. A *preview* is the buffer so far, re-decoded while the speaker
//! is still talking, so words reach the screen roughly `partial_ms` behind the
//! mouth instead of a whole chunk behind.

use niri_config::Dictation;

/// Audio is captured as 16 kHz mono s16le, which is what whisper.cpp wants, so
/// nothing resamples on either end.
pub const SAMPLE_RATE: u32 = 16000;
pub const BYTES_PER_SAMPLE: usize = 2;
const FRAME_MS: u32 = 20;
const FRAME_BYTES: usize = (SAMPLE_RATE as usize / 1000) * FRAME_MS as usize * BYTES_PER_SAMPLE;
/// Audio discarded at startup: opening a capture device produces a burst of
/// transients loud enough to read as speech.
const WARMUP_MS: u32 = 200;

pub fn ms_to_bytes(ms: u32) -> usize {
    (SAMPLE_RATE as usize / 1000) * ms as usize * BYTES_PER_SAMPLE
}

/// A piece of audio the chunker decided is worth sending.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub pcm: Vec<u8>,
    /// Which utterance this belongs to. Monotonic, so a preview whose
    /// generation has already been cut can be recognised as stale and dropped.
    pub generation: u64,
    /// Whether this is the settled utterance rather than a preview of one
    /// still being spoken.
    pub final_: bool,
}

/// Decides whether a frame is speech or room noise.
///
/// The threshold floats with the measured noise floor rather than sitting at a
/// fixed number, because that number is wrong for every microphone and room
/// but one: too high and every chunk is discarded and the transcript stays
/// silently empty, too low and silence gets sent for the server to hallucinate
/// over.
#[derive(Debug)]
struct SilenceGate {
    /// Running estimate of the room's noise level.
    floor: f64,
    /// Hard lower bound on the threshold.
    min: f64,
    seen: bool,
}

impl SilenceGate {
    fn new(min: f64) -> Self {
        Self {
            floor: 0.,
            min,
            seen: false,
        }
    }

    fn threshold(&self) -> f64 {
        (self.floor * 3.).max(self.min)
    }

    fn voiced(&mut self, level: f64) -> bool {
        if !self.seen || level < self.floor {
            self.floor = level;
            self.seen = true;
        } else {
            // Creep upward slowly (~40 s time constant) so a room that gets
            // noisier is tracked, but a burst of speech does not drag the
            // floor up behind it.
            self.floor += (level - self.floor) * 0.0005;
        }
        level > self.threshold()
    }
}

/// Splits raw PCM into utterance-sized pieces.
///
/// A chunk is cut once it holds at least `chunk_ms` of audio *and* the speaker
/// has gone quiet for `silence_ms`, or unconditionally at `max_chunk_ms` for
/// someone who never pauses. Cutting on silence rather than on a fixed clock
/// keeps words from being sliced in half mid-syllable.
#[derive(Debug)]
pub struct Chunker {
    config: Dictation,
    gate: SilenceGate,
    /// Bytes not yet forming a whole analysis frame. PipeWire hands over
    /// arbitrary sizes; the gate wants fixed ones.
    carry: Vec<u8>,
    buf: Vec<u8>,
    warmup_left: u32,
    silent_ms: u32,
    voiced_ms: u32,
    previewed: usize,
    generation: u64,
}

impl Chunker {
    pub fn new(config: Dictation) -> Self {
        let min = f64::from(config.min_speech_rms);
        Self {
            config,
            gate: SilenceGate::new(min),
            carry: Vec::new(),
            buf: Vec::new(),
            warmup_left: WARMUP_MS,
            silent_ms: 0,
            voiced_ms: 0,
            previewed: 0,
            generation: 0,
        }
    }

    /// The utterance being filled right now. Anything older has been cut.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Feed captured audio in, take chunks worth sending out.
    pub fn push(&mut self, pcm: &[u8], out: &mut Vec<Chunk>) {
        self.carry.extend_from_slice(pcm);

        while self.carry.len() >= FRAME_BYTES {
            let frame: Vec<u8> = self.carry.drain(..FRAME_BYTES).collect();
            self.push_frame(&frame, out);
        }
    }

    /// Cut whatever is buffered, for when dictation is being stopped and the
    /// last words would otherwise be lost.
    pub fn flush(&mut self, out: &mut Vec<Chunk>) {
        if !self.buf.is_empty() {
            self.emit(out);
        }
    }

    fn push_frame(&mut self, frame: &[u8], out: &mut Vec<Chunk>) {
        if self.warmup_left > 0 {
            self.warmup_left = self.warmup_left.saturating_sub(FRAME_MS);
            return;
        }

        self.buf.extend_from_slice(frame);

        if self.gate.voiced(rms(frame)) {
            self.silent_ms = 0;
            self.voiced_ms += FRAME_MS;
        } else {
            self.silent_ms += FRAME_MS;
        }

        let min_bytes = ms_to_bytes(self.config.chunk_ms);
        let max_bytes = ms_to_bytes(self.config.max_chunk_ms).max(min_bytes);

        let at_pause = self.buf.len() >= min_bytes && self.silent_ms >= self.config.silence_ms;
        if at_pause || self.buf.len() >= max_bytes {
            self.emit(out);
            return;
        }

        // Preview what has been said so far. The whole buffer is re-decoded
        // rather than just the newest slice: whisper needs the full utterance
        // to get the words right, so the preview grows and corrects itself in
        // place instead of arriving as disconnected fragments.
        if self.config.partial_ms == 0 || self.voiced_ms < self.config.min_speech_ms {
            return;
        }
        if self.buf.len() - self.previewed >= ms_to_bytes(self.config.partial_ms) {
            self.previewed = self.buf.len();
            out.push(Chunk {
                pcm: self.buf.clone(),
                generation: self.generation,
                final_: false,
            });
        }
    }

    fn emit(&mut self, out: &mut Vec<Chunk>) {
        // A lone transient — a click, a chair, a keystroke — is not speech.
        // Require a real run of voiced audio before spending a request on a
        // chunk, or the server hallucinates words over the noise.
        if self.voiced_ms >= self.config.min_speech_ms {
            out.push(Chunk {
                pcm: std::mem::take(&mut self.buf),
                generation: self.generation,
                final_: true,
            });
        }

        // Advance the generation even when the chunk was dropped: either way
        // this utterance is over and any preview of it is now stale.
        self.generation += 1;
        self.buf.clear();
        self.silent_ms = 0;
        self.voiced_ms = 0;
        self.previewed = 0;
    }
}

fn rms(frame: &[u8]) -> f64 {
    if frame.len() < BYTES_PER_SAMPLE {
        return 0.;
    }
    let mut sum = 0f64;
    let n = frame.len() / BYTES_PER_SAMPLE;
    for chunk in frame.chunks_exact(BYTES_PER_SAMPLE) {
        let sample = i16::from_le_bytes([chunk[0], chunk[1]]);
        sum += f64::from(sample) * f64::from(sample);
    }
    (sum / n as f64).sqrt()
}

/// The 44-byte RIFF header for a mono 16 kHz s16le payload.
pub fn wav_header(data_len: usize) -> Vec<u8> {
    let byte_rate = SAMPLE_RATE * BYTES_PER_SAMPLE as u32;
    let mut h = Vec::with_capacity(44);

    h.extend_from_slice(b"RIFF");
    h.extend_from_slice(&(36 + data_len as u32).to_le_bytes());
    h.extend_from_slice(b"WAVEfmt ");
    h.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    h.extend_from_slice(&1u16.to_le_bytes()); // PCM
    h.extend_from_slice(&1u16.to_le_bytes()); // channels
    h.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    h.extend_from_slice(&byte_rate.to_le_bytes());
    h.extend_from_slice(&(BYTES_PER_SAMPLE as u16).to_le_bytes()); // block align
    h.extend_from_slice(&(8 * BYTES_PER_SAMPLE as u16).to_le_bytes());
    h.extend_from_slice(b"data");
    h.extend_from_slice(&(data_len as u32).to_le_bytes());

    h
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Dictation {
        Dictation {
            chunk_ms: 1000,
            max_chunk_ms: 2000,
            silence_ms: 200,
            min_speech_ms: 100,
            min_speech_rms: 120,
            partial_ms: 400,
            ..Dictation::default()
        }
    }

    /// Loud enough to clear the gate's floor.
    fn speech(ms: u32) -> Vec<u8> {
        let samples = (SAMPLE_RATE as usize / 1000) * ms as usize;
        (0..samples)
            .flat_map(|i| {
                let v = if i % 2 == 0 { 8000i16 } else { -8000i16 };
                v.to_le_bytes()
            })
            .collect()
    }

    fn silence(ms: u32) -> Vec<u8> {
        vec![0; ms_to_bytes(ms)]
    }

    /// Get past the warmup discard *and* let the gate hear some room tone.
    ///
    /// The gate seeds its noise floor from the first frame that reaches it, so
    /// it needs quiet before speech. Real rooms supply that; a test has to say
    /// so explicitly.
    fn warmed(chunker: &mut Chunker) {
        let mut out = Vec::new();
        chunker.push(&silence(WARMUP_MS + 200), &mut out);
        out.clear();
    }

    #[test]
    fn cuts_at_a_pause_after_enough_audio() {
        let mut chunker = Chunker::new(config());
        warmed(&mut chunker);

        let mut out = Vec::new();
        chunker.push(&speech(1200), &mut out);
        assert!(
            !out.iter().any(|c| c.final_),
            "should not cut while still talking"
        );

        out.clear();
        chunker.push(&silence(300), &mut out);

        let finals: Vec<_> = out.iter().filter(|c| c.final_).collect();
        assert_eq!(
            finals.len(),
            1,
            "a pause after enough audio should cut once"
        );
    }

    #[test]
    fn forces_a_cut_for_someone_who_never_pauses() {
        let mut chunker = Chunker::new(config());
        warmed(&mut chunker);

        let mut out = Vec::new();
        chunker.push(&speech(5000), &mut out);

        let finals = out.iter().filter(|c| c.final_).count();
        assert!(finals >= 2, "max_chunk_ms should force cuts, got {finals}");
    }

    #[test]
    fn drops_a_chunk_with_no_speech_in_it() {
        let mut chunker = Chunker::new(config());
        warmed(&mut chunker);

        let mut out = Vec::new();
        chunker.push(&silence(3000), &mut out);

        assert!(
            out.is_empty(),
            "room tone is not worth a request, got {} chunks",
            out.len()
        );
    }

    #[test]
    fn previews_while_the_utterance_is_still_open() {
        let mut chunker = Chunker::new(config());
        warmed(&mut chunker);

        let mut out = Vec::new();
        chunker.push(&speech(900), &mut out);

        let previews: Vec<_> = out.iter().filter(|c| !c.final_).collect();
        assert!(!previews.is_empty(), "expected a preview before the cut");
        // Each preview carries the whole utterance so far, so they grow.
        for pair in previews.windows(2) {
            assert!(
                pair[1].pcm.len() > pair[0].pcm.len(),
                "previews should grow, not fragment"
            );
        }
    }

    /// A preview and the final that supersedes it must be distinguishable, or
    /// stale text lands on top of real text.
    #[test]
    fn generation_advances_on_every_cut() {
        let mut chunker = Chunker::new(config());
        warmed(&mut chunker);

        let mut out = Vec::new();
        chunker.push(&speech(1200), &mut out);
        chunker.push(&silence(300), &mut out);

        let first = chunker.generation();
        assert!(out.iter().any(|c| c.final_ && c.generation < first));

        out.clear();
        chunker.push(&speech(1200), &mut out);
        chunker.push(&silence(300), &mut out);
        assert!(chunker.generation() > first, "generation must be monotonic");
    }

    #[test]
    fn partial_ms_zero_disables_previews() {
        let mut chunker = Chunker::new(Dictation {
            partial_ms: 0,
            ..config()
        });
        warmed(&mut chunker);

        let mut out = Vec::new();
        chunker.push(&speech(1800), &mut out);

        assert!(out.iter().all(|c| c.final_), "previews should be off");
    }

    #[test]
    fn wav_header_describes_the_payload() {
        let h = wav_header(32000);
        assert_eq!(&h[0..4], b"RIFF");
        assert_eq!(&h[8..12], b"WAVE");
        assert_eq!(h.len(), 44);
        assert_eq!(u32::from_le_bytes([h[40], h[41], h[42], h[43]]), 32000);
        // 16 kHz, mono, 16-bit.
        assert_eq!(
            u32::from_le_bytes([h[24], h[25], h[26], h[27]]),
            SAMPLE_RATE
        );
        assert_eq!(u16::from_le_bytes([h[22], h[23]]), 1);
    }
}
