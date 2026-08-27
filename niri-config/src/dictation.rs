use crate::utils::MergeWith;

/// Speech dictation settings.
///
/// ```kdl
/// dictation {
///     whisper-url "http://127.0.0.1:9876/inference"
///     chunk-ms 3000
///     silence-ms 400
///     partial-ms 700
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dictation {
    /// The whisper.cpp server `/inference` endpoint audio is posted to.
    pub whisper_url: String,
    /// Language hint passed to the server.
    pub language: String,
    /// Smallest amount of audio worth sending as one utterance. Smaller is
    /// snappier but gives the model less context to work with.
    pub chunk_ms: u32,
    /// Forced cut for someone who talks straight through without pausing, so
    /// the transcript keeps moving.
    pub max_chunk_ms: u32,
    /// How much trailing quiet marks a natural cut once `chunk_ms` has piled
    /// up. Cutting on silence instead of on a clock keeps words whole.
    pub silence_ms: u32,
    /// How much voiced audio an utterance needs before it is worth a request.
    /// Filters out clicks, keystrokes and chair creaks.
    pub min_speech_ms: u32,
    /// Floor for the speech threshold (0-32767). The live threshold floats at
    /// three times the measured noise floor; this only stops it collapsing to
    /// nothing in a silent room.
    pub min_speech_rms: u32,
    /// How much new audio accumulates before the utterance still being spoken
    /// is re-decoded for a live preview. Every preview is another request, so
    /// this trades server load for how quickly words appear. 0 disables it.
    pub partial_ms: u32,
    /// Deadline for one request to the server.
    pub request_timeout_ms: u32,
}

impl Default for Dictation {
    fn default() -> Self {
        Self {
            whisper_url: String::from("http://127.0.0.1:9876/inference"),
            language: String::from("en"),
            chunk_ms: 3000,
            max_chunk_ms: 10000,
            silence_ms: 400,
            min_speech_ms: 200,
            min_speech_rms: 120,
            partial_ms: 700,
            request_timeout_ms: 30000,
        }
    }
}

#[derive(knuffel::Decode, Debug, Default, Clone, PartialEq, Eq)]
pub struct DictationPart {
    #[knuffel(child, unwrap(argument))]
    pub whisper_url: Option<String>,
    #[knuffel(child, unwrap(argument))]
    pub language: Option<String>,
    #[knuffel(child, unwrap(argument))]
    pub chunk_ms: Option<u32>,
    #[knuffel(child, unwrap(argument))]
    pub max_chunk_ms: Option<u32>,
    #[knuffel(child, unwrap(argument))]
    pub silence_ms: Option<u32>,
    #[knuffel(child, unwrap(argument))]
    pub min_speech_ms: Option<u32>,
    #[knuffel(child, unwrap(argument))]
    pub min_speech_rms: Option<u32>,
    #[knuffel(child, unwrap(argument))]
    pub partial_ms: Option<u32>,
    #[knuffel(child, unwrap(argument))]
    pub request_timeout_ms: Option<u32>,
}

impl MergeWith<DictationPart> for Dictation {
    fn merge_with(&mut self, part: &DictationPart) {
        merge_clone!(
            (self, part),
            whisper_url,
            language,
            chunk_ms,
            max_chunk_ms,
            silence_ms,
            min_speech_ms,
            min_speech_rms,
            partial_ms,
            request_timeout_ms
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::Config;

    #[test]
    fn parses_a_dictation_section() {
        let config = Config::parse_mem(
            r#"
            dictation {
                whisper-url "http://example.invalid:1234/inference"
                chunk-ms 2500
                partial-ms 500
            }
            "#,
        )
        .map_err(miette::Report::new)
        .unwrap();

        assert_eq!(
            config.dictation.whisper_url,
            "http://example.invalid:1234/inference"
        );
        assert_eq!(config.dictation.chunk_ms, 2500);
        assert_eq!(config.dictation.partial_ms, 500);
        // Anything not named keeps its default.
        assert_eq!(config.dictation.silence_ms, 400);
        assert_eq!(config.dictation.language, "en");
    }

    #[test]
    fn defaults_without_a_section() {
        let config = Config::parse_mem("").unwrap();
        assert_eq!(config.dictation, crate::Dictation::default());
    }
}
