use crate::tts::AudioFormat;
use crate::tts::AudioStream;
use crate::tts::Cancellation;
use crate::tts::Result;
use crate::tts::SynthesizeOptions;
use crate::tts::TtsError;
use crate::tts::Voice;
use crate::tts::VoiceProvider;
use crate::tts::resolve_voice;
use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::Client;
use reqwest::header::USER_AGENT;

const PROVIDER: &str = "google-tts";
const TTS_BASE: &str = "https://translate.google.com/translate_tts";
const MAX_CHUNK_LEN: usize = 200;
const USER_AGENT_VALUE: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";

#[derive(Clone, Debug)]
pub struct GoogleTtsOptions {
    pub(crate) voice: String,
    pub(crate) slow: bool,
}

impl Default for GoogleTtsOptions {
    fn default() -> Self {
        Self {
            voice: "en".to_owned(),
            slow: false,
        }
    }
}

pub struct GoogleTts {
    options: GoogleTtsOptions,
    client: Client,
}

impl GoogleTts {
    pub(crate) fn new(options: GoogleTtsOptions) -> Self {
        Self {
            options,
            client: Client::new(),
        }
    }
}

#[async_trait]
impl VoiceProvider for GoogleTts {
    fn name(&self) -> &'static str {
        PROVIDER
    }

    async fn synthesize(&self, text: &str, options: &SynthesizeOptions) -> Result<AudioStream> {
        let lang = resolve_voice(
            options.voice.as_ref(),
            |_| false,
            || self.list_voices(&options.cancellation),
            &options.cancellation,
        )
        .await?
        .unwrap_or_else(|| self.options.voice.clone());
        let slow = self.options.slow;
        let chunks = chunk_text(text);
        let client = self.client.clone();
        let cancellation = options.cancellation.clone();

        let stream = async_stream::try_stream! {
            let total = chunks.len();
            for (index, chunk) in chunks.into_iter().enumerate() {
                cancellation.check()?;
                let query = vec![
                    ("ie", "UTF-8".to_owned()),
                    ("q", chunk.clone()),
                    ("tl", lang.clone()),
                    ("total", total.to_string()),
                    ("idx", index.to_string()),
                    ("textlen", chunk.chars().count().to_string()),
                    ("client", "tw-ob".to_owned()),
                    ("prev", "input".to_owned()),
                    ("ttsspeed", if slow { "0.24" } else { "1" }.to_owned()),
                ];
                let response = cancellation
                    .run(async {
                        client
                            .get(TTS_BASE)
                            .header(USER_AGENT, USER_AGENT_VALUE)
                            .query(&query)
                            .send()
                            .await
                            .map_err(format_google_error)
                    })
                    .await?;
                let status = response.status();
                if !status.is_success() {
                    Err(TtsError::provider_failed(
                        PROVIDER,
                        format!("request failed: {status}"),
                    ))?;
                }
                let mut body = response.bytes_stream();
                while let Some(part) = cancellation
                    .run(async { body.next().await.transpose().map_err(format_google_error) })
                    .await?
                {
                    if !part.is_empty() {
                        yield part;
                    }
                }
            }
        };
        Ok(AudioStream::new(AudioFormat::Mp3, stream))
    }

    async fn list_voices(&self, _cancellation: &Cancellation) -> Result<Vec<Voice>> {
        Ok(LANGUAGES
            .iter()
            .map(|(id, name)| Voice {
                id: (*id).to_owned(),
                name: (*name).to_owned(),
                lang: Some((*id).to_owned()),
            })
            .collect())
    }
}

fn chunk_text(text: &str) -> Vec<String> {
    if text.chars().count() <= MAX_CHUNK_LEN {
        return vec![text.to_owned()];
    }

    let mut chunks = Vec::new();
    let mut remaining = text.trim();
    while remaining.chars().count() > MAX_CHUNK_LEN {
        let cut_byte = remaining
            .char_indices()
            .take_while(|(index, _)| *index <= MAX_CHUNK_LEN)
            .filter(|(_, char)| char.is_whitespace())
            .map(|(index, _)| index)
            .last()
            .unwrap_or_else(|| {
                remaining
                    .char_indices()
                    .nth(MAX_CHUNK_LEN)
                    .map_or(remaining.len(), |(index, _)| index)
            });
        chunks.push(remaining[..cut_byte].trim().to_owned());
        remaining = remaining[cut_byte..].trim();
    }

    if !remaining.is_empty() {
        chunks.push(remaining.to_owned());
    }
    chunks
}

fn format_google_error(error: reqwest::Error) -> TtsError {
    let message = error.to_string();
    if message.contains("translate.google.com") {
        TtsError::provider_failed(PROVIDER, format!("network error: {message}"))
    } else {
        TtsError::Http(error)
    }
}

const LANGUAGES: [(&str, &str); 57] = [
    ("af", "Afrikaans"),
    ("ar", "Arabic"),
    ("bg", "Bulgarian"),
    ("bn", "Bengali"),
    ("bs", "Bosnian"),
    ("ca", "Catalan"),
    ("cs", "Czech"),
    ("da", "Danish"),
    ("de", "German"),
    ("el", "Greek"),
    ("en", "English"),
    ("es", "Spanish"),
    ("et", "Estonian"),
    ("fi", "Finnish"),
    ("fr", "French"),
    ("gu", "Gujarati"),
    ("hi", "Hindi"),
    ("hr", "Croatian"),
    ("hu", "Hungarian"),
    ("id", "Indonesian"),
    ("is", "Icelandic"),
    ("it", "Italian"),
    ("ja", "Japanese"),
    ("jw", "Javanese"),
    ("km", "Khmer"),
    ("kn", "Kannada"),
    ("ko", "Korean"),
    ("la", "Latin"),
    ("lv", "Latvian"),
    ("ml", "Malayalam"),
    ("mr", "Marathi"),
    ("ms", "Malay"),
    ("my", "Myanmar"),
    ("ne", "Nepali"),
    ("nl", "Dutch"),
    ("no", "Norwegian"),
    ("pl", "Polish"),
    ("pt", "Portuguese"),
    ("ro", "Romanian"),
    ("ru", "Russian"),
    ("si", "Sinhala"),
    ("sk", "Slovak"),
    ("sq", "Albanian"),
    ("sr", "Serbian"),
    ("su", "Sundanese"),
    ("sv", "Swedish"),
    ("sw", "Swahili"),
    ("ta", "Tamil"),
    ("te", "Telugu"),
    ("th", "Thai"),
    ("tl", "Filipino"),
    ("tr", "Turkish"),
    ("uk", "Ukrainian"),
    ("ur", "Urdu"),
    ("vi", "Vietnamese"),
    ("zh-CN", "Chinese (Simplified)"),
    ("zh-TW", "Chinese (Traditional)"),
];
