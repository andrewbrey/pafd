use crate::tts;
use reqwest::Body;
use reqwest::StatusCode;
use reqwest::header::AUTHORIZATION;
use reqwest::header::CONTENT_TYPE;
use std::future::Future;
use thiserror::Error;

pub type Result<T> = std::result::Result<T, ClientError>;

#[derive(Debug, Error)]
pub enum ClientError {
    #[error(transparent)]
    Tts(#[from] tts::TtsError),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("server returned {status}: {body}")]
    Server { status: StatusCode, body: String },

    #[error("could not reach pafd server at {url} — is it running?")]
    ServerUnreachable { url: String },
}

#[derive(Clone, Debug)]
pub struct ClientOptions {
    pub server_url: String,
    pub token: Option<String>,
}

pub struct Client {
    http: reqwest::Client,
    server_url: String,
    token: Option<String>,
}

impl Client {
    #[must_use]
    pub fn new(options: ClientOptions) -> Self {
        Self {
            http: reqwest::Client::new(),
            server_url: options.server_url,
            token: options.token,
        }
    }

    /// Pings the playback server to verify it is reachable.
    ///
    /// # Errors
    ///
    /// Returns an error when the server is unreachable or returns a non-success
    /// status.
    pub async fn ping(&self) -> Result<()> {
        let response = self
            .http
            .get(ping_url(&self.server_url))
            .send()
            .await
            .map_err(|e| {
                if e.is_connect() || e.is_timeout() {
                    ClientError::ServerUnreachable {
                        url: self.server_url.clone(),
                    }
                } else {
                    ClientError::Http(e)
                }
            })?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let body = response.text().await?;
        Err(ClientError::Server { status, body })
    }

    /// Synthesizes speech and streams the audio bytes to the playback server.
    ///
    /// # Errors
    ///
    /// Returns an error when synthesis fails, cancellation is requested, the
    /// server is unreachable, or the server rejects playback.
    pub async fn play_tts(&self, request: &TtsPlaybackRequest) -> Result<()> {
        let mut tts = request.build_tts();
        let options = request.synthesize_options();
        let stream = tts.synthesize(&request.message, &options).await?;
        let content_type = content_type(stream.format());
        let body = Body::wrap_stream(stream.into_inner());
        self.post_stream(content_type, body, &request.cancellation)
            .await
    }

    /// Sends OGG audio bytes to the playback server.
    ///
    /// # Errors
    ///
    /// Returns an error when cancellation is requested, the server is
    /// unreachable, or the server rejects playback.
    pub async fn play_ogg_bytes(
        &self,
        bytes: &[u8],
        cancellation: &tts::Cancellation,
    ) -> Result<()> {
        self.post_stream("audio/ogg", Body::from(bytes.to_vec()), cancellation)
            .await
    }

    async fn post_stream(
        &self,
        content_type: &'static str,
        body: Body,
        cancellation: &tts::Cancellation,
    ) -> Result<()> {
        cancellation.check()?;
        let mut builder = self
            .http
            .post(stream_url(&self.server_url))
            .header(CONTENT_TYPE, content_type)
            .body(body);

        if let Some(token) = &self.token {
            builder = builder.header(AUTHORIZATION, format!("Bearer {token}"));
        }

        let response = cancellable(cancellation, builder.send()).await?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }

        let body = cancellable(cancellation, response.text()).await?;
        Err(ClientError::Server { status, body })
    }
}

#[derive(Clone, Debug)]
pub struct TtsPlaybackRequest {
    pub message: String,
    pub provider: tts::ProviderKind,
    pub voice: Option<String>,
    pub cancellation: tts::Cancellation,
}

impl TtsPlaybackRequest {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            provider: tts::ProviderKind::Auto,
            voice: None,
            cancellation: tts::Cancellation::none(),
        }
    }

    #[must_use]
    pub const fn with_provider(mut self, provider: tts::ProviderKind) -> Self {
        self.provider = provider;
        self
    }

    #[must_use]
    pub fn with_voice(mut self, voice: impl Into<String>) -> Self {
        self.voice = Some(voice.into());
        self
    }

    #[must_use]
    pub fn with_cancellation(mut self, cancellation: tts::Cancellation) -> Self {
        self.cancellation = cancellation;
        self
    }

    fn build_tts(&self) -> tts::Tts {
        match self.provider {
            tts::ProviderKind::Auto => tts::Tts::new(),
            kind => tts::Tts::with_provider_kind(kind),
        }
    }

    fn synthesize_options(&self) -> tts::SynthesizeOptions {
        let options =
            tts::SynthesizeOptions::default().with_cancellation(self.cancellation.clone());
        if let Some(voice) = &self.voice {
            options.with_voice(voice.as_str())
        } else {
            options
        }
    }
}

async fn cancellable<T, F>(cancellation: &tts::Cancellation, future: F) -> Result<T>
where
    F: Future<Output = std::result::Result<T, reqwest::Error>>,
{
    tokio::select! {
        result = future => result.map_err(ClientError::from),
        () = cancellation.cancelled() => Err(ClientError::Tts(tts::TtsError::Cancelled)),
    }
}

fn stream_url(server_url: &str) -> String {
    format!("{}/stream", server_url.trim_end_matches('/'))
}

fn ping_url(server_url: &str) -> String {
    format!("{}/ping", server_url.trim_end_matches('/'))
}

const fn content_type(format: &tts::AudioFormat) -> &'static str {
    match format {
        tts::AudioFormat::Mp3 => "audio/mpeg",
        tts::AudioFormat::Wav => "audio/wav",
        tts::AudioFormat::Unknown(_) => "application/octet-stream",
    }
}
