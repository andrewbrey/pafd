use crate::tts::AudioFormat;
use crate::tts::AudioStream;
use crate::tts::Cancellation;
use crate::tts::Result;
use crate::tts::SynthesizeOptions;
use crate::tts::TtsError;
use crate::tts::Voice;
use crate::tts::VoiceProvider;
use crate::tts::cached_voices;
use crate::tts::resolve_voice;
use async_trait::async_trait;
use bytes::Bytes;
use futures_util::SinkExt;
use futures_util::StreamExt;
use reqwest::Client;
use serde::Deserialize;
use sha2::Digest;
use sha2::Sha256;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tokio::net::TcpStream;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;

const PROVIDER: &str = "edge-tts";
const TRUSTED_CLIENT_TOKEN: &str = "6A5AA1D4EAFF4E9FB37E23D68491D6F4";
const CHROMIUM_FULL_VERSION: &str = "143.0.3650.75";
const WINDOWS_FILE_TIME_EPOCH: u128 = 11_644_473_600;
const VOICES_URL: &str = "https://speech.platform.bing.com/consumer/speech/synthesize/readaloud/voices/list?trustedclienttoken=6A5AA1D4EAFF4E9FB37E23D68491D6F4";

#[derive(Clone, Debug)]
pub struct EdgeTtsOptions {
    voice: String,
    pitch: String,
    volume: String,
    output_format: String,
}

impl Default for EdgeTtsOptions {
    fn default() -> Self {
        Self {
            voice: "en-US-AvaNeural".to_owned(),
            pitch: "default".to_owned(),
            volume: "default".to_owned(),
            output_format: "audio-24khz-48kbitrate-mono-mp3".to_owned(),
        }
    }
}

pub struct EdgeTts {
    options: EdgeTtsOptions,
    client: Client,
}

impl EdgeTts {
    pub(crate) fn new(options: EdgeTtsOptions) -> Self {
        Self {
            options,
            client: Client::new(),
        }
    }
}

#[async_trait]
impl VoiceProvider for EdgeTts {
    fn name(&self) -> &'static str {
        PROVIDER
    }

    fn has_voice(&self, id: &str) -> bool {
        let mut parts = id.split('-');
        let Some(lang) = parts.next() else {
            return false;
        };
        (lang.len() == 2 || lang.len() == 3) && id.ends_with("Neural") && parts.count() >= 2
    }

    async fn synthesize(&self, text: &str, options: &SynthesizeOptions) -> Result<AudioStream> {
        let voice = resolve_voice(
            options.voice.as_ref(),
            |voice| self.has_voice(voice),
            || self.list_voices(&options.cancellation),
            &options.cancellation,
        )
        .await?
        .unwrap_or_else(|| self.options.voice.clone());
        let format = format_from_output_format(&self.options.output_format);
        let socket = Box::pin(open_edge_socket(
            text,
            &voice,
            &self.options.pitch,
            &self.options.volume,
            &self.options.output_format,
            &options.cancellation,
        ))
        .await?;
        let stream = edge_stream(socket, options.cancellation.clone());
        Ok(AudioStream::new(format, stream))
    }

    async fn list_voices(&self, cancellation: &Cancellation) -> Result<Vec<Voice>> {
        cached_voices(PROVIDER, || {
            cancellation.run(async {
                let response = self
                    .client
                    .get(VOICES_URL)
                    .send()
                    .await
                    .map_err(format_edge_error)?;
                let status = response.status();
                if !status.is_success() {
                    return Err(TtsError::provider_failed(
                        PROVIDER,
                        format!("voice list request failed: {status}"),
                    ));
                }
                let voices = response
                    .json::<Vec<EdgeVoice>>()
                    .await
                    .map_err(format_edge_error)?;
                Ok(voices
                    .into_iter()
                    .map(|voice| Voice {
                        id: voice.short_name,
                        name: voice.friendly_name,
                        lang: Some(voice.locale),
                    })
                    .collect())
            })
        })
        .await
    }
}

#[derive(Debug, Deserialize)]
struct EdgeVoice {
    #[serde(rename = "ShortName")]
    short_name: String,
    #[serde(rename = "FriendlyName")]
    friendly_name: String,
    #[serde(rename = "Locale")]
    locale: String,
}

type EdgeSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn open_edge_socket(
    text: &str,
    voice: &str,
    pitch: &str,
    volume: &str,
    output_format: &str,
    cancellation: &Cancellation,
) -> Result<EdgeSocket> {
    cancellation.check()?;
    let ws_url = build_ws_url();
    let mut request = ws_url.into_client_request()?;
    let headers = request.headers_mut();
    headers.insert("host", HeaderValue::from_static("speech.platform.bing.com"));
    headers.insert(
        "origin",
        HeaderValue::from_static("chrome-extension://jdiccldimpdaibmpdkjnbmckianbfold"),
    );
    headers.insert("user-agent", HeaderValue::from_str(&edge_user_agent())?);

    let (mut socket, _) =
        Box::pin(cancellation.run(async { connect_async(request).await.map_err(TtsError::from) }))
            .await?;

    let config = format!(
        "Content-Type:application/json; charset=utf-8\r\nPath:speech.config\r\n\r\n{{\"context\":{{\"synthesis\":{{\"audio\":{{\"metadataoptions\":{{\"sentenceBoundaryEnabled\":\"false\",\"wordBoundaryEnabled\":\"true\"}},\"outputFormat\":\"{output_format}\"}}}}}}}}"
    );
    cancellation
        .run(async {
            socket
                .send(Message::Text(config.into()))
                .await
                .map_err(TtsError::from)
        })
        .await?;

    let request_id = request_id();
    let ssml = build_ssml(text, voice, pitch, volume);
    let request = format!(
        "X-RequestId:{request_id}\r\nContent-Type:application/ssml+xml\r\nPath:ssml\r\n\r\n{ssml}"
    );
    cancellation
        .run(async {
            socket
                .send(Message::Text(request.into()))
                .await
                .map_err(TtsError::from)
        })
        .await?;

    Ok(socket)
}

fn edge_stream(
    mut socket: EdgeSocket,
    cancellation: Cancellation,
) -> impl futures_util::Stream<Item = Result<Bytes>> + Send + 'static {
    async_stream::try_stream! {
        loop {
            let message = cancellation
                .run(async {
                    socket
                        .next()
                        .await
                        .transpose()
                        .map_err(TtsError::from)?
                        .ok_or_else(|| {
                            TtsError::provider_failed(PROVIDER, "websocket closed before turn.end")
                        })
                })
                .await?;

            match message {
                Message::Binary(data) => {
                    let data = data.as_ref();
                    if let Some(index) = find_subslice(data, b"Path:audio\r\n") {
                        let chunk = Bytes::copy_from_slice(&data[index + "Path:audio\r\n".len()..]);
                        if !chunk.is_empty() {
                            yield chunk;
                        }
                    }
                }
                Message::Text(text) if text.contains("Path:turn.end") => {
                    let _ = socket.close(None).await;
                    break;
                }
                Message::Close(_) => {
                    Err(TtsError::provider_failed(
                        PROVIDER,
                        "websocket closed before turn.end",
                    ))?;
                }
                _ => {}
            }
        }
    }
}

fn build_ws_url() -> String {
    let token = generate_sec_ms_gec_token();
    format!(
        "wss://speech.platform.bing.com/consumer/speech/synthesize/readaloud/edge/v1?TrustedClientToken={TRUSTED_CLIENT_TOKEN}&Sec-MS-GEC={token}&Sec-MS-GEC-Version=1-{CHROMIUM_FULL_VERSION}"
    )
}

fn generate_sec_ms_gec_token() -> String {
    let now = u128::from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    );
    let ticks = (now + WINDOWS_FILE_TIME_EPOCH) * 10_000_000;
    let rounded_ticks = ticks - (ticks % 3_000_000_000);
    let mut hasher = Sha256::new();
    hasher.update(format!("{rounded_ticks}{TRUSTED_CLIENT_TOKEN}").as_bytes());
    to_hex_upper(&hasher.finalize())
}

fn request_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = std::process::id();
    format!("{now:032x}{pid:08x}").chars().take(32).collect()
}

fn edge_user_agent() -> String {
    let major = CHROMIUM_FULL_VERSION.split('.').next().unwrap_or("143");
    format!(
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{major}.0.0.0 Safari/537.36 Edg/{major}.0.0.0"
    )
}

fn to_hex_upper(bytes: &[u8]) -> String {
    use std::fmt::Write;

    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut acc, byte| {
            let _ = write!(acc, "{byte:02X}");
            acc
        })
}

fn build_ssml(text: &str, voice: &str, pitch: &str, volume: &str) -> String {
    format!(
        "<speak version=\"1.0\" xmlns=\"http://www.w3.org/2001/10/synthesis\" xmlns:mstts=\"https://www.w3.org/2001/mstts\" xml:lang=\"en-US\">
  <voice name=\"{}\">
    <prosody rate=\"default\" pitch=\"{}\" volume=\"{}\">
      {}
    </prosody>
  </voice>
</speak>",
        escape_xml(voice),
        escape_xml(pitch),
        escape_xml(volume),
        escape_xml(text)
    )
}

fn escape_xml(value: &str) -> String {
    value
        .chars()
        .map(|char| match char {
            '<' => "&lt;".to_owned(),
            '>' => "&gt;".to_owned(),
            '&' => "&amp;".to_owned(),
            '"' => "&quot;".to_owned(),
            '\'' => "&apos;".to_owned(),
            _ => char.to_string(),
        })
        .collect()
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn format_from_output_format(output_format: &str) -> AudioFormat {
    let lower = output_format.to_ascii_lowercase();
    if lower.contains("mp3") {
        AudioFormat::Mp3
    } else if lower.contains("riff") || lower.contains("wav") {
        AudioFormat::Wav
    } else {
        AudioFormat::Unknown(output_format.to_owned())
    }
}

fn format_edge_error(error: reqwest::Error) -> TtsError {
    let message = error.to_string();
    if message.contains("speech.platform.bing.com") {
        TtsError::provider_failed(PROVIDER, format!("network error: {message}"))
    } else {
        TtsError::Http(error)
    }
}
