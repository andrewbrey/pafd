use crate::tts::AudioFormat;
use crate::tts::AudioStream;
use crate::tts::Cancellation;
use crate::tts::Result;
use crate::tts::SynthesizeOptions;
use crate::tts::TtsError;
use crate::tts::Voice;
use crate::tts::VoiceProvider;
use crate::tts::cache_dir;
use crate::tts::cached_voices;
use crate::tts::process;
use crate::tts::resolve_voice;
use async_trait::async_trait;
use reqwest::Client;
use serde_json::Value;
use std::path::Path;
use std::path::PathBuf;

const PROVIDER: &str = "piper";
const PIPER_BINARY_VERSION: &str = "2023.11.14-2";
const PIPER_PIP_VERSION: &str = "1.4.1";
const DEFAULT_VOICE: &str = "en_US-libritts-high";
const HF_BASE: &str = "https://huggingface.co/rhasspy/piper-voices/resolve/main";

#[derive(Clone, Debug)]
pub struct PiperOptions {
    pub(crate) voice: String,
    pub(crate) length_scale: f32,
    pub(crate) speaker: u32,
    pub(crate) cache_dir: Option<PathBuf>,
}

impl Default for PiperOptions {
    fn default() -> Self {
        Self {
            voice: DEFAULT_VOICE.to_owned(),
            length_scale: 1.0,
            speaker: 0,
            cache_dir: None,
        }
    }
}

pub struct Piper {
    options: PiperOptions,
    client: Client,
}

impl Piper {
    pub(crate) fn new(options: PiperOptions) -> Self {
        Self {
            options,
            client: Client::new(),
        }
    }

    fn cache_dir(&self) -> Result<PathBuf> {
        self.options
            .cache_dir
            .as_ref()
            .map_or_else(|| cache_dir("piper"), |dir| Ok(dir.clone()))
    }
}

#[async_trait]
impl VoiceProvider for Piper {
    fn name(&self) -> &'static str {
        PROVIDER
    }

    async fn synthesize(&self, text: &str, options: &SynthesizeOptions) -> Result<AudioStream> {
        let voice = resolve_voice(
            options.voice.as_ref(),
            |_| false,
            || self.list_voices(&options.cancellation),
            &options.cancellation,
        )
        .await?
        .unwrap_or_else(|| self.options.voice.clone());

        let piper = ensure_piper(&self.cache_dir()?, &self.client, &options.cancellation).await?;
        let model_path = ensure_voice(
            &voice,
            &self.cache_dir()?,
            &self.client,
            &options.cancellation,
        )
        .await?;
        let stream = stream_piper(
            &piper,
            &model_path,
            text,
            self.options.length_scale,
            self.options.speaker,
            options.cancellation.clone(),
        )?;
        Ok(AudioStream::new(AudioFormat::Wav, stream))
    }

    async fn list_voices(&self, cancellation: &Cancellation) -> Result<Vec<Voice>> {
        cached_voices(PROVIDER, || async {
            let index = fetch_voices_index(&self.client, cancellation).await?;
            let Some(voices) = index.as_object() else {
                return Err(TtsError::provider_failed(
                    PROVIDER,
                    "voices index was not an object",
                ));
            };

            Ok(voices
                .iter()
                .map(|(id, info)| {
                    let name = info
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or(id)
                        .to_owned();
                    let lang = info
                        .get("language")
                        .and_then(|language| language.get("code"))
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    Voice {
                        id: id.clone(),
                        name,
                        lang,
                    }
                })
                .collect())
        })
        .await
    }
}

#[derive(Clone, Debug)]
struct PiperCmd {
    command: String,
    args: Vec<String>,
    ld_library_path: Option<String>,
}

async fn ensure_piper(
    cache_dir: &Path,
    client: &Client,
    cancellation: &Cancellation,
) -> Result<PiperCmd> {
    cancellation.check()?;
    if let Some(piper) = find_system_piper(cancellation).await {
        return Ok(piper);
    }
    if let Some(piper) = find_cached_piper(cache_dir).await? {
        return Ok(piper);
    }
    if cfg!(target_os = "linux") && matches!(std::env::consts::ARCH, "x86_64" | "aarch64") {
        install_binary(cache_dir, client, cancellation).await
    } else {
        install_pip_venv(cache_dir, cancellation).await
    }
}

async fn find_system_piper(cancellation: &Cancellation) -> Option<PiperCmd> {
    let piper_args = vec!["--version".to_owned()];
    if process::status("piper", &piper_args, cancellation)
        .await
        .is_ok()
    {
        return Some(PiperCmd {
            command: "piper".to_owned(),
            args: Vec::new(),
            ld_library_path: None,
        });
    }

    for python in ["python3", "python"] {
        let args = vec!["-m".to_owned(), "piper".to_owned(), "--version".to_owned()];
        if process::status(python, &args, cancellation).await.is_ok() {
            return Some(PiperCmd {
                command: python.to_owned(),
                args: vec!["-m".to_owned(), "piper".to_owned()],
                ld_library_path: None,
            });
        }
    }

    None
}

async fn find_cached_piper(cache_dir: &Path) -> Result<Option<PiperCmd>> {
    let binary_path = cache_dir.join("bin").join("piper").join("piper");
    if tokio::fs::try_exists(&binary_path).await? {
        let ld_library_path = binary_path.parent().map(|path| path.display().to_string());
        return Ok(Some(PiperCmd {
            command: binary_path.display().to_string(),
            args: Vec::new(),
            ld_library_path,
        }));
    }

    let venv_path = venv_executable(cache_dir, "piper");
    if tokio::fs::try_exists(&venv_path).await? {
        return Ok(Some(PiperCmd {
            command: venv_path.display().to_string(),
            args: Vec::new(),
            ld_library_path: None,
        }));
    }

    Ok(None)
}

#[cfg(unix)]
async fn install_binary(
    cache_dir: &Path,
    client: &Client,
    cancellation: &Cancellation,
) -> Result<PiperCmd> {
    let bin_dir = cache_dir.join("bin");
    cancellation
        .run(async {
            tokio::fs::create_dir_all(&bin_dir)
                .await
                .map_err(TtsError::from)
        })
        .await?;

    let file = match std::env::consts::ARCH {
        "aarch64" => "piper_linux_aarch64",
        _ => "piper_linux_x86_64",
    };
    let archive_path = bin_dir.join(format!("{file}.tar.gz"));
    let url = format!(
        "https://github.com/rhasspy/piper/releases/download/{PIPER_BINARY_VERSION}/{file}.tar.gz"
    );

    tracing::info!("downloading piper binary to {}", archive_path.display());
    eprintln!(
        "piper not installed; downloading and caching the piper binary to {} (one-time setup)",
        bin_dir.display()
    );
    download(client, &url, &archive_path, cancellation).await?;

    let args = vec![
        "xzf".to_owned(),
        archive_path.display().to_string(),
        "-C".to_owned(),
        bin_dir.display().to_string(),
    ];
    process::status("tar", &args, cancellation).await?;

    let binary_path = bin_dir.join("piper").join("piper");
    let permissions = std::os::unix::fs::PermissionsExt::from_mode(0o755);
    cancellation
        .run(async {
            tokio::fs::set_permissions(&binary_path, permissions)
                .await
                .map_err(TtsError::from)
        })
        .await?;
    let _ = tokio::fs::remove_file(&archive_path).await;

    let ld_library_path = binary_path.parent().map(|path| path.display().to_string());
    Ok(PiperCmd {
        command: binary_path.display().to_string(),
        args: Vec::new(),
        ld_library_path,
    })
}

#[cfg(not(unix))]
async fn install_binary(
    cache_dir: &Path,
    _client: &Client,
    cancellation: &Cancellation,
) -> Result<PiperCmd> {
    install_pip_venv(cache_dir, cancellation).await
}

async fn install_pip_venv(cache_dir: &Path, cancellation: &Cancellation) -> Result<PiperCmd> {
    let venv_dir = cache_dir.join("venv");
    cancellation
        .run(async {
            tokio::fs::create_dir_all(&venv_dir)
                .await
                .map_err(TtsError::from)
        })
        .await?;

    let python_args = vec!["--version".to_owned()];
    let python = if process::status("python3", &python_args, cancellation)
        .await
        .is_ok()
    {
        "python3"
    } else {
        "python"
    };

    tracing::info!("creating piper virtualenv at {}", venv_dir.display());
    eprintln!(
        "piper not installed; creating a virtualenv and installing piper-tts at {} (one-time setup, may take a minute)",
        venv_dir.display()
    );
    let venv_args = vec![
        "-m".to_owned(),
        "venv".to_owned(),
        venv_dir.display().to_string(),
    ];
    process::status(python, &venv_args, cancellation).await?;

    let pip = venv_executable(cache_dir, "pip");
    let pip_command = pip.display().to_string();
    let install_args = vec![
        "install".to_owned(),
        format!("piper-tts=={PIPER_PIP_VERSION}"),
        "pathvalidate".to_owned(),
    ];
    process::status(&pip_command, &install_args, cancellation).await?;

    let piper = venv_executable(cache_dir, "piper");
    Ok(PiperCmd {
        command: piper.display().to_string(),
        args: Vec::new(),
        ld_library_path: None,
    })
}

async fn ensure_voice(
    voice: &str,
    cache_dir: &Path,
    client: &Client,
    cancellation: &Cancellation,
) -> Result<PathBuf> {
    let voices_dir = cache_dir.join("voices");
    let model_path = voices_dir.join(format!("{voice}.onnx"));
    let config_path = voices_dir.join(format!("{voice}.onnx.json"));
    let has_model = tokio::fs::try_exists(&model_path).await?;
    let has_config = tokio::fs::try_exists(&config_path).await?;
    if has_model && has_config {
        return Ok(model_path);
    }

    cancellation
        .run(async {
            tokio::fs::create_dir_all(&voices_dir)
                .await
                .map_err(TtsError::from)
        })
        .await?;

    let base_path = voice_base_path(voice)?;
    tracing::info!("downloading piper voice {voice}");
    eprintln!(
        "downloading and caching piper voice '{voice}' to {} (one-time per voice)",
        voices_dir.display()
    );
    download(
        client,
        &format!("{HF_BASE}/{base_path}.onnx?download=true"),
        &model_path,
        cancellation,
    )
    .await?;
    download(
        client,
        &format!("{HF_BASE}/{base_path}.onnx.json?download=true"),
        &config_path,
        cancellation,
    )
    .await?;
    Ok(model_path)
}

async fn fetch_voices_index(client: &Client, cancellation: &Cancellation) -> Result<Value> {
    let url = format!("{HF_BASE}/voices.json?download=true");
    cancellation
        .run(async {
            let response = client.get(url).send().await?;
            let status = response.status();
            if !status.is_success() {
                return Err(TtsError::provider_failed(
                    PROVIDER,
                    format!("voices index request failed: {status}"),
                ));
            }
            response.json::<Value>().await.map_err(TtsError::from)
        })
        .await
}

async fn download(
    client: &Client,
    url: &str,
    destination: &Path,
    cancellation: &Cancellation,
) -> Result<()> {
    let bytes = cancellation
        .run(async {
            let response = client.get(url).send().await?;
            let status = response.status();
            if !status.is_success() {
                return Err(TtsError::provider_failed(
                    PROVIDER,
                    format!("download failed: {status} {url}"),
                ));
            }
            let bytes = response.bytes().await?;
            Ok(bytes.to_vec())
        })
        .await?;
    cancellation
        .run(async {
            tokio::fs::write(destination, bytes)
                .await
                .map_err(TtsError::from)
        })
        .await
}

fn stream_piper(
    piper: &PiperCmd,
    model_path: &Path,
    text: &str,
    length_scale: f32,
    speaker: u32,
    cancellation: Cancellation,
) -> Result<impl futures_util::Stream<Item = Result<bytes::Bytes>> + Send + 'static> {
    let mut args = piper.args.clone();
    args.extend([
        "--model".to_owned(),
        model_path.display().to_string(),
        "--output_file".to_owned(),
        "-".to_owned(),
        "--length_scale".to_owned(),
        length_scale.to_string(),
        "--speaker".to_owned(),
        speaker.to_string(),
    ]);
    let env = piper
        .ld_library_path
        .as_deref()
        .map(|path| vec![("LD_LIBRARY_PATH".to_owned(), path.to_owned())])
        .unwrap_or_default();
    process::stream_output(
        piper.command.clone(),
        args,
        Some(text.as_bytes().to_vec()),
        env,
        cancellation,
    )
}

fn voice_base_path(voice: &str) -> Result<String> {
    let parts = voice.split('-').collect::<Vec<&str>>();
    if parts.len() < 3 {
        return Err(TtsError::provider_failed(
            PROVIDER,
            format!("invalid piper voice id: {voice}"),
        ));
    }
    let lang_code = parts[0];
    let lang = lang_code.split('_').next().unwrap_or(lang_code);
    let quality = parts[parts.len() - 1];
    let voice_name = parts[1..parts.len() - 1].join("-");
    Ok(format!("{lang}/{lang_code}/{voice_name}/{quality}/{voice}"))
}

fn venv_executable(cache_dir: &Path, name: &str) -> PathBuf {
    if cfg!(target_os = "windows") {
        cache_dir
            .join("venv")
            .join("Scripts")
            .join(format!("{name}.exe"))
    } else {
        cache_dir.join("venv").join("bin").join(name)
    }
}
